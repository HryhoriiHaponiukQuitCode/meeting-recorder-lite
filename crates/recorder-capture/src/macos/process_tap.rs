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
//! A tap can come up dead: everything is created without an error, but the
//! IOProc is never called (seen intermittently on macOS 26, a whole call
//! with an empty right channel). A working tap calls it every few
//! milliseconds even in silence, so [`Liveness`] treats [`DEAD_AFTER`]
//! without a call as dead and the tap is rebuilt. If that fails twice in a
//! row and ScreenCaptureKit is already allowed, capture switches to it.
//!
//! `AudioHardwareCreateProcessTap` / `AudioHardwareDestroyProcessTap` are
//! resolved at run time with `dlsym`, so the binary still launches on macOS
//! 13, where those symbols do not exist.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

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
    /// IOProc calls so far, for the liveness check.
    calls: Arc<AtomicU64>,
}

/// A tap whose IOProc has not been called for this long is dead.
pub(crate) const DEAD_AFTER: Duration = Duration::from_secs(3);
/// Dead taps in a row before switching to ScreenCaptureKit.
const DEAD_BEFORE_FALLBACK: u32 = 2;

/// Watches the IOProc call counter of one tap.
#[derive(Debug)]
pub(crate) struct Liveness {
    last_count: u64,
    last_change: Instant,
    /// The IOProc has been called at least once.
    pub(crate) seen: bool,
}

impl Liveness {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_count: 0,
            last_change: now,
            seen: false,
        }
    }

    /// `true` when the counter has not moved for [`DEAD_AFTER`].
    pub(crate) fn dead(&mut self, count: u64, now: Instant) -> bool {
        if count != self.last_count {
            self.last_count = count;
            self.last_change = now;
            self.seen = true;
            return false;
        }
        now.duration_since(self.last_change) >= DEAD_AFTER
    }
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
    ctx.calls.fetch_add(1, Ordering::Relaxed);
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
    calls: Arc<AtomicU64>,
    liveness: Liveness,
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
    let calls = Arc::new(AtomicU64::new(0));
    let mut tap = Tap {
        tap_id,
        aggregate_id: 0,
        proc_id: None,
        _ctx: Box::new(IoContext {
            producer: producer.clone(),
            rate: 48_000,
            mono: Vec::with_capacity(48_000),
            calls: calls.clone(),
        }),
        output_uid: output_uid.to_string(),
        // SAFETY: plain-old-data struct, overwritten just below.
        format: unsafe { std::mem::zeroed() },
        calls,
        liveness: Liveness::new(Instant::now()),
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
    tap.liveness = Liveness::new(Instant::now());
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
    // Taps in a row that never delivered audio.
    let mut dead_taps = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let output = default_output_device().and_then(device_uid);
        if let Some(tap) = &mut current {
            let count = tap.calls.load(Ordering::Relaxed);
            if tap.liveness.dead(count, Instant::now()) {
                let never = !tap.liveness.seen;
                dead_taps = if never { dead_taps + 1 } else { 1 };
                // Reported once per run of dead taps, so a long outage does
                // not flood the manifest; the log keeps every attempt.
                tracing::warn!(
                    never_called = never,
                    dead_taps,
                    "system audio tap delivers no audio, rebuilding"
                );
                if dead_taps == 1 {
                    let _ = events.send(CaptureEvent::Lost {
                        source: Source::System,
                        reason: "the system audio tap delivers no audio".into(),
                    });
                }
                current = None;
                if dead_taps >= DEAD_BEFORE_FALLBACK
                    && permissions::screen_capture_status() == Access::Granted
                {
                    tracing::warn!("switching system audio capture to ScreenCaptureKit");
                    super::sck::supervise(producer, events, stop);
                    return;
                }
                // Retry soon at first, then every 30 s at most.
                let wait = Duration::from_secs(u64::from(dead_taps.saturating_sub(1)) * 5)
                    .min(Duration::from_secs(30));
                let until = Instant::now() + wait;
                while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(250));
                }
                continue;
            } else if tap.liveness.seen {
                dead_taps = 0;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_without_calls_is_dead_after_three_seconds() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        assert!(!l.dead(0, t0 + Duration::from_millis(2900)));
        assert!(l.dead(0, t0 + DEAD_AFTER));
        assert!(!l.seen, "never called");
    }

    #[test]
    fn a_working_tap_that_stops_is_dead_three_seconds_later() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        // Called every 10 ms for a minute: alive.
        for i in 1..=6000u64 {
            assert!(!l.dead(i, t0 + Duration::from_millis(i * 10)));
        }
        assert!(l.seen);
        let stopped = t0 + Duration::from_secs(60);
        assert!(!l.dead(6000, stopped + Duration::from_secs(2)));
        assert!(l.dead(6000, stopped + DEAD_AFTER));
    }
}
