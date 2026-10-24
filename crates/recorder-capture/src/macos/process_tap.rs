//! System audio on macOS 14.4+ through a Core Audio process tap.
//!
//! The sequence follows minutiae (MIT, © 2026 Minutiae contributors,
//! `engine/Sources/EngineCore/Capture/SystemAudioTap.swift`, itself ported
//! from insidegui/AudioCap, BSD-2-Clause, © 2024 Guilherme Rambo; notices
//! in THIRD_PARTY_NOTICES.md): a global stereo tap that excludes our own
//! process → a private aggregate device anchored to the default output with
//! the tap (drift-compensated) as its input → an IOProc → start. When the
//! default output device or the tap format changes, the tap is rebuilt.
//!
//! `AudioHardwareCreateProcessTap` / `AudioHardwareDestroyProcessTap` are
//! resolved at run time with `dlsym`, so the binary still launches on macOS
//! 13, where those symbols do not exist.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectID, CATapDescription, CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey,
    kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectSystemObject, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use super::permissions::{self, Access};
use super::{default_output_device, device_uid, get_property, host_ticks_to_ns};
use crate::ring::{SharedProducer, push_shared};
use crate::{CaptureEvent, Source, SourceHandle, host_now_ns};

/// `'nope'`: Core Audio's status when the system-audio permission is missing.
const PERMISSION_DENIED: i32 = 0x6E6F_7065;

type CreateTapFn = unsafe extern "C-unwind" fn(*const AnyObject, *mut AudioObjectID) -> i32;
type DestroyTapFn = unsafe extern "C-unwind" fn(AudioObjectID) -> i32;

fn tap_api() -> Option<(CreateTapFn, DestroyTapFn)> {
    static API: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let (c, d) = (*API.get_or_init(|| {
        // SAFETY: dlsym in the already-loaded CoreAudio framework; null if absent.
        unsafe {
            let c = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareCreateProcessTap".as_ptr(),
            );
            let d = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareDestroyProcessTap".as_ptr(),
            );
            (!c.is_null() && !d.is_null()).then_some((c as usize, d as usize))
        }
    }))?;
    // SAFETY: the symbols have exactly these C signatures.
    unsafe {
        Some((
            std::mem::transmute::<usize, CreateTapFn>(c),
            std::mem::transmute::<usize, DestroyTapFn>(d),
        ))
    }
}

/// Process taps are used from macOS 14.4 (both reference implementations
/// require it; 14.2–14.3 behave differently with permissions).
pub fn supported() -> bool {
    super::macos_version() >= (14, 4) && tap_api().is_some()
}

struct IoContext {
    producer: SharedProducer,
    rate: u32,
    mono: Vec<f32>,
}

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the IoContext registered with this IOProc; Core
    // Audio calls the IOProc from one thread at a time.
    let ctx = unsafe { &mut *client.cast::<IoContext>() };
    unsafe { super::downmix_buffer_list(input.as_ptr(), &mut ctx.mono) };
    let (it, nw) = unsafe { (input_time.as_ref(), now.as_ref()) };
    let valid = |t: &AudioTimeStamp| {
        t.mFlags.contains(AudioTimeStampFlags::HostTimeValid) && t.mHostTime != 0
    };
    let host_ns = if valid(it) {
        host_ticks_to_ns(it.mHostTime)
    } else if valid(nw) {
        host_ticks_to_ns(nw.mHostTime)
            .saturating_sub(ctx.mono.len() as u64 * 1_000_000_000 / ctx.rate as u64)
    } else {
        host_now_ns().saturating_sub(ctx.mono.len() as u64 * 1_000_000_000 / ctx.rate as u64)
    };
    push_shared(&ctx.producer, host_ns, ctx.rate, &ctx.mono);
    0
}

/// A running tap + aggregate device + IOProc.
struct Tap {
    tap_id: AudioObjectID,
    aggregate_id: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    _ctx: Box<IoContext>,
    output_uid: String,
    format: AudioStreamBasicDescription,
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: tearing down objects this struct created, in reverse order.
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.aggregate_id, self.proc_id);
                AudioDeviceDestroyIOProcID(self.aggregate_id, self.proc_id);
            }
            if self.aggregate_id != 0 {
                AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            }
            if let Some((_, destroy)) = tap_api()
                && self.tap_id != 0
            {
                destroy(self.tap_id);
            }
        }
    }
}

fn own_process_object() -> Option<AudioObjectID> {
    let pid = std::process::id() as i32;
    get_property::<AudioObjectID>(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        Some(&pid.to_ne_bytes()),
    )
    .ok()
    .filter(|&o| o != 0)
}

fn key(k: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().expect("ASCII key"))
}

fn dict(
    entries: &[(&std::ffi::CStr, Retained<AnyObject>)],
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = entries.iter().map(|(k, _)| key(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<AnyObject>> = entries.iter().map(|(_, v)| v.clone()).collect();
    NSDictionary::from_retained_objects(&key_refs, &values)
}

fn obj<T: objc2::Message>(r: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { Retained::cast_unchecked(r) }
}

fn build(producer: &SharedProducer, output_uid: &str) -> Result<Tap, (String, bool)> {
    let (create, _) = tap_api().ok_or((
        "process taps are not available on this macOS".to_string(),
        false,
    ))?;
    let excluded: Vec<Retained<NSNumber>> = own_process_object()
        .map(NSNumber::new_u32)
        .into_iter()
        .collect();
    let excluded = NSArray::from_retained_slice(&excluded);
    // SAFETY: documented initialiser and setters of CATapDescription.
    let description = unsafe {
        let d = CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &excluded,
        );
        d.setUUID(&NSUUID::new());
        d.setPrivate(true);
        d.setMuteBehavior(CATapMuteBehavior::Unmuted);
        d.setName(&NSString::from_str("Meeting Recorder"));
        d
    };
    let tap_uuid = unsafe { description.UUID().UUIDString() };

    let mut tap_id: AudioObjectID = 0;
    let status = unsafe { create(Retained::as_ptr(&description).cast(), &mut tap_id) };
    if status == PERMISSION_DENIED {
        return Err(("system audio recording permission denied".into(), true));
    }
    if status != 0 || tap_id == 0 {
        return Err((
            format!("AudioHardwareCreateProcessTap failed: {status}"),
            false,
        ));
    }
    // From here on `Drop for Tap` releases whatever has been created.
    let mut tap = Tap {
        tap_id,
        aggregate_id: 0,
        proc_id: None,
        _ctx: Box::new(IoContext {
            producer: producer.clone(),
            rate: 48_000,
            mono: Vec::with_capacity(48_000),
        }),
        output_uid: output_uid.to_string(),
        // SAFETY: plain-old-data struct, overwritten just below.
        format: unsafe { std::mem::zeroed() },
    };
    tap.format = get_property::<AudioStreamBasicDescription>(tap_id, kAudioTapPropertyFormat, None)
        .map_err(|s| (format!("cannot read tap format: {s}"), false))?;
    tap._ctx.rate = tap.format.mSampleRate.round() as u32;

    let sub_device = dict(&[(kAudioSubDeviceUIDKey, obj(NSString::from_str(output_uid)))]);
    let sub_tap = dict(&[
        (kAudioSubTapUIDKey, obj(tap_uuid)),
        (
            kAudioSubTapDriftCompensationKey,
            obj(NSNumber::new_bool(true)),
        ),
    ]);
    let description = dict(&[
        (
            kAudioAggregateDeviceNameKey,
            obj(NSString::from_str("Meeting Recorder Tap")),
        ),
        (kAudioAggregateDeviceUIDKey, obj(NSUUID::new().UUIDString())),
        (
            kAudioAggregateDeviceMainSubDeviceKey,
            obj(NSString::from_str(output_uid)),
        ),
        (
            kAudioAggregateDeviceIsPrivateKey,
            obj(NSNumber::new_bool(true)),
        ),
        (
            kAudioAggregateDeviceIsStackedKey,
            obj(NSNumber::new_bool(false)),
        ),
        (
            kAudioAggregateDeviceTapAutoStartKey,
            obj(NSNumber::new_bool(true)),
        ),
        (
            kAudioAggregateDeviceSubDeviceListKey,
            obj(NSArray::from_retained_slice(&[sub_device])),
        ),
        (
            kAudioAggregateDeviceTapListKey,
            obj(NSArray::from_retained_slice(&[sub_tap])),
        ),
    ]);
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
    let cf: &CFDictionary = unsafe { &*Retained::as_ptr(&description).cast::<CFDictionary>() };
    let mut aggregate: AudioObjectID = 0;
    let status = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut aggregate)) };
    if status != 0 {
        return Err((
            format!("AudioHardwareCreateAggregateDevice failed: {status}"),
            false,
        ));
    }
    tap.aggregate_id = aggregate;

    let ctx_ptr: *mut IoContext = &mut *tap._ctx;
    let mut proc_id: AudioDeviceIOProcID = None;
    let status = unsafe {
        AudioDeviceCreateIOProcID(
            aggregate,
            Some(io_proc),
            ctx_ptr.cast(),
            NonNull::from(&mut proc_id),
        )
    };
    if status != 0 {
        return Err((format!("AudioDeviceCreateIOProcID failed: {status}"), false));
    }
    tap.proc_id = proc_id;
    let status = unsafe { AudioDeviceStart(aggregate, proc_id) };
    if status != 0 {
        return Err((format!("AudioDeviceStart failed: {status}"), false));
    }
    Ok(tap)
}

pub fn start(producer: SharedProducer, events: Sender<CaptureEvent>) -> SourceHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = thread::Builder::new()
        .name("tap-capture".into())
        .spawn(move || supervise(producer, events, stop2))
        .expect("spawn tap thread");
    SourceHandle::new(stop, thread)
}

fn supervise(producer: SharedProducer, events: Sender<CaptureEvent>, stop: Arc<AtomicBool>) {
    let fail = |error: String, denied: bool| {
        let _ = events.send(CaptureEvent::Failed {
            source: Source::System,
            error,
            permission_denied: denied,
        });
    };
    match permissions::system_audio_status() {
        Access::Granted => {}
        Access::Denied => fail("system audio recording permission denied".into(), true),
        Access::Unknown => {
            tracing::info!("requesting system audio recording permission");
            let answer = permissions::request_system_audio();
            tracing::info!(?answer, "system audio permission answer");
            if answer == Access::Denied {
                fail("system audio recording permission denied".into(), true);
            }
        }
    }

    let mut current: Option<Tap> = None;
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::SeqCst) {
        let output = default_output_device().and_then(device_uid);
        let needs_rebuild = match (&current, &output) {
            (None, _) => true,
            (Some(tap), Some(uid)) => {
                let fmt = get_property::<AudioStreamBasicDescription>(
                    tap.tap_id,
                    kAudioTapPropertyFormat,
                    None,
                )
                .ok();
                if tap.output_uid != *uid {
                    let _ = events.send(CaptureEvent::Lost {
                        source: Source::System,
                        reason: "default output device changed".into(),
                    });
                    true
                } else if fmt.is_some_and(|f| {
                    f.mSampleRate != tap.format.mSampleRate
                        || f.mChannelsPerFrame != tap.format.mChannelsPerFrame
                }) {
                    let _ = events.send(CaptureEvent::Lost {
                        source: Source::System,
                        reason: "tap format changed".into(),
                    });
                    true
                } else {
                    false
                }
            }
            (Some(_), None) => false,
        };
        if needs_rebuild {
            current = None; // tear the old tap down first
            match output {
                None => fail("no output device".into(), false),
                Some(uid) => match build(&producer, &uid) {
                    Ok(tap) => {
                        backoff = Duration::from_millis(200);
                        let name = super::get_string_property(
                            default_output_device().unwrap_or(0),
                            objc2_core_audio::kAudioObjectPropertyName,
                        )
                        .unwrap_or(uid);
                        let _ = events.send(CaptureEvent::Opened {
                            source: Source::System,
                            device: name,
                            rate: tap.format.mSampleRate.round() as u32,
                            channels: tap.format.mChannelsPerFrame as u16,
                        });
                        current = Some(tap);
                    }
                    Err((error, denied)) => {
                        fail(error, denied);
                        thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                        continue;
                    }
                },
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    drop(current);
}
