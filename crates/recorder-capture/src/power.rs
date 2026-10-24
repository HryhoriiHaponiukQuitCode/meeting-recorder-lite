//! Keeps the computer from idle-sleeping while a recording runs. Closing the
//! lid or choosing Sleep still works; the recorder handles that separately.

/// Holds the assertion until dropped. On Windows it must be dropped on the
/// thread that created it.
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    id: u32,
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2_foundation::NSString;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: *const NSString,
            level: u32,
            name: *const NSString,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    pub fn acquire(reason: &str) -> Option<u32> {
        let kind = NSString::from_str("PreventUserIdleSystemSleep");
        let name = NSString::from_str(reason);
        let mut id = 0u32;
        // SAFETY: CFString is toll-free bridged to NSString; 255 = kIOPMAssertionLevelOn.
        let rc = unsafe {
            IOPMAssertionCreateWithName(
                Retained::as_ptr(&kind),
                255,
                Retained::as_ptr(&name),
                &mut id,
            )
        };
        (rc == 0).then_some(id)
    }

    pub fn release(id: u32) {
        // SAFETY: releasing an assertion we created.
        unsafe { IOPMAssertionRelease(id) };
    }
}

impl KeepAwake {
    pub fn new(reason: &str) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            imp::acquire(reason).map(|id| Self { id })
        }
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{
                ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
            };
            let _ = reason;
            // SAFETY: plain Win32 call affecting only this thread's request.
            let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            (prev.0 != 0).then_some(Self {})
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = reason;
            None
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        imp::release(self.id);
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
            // SAFETY: clears this thread's execution-state request.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}
