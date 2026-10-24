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
pub fn host_now_ns() -> u64 {
    #[repr(C)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
    }
    let mut tb = TimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: plain libSystem calls writing into a local struct.
    let ticks = unsafe {
        mach_timebase_info(&mut tb);
        mach_absolute_time()
    };
    (ticks as u128 * tb.numer as u128 / tb.denom.max(1) as u128) as u64
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn host_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}
