//! Where recordings, logs and settings live.

use std::path::PathBuf;

use crate::APP_NAME;

/// `Documents/Meeting Recorder/recordings`.
pub fn default_recordings_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("recordings")
}

/// `~/Library/Logs/Meeting Recorder` or `%LOCALAPPDATA%\Meeting Recorder\logs`.
pub fn log_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .unwrap_or_default()
            .join("Library/Logs")
            .join(APP_NAME)
    } else {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME)
            .join("logs")
    }
}

/// `~/Library/Application Support/Meeting Recorder` or `%APPDATA%\Meeting Recorder`.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

/// Speech model: `~/Library/Application Support/Meeting Recorder/models` or
/// `%LOCALAPPDATA%\Meeting Recorder\models` (local, not roaming: 640 MiB).
pub fn default_models_dir() -> PathBuf {
    let base = if cfg!(windows) {
        dirs::data_local_dir()
    } else {
        dirs::config_dir()
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join("models")
}

/// Opens a file with its default app (a transcript in the text editor).
pub fn open_file(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Opens a folder in Finder / Explorer.
pub fn open_in_file_manager(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
