//! The host monotonic clock shared by every capture source and the engine.
//! It must be the same clock the audio APIs stamp their buffers with:
//! `QueryPerformanceCounter` on Windows (WASAPI QPC positions, cpal WASAPI
//! timestamps) and `mach_absolute_time` on macOS (Core Audio `mHostTime`).

/// Current host time in nanoseconds.
#[cfg(windows)]
pub fn host_now_ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: both calls only write to the provided integers and cannot fail on XP+.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (count as u128 * 1_000_000_000 / freq.max(1) as u128) as u64
}

#[cfg(target_os = "macos")]
mod mach {
    #[repr(C)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
    }

    pub fn now_ticks() -> u64 {
        // SAFETY: plain libSystem call.
        unsafe { mach_absolute_time() }
    }

    /// Converts `mach_absolute_time` ticks (Core Audio `mHostTime`) to nanoseconds.
    pub fn ticks_to_ns(ticks: u64) -> u64 {
        static TB: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        let (numer, denom) = *TB.get_or_init(|| {
            let mut tb = TimebaseInfo { numer: 1, denom: 1 };
            // SAFETY: writes into a local struct.
            unsafe { mach_timebase_info(&mut tb) };
            (tb.numer, tb.denom.max(1))
        });
        (ticks as u128 * numer as u128 / denom as u128) as u64
    }
}

#[cfg(target_os = "macos")]
pub(crate) use mach::ticks_to_ns;

#[cfg(target_os = "macos")]
pub fn host_now_ns() -> u64 {
    mach::ticks_to_ns(mach::now_ticks())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn host_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}
