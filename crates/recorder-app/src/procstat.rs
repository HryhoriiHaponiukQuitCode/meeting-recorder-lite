//! Own-process CPU time and memory, for the per-minute log line that checks
//! the CPU ≤ 3 % and RAM ≤ 80 MB acceptance criteria.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcStat {
    /// User + system CPU time consumed so far.
    pub cpu: Duration,
    /// Current resident set / working set, bytes (peak on macOS).
    pub rss_bytes: u64,
}

#[cfg(unix)]
pub fn sample() -> ProcStat {
    // SAFETY: getrusage only writes into the zero-initialised struct.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    // ru_maxrss is bytes on macOS, kilobytes on Linux.
    let rss = if cfg!(target_os = "macos") {
        ru.ru_maxrss as u64
    } else {
        ru.ru_maxrss as u64 * 1024
    };
    ProcStat {
        cpu: tv(ru.ru_utime) + tv(ru.ru_stime),
        rss_bytes: rss,
    }
}

#[cfg(windows)]
pub fn sample() -> ProcStat {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ft = |f: FILETIME| {
        Duration::from_nanos((((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) * 100)
    };
    let mut out = ProcStat::default();
    // SAFETY: the pseudo-handle of the current process is always valid; the
    // calls only write into local structs.
    unsafe {
        let p = GetCurrentProcess();
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(p, &mut c, &mut e, &mut k, &mut u).is_ok() {
            out.cpu = ft(k) + ft(u);
        }
        let mut mem = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(p, &mut mem, mem.cb).is_ok() {
            out.rss_bytes = mem.WorkingSetSize as u64;
        }
    }
    out
}
