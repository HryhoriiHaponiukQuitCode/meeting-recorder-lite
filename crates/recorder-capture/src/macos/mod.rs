//! System-audio capture on macOS.
//!
//! * macOS 14.4+: Core Audio process tap ([`process_tap`]). Needs the
//!   "System Audio Recording" permission (`NSAudioCaptureUsageDescription`),
//!   not screen recording.
//! * macOS 13.0–14.3: ScreenCaptureKit, audio only ([`sck`]). Needs the
//!   "Screen Recording" permission.

pub mod permissions;
pub mod process_tap;
pub mod sck;

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use objc2::rc::Retained;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_foundation::NSString;

use crate::ring::SharedProducer;
use crate::{CaptureEvent, SourceHandle};

/// Which system-audio backend to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemCapture {
    /// Process tap where supported, ScreenCaptureKit otherwise.
    #[default]
    Auto,
    ProcessTap,
    ScreenCaptureKit,
}

/// Starts system-audio capture and returns the method used for the manifest.
pub fn start_system(
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
    mode: SystemCapture,
) -> (SourceHandle, &'static str) {
    let use_tap = match mode {
        SystemCapture::Auto => process_tap::supported(),
        SystemCapture::ProcessTap => true,
        SystemCapture::ScreenCaptureKit => false,
    };
    tracing::info!(macos = %macos_version_string(), use_tap, "selecting system audio capture");
    if use_tap {
        (
            process_tap::start(producer, events),
            "coreaudio_process_tap",
        )
    } else {
        (sck::start(producer, events), "screencapturekit")
    }
}

/// `(major, minor)` of the running macOS.
pub fn macos_version() -> (u32, u32) {
    static V: OnceLock<(u32, u32)> = OnceLock::new();
    *V.get_or_init(|| {
        let s = macos_version_string();
        let mut it = s.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        (it.next().unwrap_or(0), it.next().unwrap_or(0))
    })
}

pub fn macos_version_string() -> String {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: sysctlbyname writes at most `len` bytes into `buf`.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return "0.0".into();
    }
    String::from_utf8_lossy(&buf[..len.saturating_sub(1)]).into_owned()
}

pub(crate) use crate::clock::ticks_to_ns as host_ticks_to_ns;

fn global_address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a plain-data property.
pub(crate) fn get_property<T: Copy>(
    object: AudioObjectID,
    selector: u32,
    qualifier: Option<&[u8]>,
) -> Result<T, i32> {
    let mut address = global_address(selector);
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let (q_size, q_ptr) = qualifier.map_or((0, std::ptr::null()), |q| {
        (q.len() as u32, q.as_ptr().cast())
    });
    // SAFETY: `value` has room for `size` bytes; the address outlives the call.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut address),
            q_size,
            q_ptr,
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: the HAL filled the value.
    Ok(unsafe { value.assume_init() })
}

/// Reads a CFString property (the HAL returns a +1 reference).
pub(crate) fn get_string_property(object: AudioObjectID, selector: u32) -> Option<String> {
    let ptr: *mut NSString = get_property(object, selector, None).ok()?;
    // SAFETY: CFString is toll-free bridged to NSString and we own the reference.
    let s = unsafe { Retained::from_raw(ptr) }?;
    Some(s.to_string())
}

pub(crate) fn default_output_device() -> Option<AudioObjectID> {
    get_property::<AudioObjectID>(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDefaultOutputDevice,
        None,
    )
    .ok()
    .filter(|&id| id != 0)
}

pub(crate) fn device_uid(device: AudioObjectID) -> Option<String> {
    get_string_property(device, kAudioDevicePropertyDeviceUID)
}

/// Mixes an `AudioBufferList` of float32 samples (interleaved or not) to mono.
///
/// # Safety
/// `abl` must point to a valid buffer list whose buffers hold float32 data.
pub(crate) unsafe fn downmix_buffer_list(abl: *const AudioBufferList, out: &mut Vec<f32>) {
    out.clear();
    // SAFETY: guaranteed by the caller.
    let (n, first) = unsafe {
        (
            (*abl).mNumberBuffers as usize,
            (&raw const (*abl).mBuffers).cast::<AudioBuffer>(),
        )
    };
    if n == 0 {
        return;
    }
    // SAFETY: an AudioBufferList stores `mNumberBuffers` buffers contiguously.
    let buffers = unsafe { std::slice::from_raw_parts(first, n) };
    let slices: Vec<&[f32]> = buffers
        .iter()
        .filter(|b| !b.mData.is_null())
        .map(|b| unsafe {
            std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        })
        .collect();
    if slices.is_empty() {
        return;
    }
    let channels_in_first = buffers[0].mNumberChannels.max(1) as usize;
    if slices.len() == 1 && channels_in_first > 1 {
        let ch = channels_in_first;
        let scale = 1.0 / ch as f32;
        for frame in slices[0].chunks_exact(ch) {
            out.push(frame.iter().sum::<f32>() * scale);
        }
    } else {
        let frames = slices.iter().map(|s| s.len()).min().unwrap_or(0);
        let scale = 1.0 / slices.len() as f32;
        for i in 0..frames {
            out.push(slices.iter().map(|s| s[i]).sum::<f32>() * scale);
        }
    }
}
