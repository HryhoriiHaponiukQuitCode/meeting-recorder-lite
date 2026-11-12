//! User settings (`settings.json` in the config directory). Missing or
//! unreadable files fall back to defaults; unknown fields are ignored.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::recorder::AecMode;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The consent reminder was dismissed with "don't show again".
    pub consent_reminder_dismissed: bool,
    pub bitrate_kbps: u32,
    pub aec: AecMode,
    /// `None` = Documents/Meeting Recorder/recordings.
    pub recordings_dir: Option<PathBuf>,
    /// Microphone name substring; `None` = system default.
    pub mic_device: Option<String>,
    /// Transcribe each recording once it is saved (when the model is there).
    pub transcribe_automatically: bool,
    /// Recognizer threads: 2 = gentle (default), 4 = fast.
    pub transcription_threads: u32,
    /// Best (Whisper + Parakeet, macOS) or Fast (Parakeet).
    pub transcription_quality: recorder_transcribe::model::Quality,
    /// Tell the voices of the system audio apart (Others 1, 2, …).
    pub tell_voices_apart: bool,
    /// `None` = the default models folder.
    pub models_dir: Option<PathBuf>,
    /// The user's name, shown for their microphone in transcripts (empty:
    /// "Participant N").
    pub your_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            consent_reminder_dismissed: false,
            bitrate_kbps: 32,
            aec: AecMode::Auto,
            recordings_dir: None,
            mic_device: None,
            transcribe_automatically: true,
            transcription_threads: recorder_transcribe::DEFAULT_THREADS,
            transcription_quality: recorder_transcribe::model::Quality::default_for_this_system(),
            tell_voices_apart: true,
            models_dir: None,
            your_name: String::new(),
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        crate::paths::config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn recordings_dir(&self) -> PathBuf {
        self.recordings_dir
            .clone()
            .unwrap_or_else(crate::paths::default_recordings_dir)
    }

    pub fn models_dir(&self) -> PathBuf {
        self.models_dir
            .clone()
            .unwrap_or_else(crate::paths::default_models_dir)
    }

    /// The chosen quality if this system can run it, else the default.
    pub fn transcription_quality(&self) -> recorder_transcribe::model::Quality {
        if self.transcription_quality.is_available() {
            self.transcription_quality
        } else {
            recorder_transcribe::model::Quality::default_for_this_system()
        }
    }

    /// Gentle (the default) rather than Fast.
    pub fn gentle_transcription(&self) -> bool {
        self.transcription_threads < 4
    }

    /// Gentle on a Mac: the transcriber runs as background work, only on
    /// the efficiency cores (see `meeting-recorder-transcribe
    /// --background`).
    pub fn transcription_in_background(&self) -> bool {
        cfg!(target_os = "macos") && self.gentle_transcription()
    }

    /// Threads within 1..=available cores. In the background 4: on the
    /// efficiency cores they are faster than 2 (111 s against 155 s for 6
    /// minutes of speech) and still cool.
    pub fn transcription_threads(&self) -> u32 {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
        let wanted = if self.transcription_in_background() {
            4
        } else {
            self.transcription_threads
        };
        wanted.clamp(1, cores.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gentle_runs_in_the_background_on_a_mac_and_fast_does_not() {
        let gentle = Settings::default();
        assert!(gentle.gentle_transcription());
        assert_eq!(
            gentle.transcription_in_background(),
            cfg!(target_os = "macos")
        );
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
        let expected = if cfg!(target_os = "macos") { 4 } else { 2 };
        assert_eq!(gentle.transcription_threads(), expected.min(cores));

        let fast = Settings {
            transcription_threads: 4,
            ..Settings::default()
        };
        assert!(!fast.gentle_transcription());
        assert!(!fast.transcription_in_background());
        assert_eq!(fast.transcription_threads(), 4.min(cores));
    }
}
