//! Application layer shared by the console and tray front ends: runs a
//! recording session (capture → engine → disk) on its own thread, reports
//! status and user-facing notices, and owns paths, settings and logging.

pub mod logging;
mod monitor;
pub mod paths;
pub mod permissions;
pub mod procstat;
mod recorder;
pub mod settings;
pub mod transcription;

pub use monitor::{Monitor, MonitorStatus};
pub use recorder::{AecMode, Notice, Recorder, RecorderOptions, Status, SystemOption};
pub use recorder_capture::Source;
pub use recorder_core::manifest::Manifest;
pub use recorder_core::recovery::{Recovered, recover_all};

/// Name used for folders, logs and the app bundle.
pub const APP_NAME: &str = "Meeting Recorder";

/// Text shown before recording starts (spec: legal reminder).
pub const CONSENT_REMINDER: &str = "You are responsible for getting consent from everyone in the call \
before recording, as the law requires (GDPR, local laws).";
