//! Rests between pieces of work when the computer runs hot (policy in
//! `recorder_transcribe::heat`).
//!
//! The thermal pressure comes from the system notification
//! `com.apple.system.thermalpressurelevel` (libnotify, part of libSystem;
//! the same source as `ProcessInfo.thermalState`). Other systems report
//! Normal. For checks, `MR_THERMAL_OVERRIDE` may name a file holding the
//! pressure level (0–4), read on every check.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use recorder_transcribe::heat::{CHECK_EVERY, HOT_CHECK, Heat};

use crate::pipeline::Cancelled;

/// Called when the level changes (the window shows it).
pub type HeatReport = Arc<dyn Fn(Heat) + Send + Sync>;

pub struct Pacer {
    source: Source,
    heat: Heat,
    checked: Option<Instant>,
    work_since: Instant,
    report: Option<HeatReport>,
}

enum Source {
    #[cfg(target_os = "macos")]
    Notify(i32),
    Override(std::path::PathBuf),
    None,
}

#[cfg(target_os = "macos")]
mod notify {
    use std::ffi::{c_char, c_int};

    unsafe extern "C" {
        pub fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
        pub fn notify_get_state(token: c_int, state: *mut u64) -> u32;
    }

    pub const NAME: &std::ffi::CStr = c"com.apple.system.thermalpressurelevel";
}

impl Pacer {
    pub fn new(report: Option<HeatReport>) -> Self {
        let source = if let Some(p) = std::env::var_os("MR_THERMAL_OVERRIDE") {
            Source::Override(p.into())
        } else {
            Self::system()
        };
        Self {
            source,
            heat: Heat::Normal,
            checked: None,
            work_since: Instant::now(),
            report,
        }
    }

    #[cfg(target_os = "macos")]
    fn system() -> Source {
        let mut token = 0;
        // SAFETY: a static C string and an out-pointer to a local.
        let status = unsafe { notify::notify_register_check(notify::NAME.as_ptr(), &mut token) };
        if status == 0 {
            Source::Notify(token)
        } else {
            eprintln!("thermal state not available (notify status {status})");
            Source::None
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn system() -> Source {
        Source::None
    }

    fn read(&self) -> Heat {
        let level = match &self.source {
            #[cfg(target_os = "macos")]
            Source::Notify(token) => {
                let mut state = 0u64;
                // SAFETY: a token from notify_register_check and an
                // out-pointer to a local.
                let status = unsafe { notify::notify_get_state(*token, &mut state) };
                if status == 0 { state } else { 0 }
            }
            Source::Override(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
            Source::None => 0,
        };
        Heat::from_pressure(level)
    }

    fn set(&mut self, heat: Heat) {
        if heat != self.heat {
            eprintln!("thermal state: {heat:?}");
            self.heat = heat;
            if let Some(r) = &self.report {
                r(heat);
            }
        }
    }

    pub fn heat(&self) -> Heat {
        self.heat
    }

    /// Call between pieces of work: rests when warm, waits when hot. The
    /// time since the previous call counts as work.
    pub fn tick(&mut self, stop: &AtomicBool) -> Result<()> {
        let now = Instant::now();
        if self.checked.is_none_or(|t| now - t >= CHECK_EVERY) {
            self.checked = Some(now);
            let heat = self.read();
            self.set(heat);
        }
        let work = now - self.work_since;
        let rest = self.heat.rest_after(work);
        if !rest.is_zero() {
            sleep(rest, stop)?;
        }
        while self.heat == Heat::Hot {
            sleep(HOT_CHECK, stop)?;
            let heat = self.read();
            self.set(heat);
            self.checked = Some(Instant::now());
        }
        self.work_since = Instant::now();
        Ok(())
    }
}

/// Sleeps in short steps so that Stop still works at once.
fn sleep(d: Duration, stop: &AtomicBool) -> Result<()> {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if stop.load(Ordering::SeqCst) {
            return Err(Cancelled.into());
        }
        std::thread::sleep((until - Instant::now()).min(Duration::from_millis(200)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_state_can_be_read() {
        let p = Pacer::new(None);
        assert!(matches!(p.source, Source::Notify(_) | Source::Override(_)));
        // Whatever it is now, reading works and gives a level.
        let _ = p.read();
    }

    #[test]
    fn warm_rests_and_hot_waits_until_cooler() {
        let dir = std::env::temp_dir().join(format!("mr-heat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("level");
        std::fs::write(&file, "2").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let mut p = Pacer {
            source: Source::Override(file.clone()),
            heat: Heat::Normal,
            checked: None,
            work_since: Instant::now(),
            report: Some(Arc::new(move |h| s2.lock().unwrap().push(h))),
        };
        let stop = AtomicBool::new(false);
        // Warm: 100 ms of work, then about 100 ms of rest.
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        p.tick(&stop).unwrap();
        assert!(
            t.elapsed() >= Duration::from_millis(90),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(p.heat(), Heat::Warm);
        // Hot: waits until the level drops (another thread cools it down).
        std::fs::write(&file, "3").unwrap();
        p.checked = None;
        let f2 = file.clone();
        let cooler = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(f2, "1").unwrap();
        });
        let t = Instant::now();
        p.tick(&stop).unwrap();
        cooler.join().unwrap();
        assert!(t.elapsed() >= HOT_CHECK, "waited {:?}", t.elapsed());
        assert_eq!(p.heat(), Heat::Normal);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Heat::Warm, Heat::Hot, Heat::Normal]
        );
        // Stop ends a wait at once.
        std::fs::write(&file, "4").unwrap();
        p.checked = None;
        stop.store(true, Ordering::SeqCst);
        assert!(p.tick(&stop).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
