//! Privacy permissions on macOS.
//!
//! * Microphone: `AVCaptureDevice` authorisation. When it is denied, Core
//!   Audio delivers digital silence instead of failing, so it must be checked
//!   explicitly.
//! * System audio (process tap): the private TCC.framework service
//!   `kTCCServiceAudioCapture`, the same approach as AudioCap and minutiae.
//!   Only used to ask and to report; the tap itself also fails with `'nope'`.
//! * Screen recording (ScreenCaptureKit fallback): CoreGraphics preflight.

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Granted,
    Denied,
    /// Not asked yet, or the status cannot be determined.
    Unknown,
}

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Microphone permission; asks the user (and waits up to two minutes) when
/// it has not been decided yet.
pub fn ensure_microphone() -> Access {
    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
        return Access::Unknown;
    };
    let media = NSString::from_str("soun"); // AVMediaTypeAudio
    // SAFETY: documented class method returning AVAuthorizationStatus (NSInteger).
    let status: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: &*media] };
    match status {
        3 => Access::Granted,
        1 | 2 => Access::Denied,
        _ => {
            let (tx, rx) = mpsc::channel();
            let block = RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            // SAFETY: documented class method; the block is copied by the callee.
            let _: () = unsafe {
                msg_send![cls, requestAccessForMediaType: &*media, completionHandler: &*block]
            };
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => Access::Granted,
                Ok(false) => Access::Denied,
                Err(_) => Access::Unknown,
            }
        }
    }
}

type PreflightFn = unsafe extern "C" fn(*const AnyObject, *const c_void) -> i64;
type RequestFn =
    unsafe extern "C" fn(*const AnyObject, *const c_void, *const block2::DynBlock<dyn Fn(Bool)>);

fn tcc_symbol(name: &std::ffi::CStr) -> Option<*mut c_void> {
    // SAFETY: dlopen/dlsym on a system framework path; failure returns null.
    unsafe {
        let handle = libc::dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(),
            libc::RTLD_NOW,
        );
        if handle.is_null() {
            return None;
        }
        let sym = libc::dlsym(handle, name.as_ptr());
        (!sym.is_null()).then_some(sym)
    }
}

/// "System Audio Recording" permission used by process taps.
pub fn system_audio_status() -> Access {
    let Some(sym) = tcc_symbol(c"TCCAccessPreflight") else {
        return Access::Unknown;
    };
    // SAFETY: TCCAccessPreflight(CFStringRef service, CFDictionaryRef options) -> int.
    let preflight: PreflightFn = unsafe { std::mem::transmute(sym) };
    let service = NSString::from_str("kTCCServiceAudioCapture");
    let r = unsafe { preflight(Retained::as_ptr(&service).cast(), std::ptr::null()) };
    match r {
        0 => Access::Granted,
        1 => Access::Denied,
        _ => Access::Unknown,
    }
}

/// Shows the system prompt for system-audio recording and waits for the answer.
pub fn request_system_audio() -> Access {
    let Some(sym) = tcc_symbol(c"TCCAccessRequest") else {
        return Access::Unknown;
    };
    // SAFETY: TCCAccessRequest(CFStringRef, CFDictionaryRef, void (^)(BOOL)).
    let request: RequestFn = unsafe { std::mem::transmute(sym) };
    let service = NSString::from_str("kTCCServiceAudioCapture");
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe { request(Retained::as_ptr(&service).cast(), std::ptr::null(), &*block) };
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(true) => Access::Granted,
        Ok(false) => Access::Denied,
        Err(_) => Access::Unknown,
    }
}

/// Screen-recording permission (ScreenCaptureKit). Asks once if needed.
pub fn ensure_screen_capture() -> Access {
    // SAFETY: plain CoreGraphics calls (macOS 10.15+).
    unsafe {
        if CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() {
            Access::Granted
        } else {
            Access::Denied
        }
    }
}

/// Opens the right pane of System Settings.
pub fn open_privacy_settings(pane: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    let _ = std::process::Command::new("open").arg(url).spawn();
}
