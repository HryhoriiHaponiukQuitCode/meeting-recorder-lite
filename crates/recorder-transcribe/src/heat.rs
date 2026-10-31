//! Keeping the computer cool: how much the transcriber rests at each level
//! of the system's thermal pressure.
//!
//! macOS reports the pressure as nominal, moderate, heavy, trapping or
//! sleeping (`ProcessInfo.thermalState`: nominal, fair, serious, critical).
//!
//! - **Normal** (nominal, moderate): full speed; the fans handle it.
//! - **Warm** (heavy): rest as long as the last stretch of work took, so the
//!   transcriber uses at most half of the time it would.
//! - **Hot** (trapping and above): stop and wait until it is no longer hot,
//!   checking every [`HOT_CHECK`].
//!
//! The transcript does not change; it only takes longer.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Heat {
    #[default]
    Normal,
    /// Taking breaks.
    Warm,
    /// Waiting for the computer to cool down.
    Hot,
}

/// How often the thermal state is read while working.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);
/// How often it is read while waiting in [`Heat::Hot`].
pub const HOT_CHECK: Duration = Duration::from_secs(5);
/// The longest single rest in [`Heat::Warm`].
pub const MAX_REST: Duration = Duration::from_secs(30);

impl Heat {
    /// From the macOS thermal pressure level (`OSThermalPressureLevel`:
    /// 0 nominal, 1 moderate, 2 heavy, 3 trapping, 4 sleeping).
    pub fn from_pressure(level: u64) -> Heat {
        match level {
            0 | 1 => Heat::Normal,
            2 => Heat::Warm,
            _ => Heat::Hot,
        }
    }

    /// The rest after `work` of busy time at this level (Hot waits instead).
    pub fn rest_after(self, work: Duration) -> Duration {
        match self {
            Heat::Warm => work.min(MAX_REST),
            Heat::Normal | Heat::Hot => Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_rests() {
        assert_eq!(Heat::from_pressure(0), Heat::Normal);
        assert_eq!(Heat::from_pressure(1), Heat::Normal);
        assert_eq!(Heat::from_pressure(2), Heat::Warm);
        assert_eq!(Heat::from_pressure(3), Heat::Hot);
        assert_eq!(Heat::from_pressure(4), Heat::Hot);
        let s = Duration::from_secs;
        assert_eq!(Heat::Normal.rest_after(s(3)), Duration::ZERO);
        assert_eq!(Heat::Warm.rest_after(s(3)), s(3));
        assert_eq!(Heat::Warm.rest_after(s(300)), MAX_REST);
        assert_eq!(Heat::Hot.rest_after(s(3)), Duration::ZERO);
    }
}
