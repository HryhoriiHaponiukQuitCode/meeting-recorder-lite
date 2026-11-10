//! The recordings in the recordings folder, newest first, for the list in
//! the window, and the names the user gives them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use recorder_core::manifest::{self, Manifest, Status};

use crate::transcription::{TranscriptState, transcript_state_of};

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub dir: PathBuf,
    pub started: DateTime<Utc>,
    /// Recorded audio (pauses and sleep not counted).
    pub seconds: u64,
    /// Still being recorded (by this or another app instance).
    pub recording: bool,
    /// The manifest lists audio, but none of the files is there (deleted
    /// or moved by hand).
    pub audio_missing: bool,
    pub transcript: TranscriptState,
    /// The name the user gave it ([`set_title`]); otherwise it is shown by
    /// its date.
    pub title: Option<String>,
}

impl SessionInfo {
    fn from_manifest(dir: PathBuf, m: &Manifest) -> Self {
        Self {
            started: m.started_at_utc,
            seconds: m.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000,
            recording: m.status == Status::Recording,
            audio_missing: !m.parts.is_empty()
                && !m.parts.iter().any(|p| dir.join(&p.file).is_file()),
            transcript: transcript_state_of(m, &dir),
            title: title(&dir),
            dir,
        }
    }

    /// Has audio and is closed, so it can be transcribed.
    pub fn transcribable(&self) -> bool {
        !self.recording && self.seconds > 0 && !self.audio_missing
    }
}

/// Every folder in `root` with a readable `manifest.json`, newest first.
/// The user's name for a recording, one line in the recording's folder. Not
/// in `manifest.json`: the recorder and the transcriber rewrite the manifest
/// while they work and would drop a name given meanwhile. The folder keeps
/// its name (the start time), which orders the list.
pub const TITLE_FILE: &str = "title.txt";
/// Longer names are cut.
pub const TITLE_MAX_CHARS: usize = 120;

/// One line, spaces collapsed, at most [`TITLE_MAX_CHARS`].
pub fn clean_title(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The recording's name, if it was given one.
pub fn title(dir: &Path) -> Option<String> {
    let t = clean_title(&fs::read_to_string(dir.join(TITLE_FILE)).ok()?);
    (!t.is_empty()).then_some(t)
}

/// Names the recording in `dir`; an empty name removes the name (the date
/// shows again).
pub fn set_title(dir: &Path, name: &str) -> io::Result<()> {
    if !dir.join(manifest::FILE_NAME).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the recording is not there any more",
        ));
    }
    let name = clean_title(name);
    let path = dir.join(TITLE_FILE);
    if name.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    }
    let tmp = dir.join(format!("{TITLE_FILE}.tmp"));
    fs::write(&tmp, format!("{name}\n"))?;
    fs::rename(&tmp, &path)
}

pub fn list(root: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<SessionInfo> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|dir| {
            let m = Manifest::load(&dir).ok()?;
            Some(SessionInfo::from_manifest(dir, &m))
        })
        .collect();
    out.sort_by(|a, b| b.started.cmp(&a.started).then(b.dir.cmp(&a.dir)));
    out
}

/// One page of recordings, newest first, and how many there are. Only the
/// page's manifests are read: session folders are named after their start
/// time (`2026-10-30_14-00-54_<id>`), so the folder names give the order.
pub fn page(root: &Path, offset: usize, limit: usize) -> (Vec<SessionInfo>, usize) {
    let dirs = folders(root);
    let total = dirs.len();
    let page = dirs
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|dir| info(&dir))
        .collect();
    (page, total)
}

/// One recording, from its folder.
pub fn info(dir: &Path) -> Option<SessionInfo> {
    let m = Manifest::load(dir).ok()?;
    Some(SessionInfo::from_manifest(dir.to_path_buf(), &m))
}

/// Session folders, newest first by name (named after their start time).
fn folders(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(recorder_core::manifest::FILE_NAME).is_file())
        .collect();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    dirs
}

/// The newest closed recording with audio (reads manifests only until it
/// is found).
pub fn latest(root: &Path) -> Option<SessionInfo> {
    folders(root)
        .into_iter()
        .filter_map(|d| info(&d))
        .find(SessionInfo::transcribable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_core::manifest::{OsInfo, PartInfo, PartStatus};
    use uuid::Uuid;

    fn session(root: &Path, name: &str, minutes_ago: i64, status: Status, ms: u64) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = Manifest::new(
            Uuid::new_v4(),
            Utc::now() - chrono::Duration::minutes(minutes_ago),
            OsInfo {
                family: "t".into(),
                version: "0".into(),
                arch: "x".into(),
            },
            32_000,
            0,
        );
        m.status = status;
        if ms > 0 {
            m.parts.push(PartInfo {
                file: "part_0001.ogg".into(),
                start_offset_ms: 0,
                started_at_utc: None,
                duration_ms: ms,
                status: PartStatus::Closed,
            });
        }
        m.save(&dir).unwrap();
    }

    #[test]
    fn a_recording_keeps_the_name_it_was_given() {
        let root = std::env::temp_dir().join(format!("mr-titles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        session(&root, "call", 5, Status::Completed, 60_000);
        let dir = root.join("call");
        assert_eq!(info(&dir).unwrap().title, None);
        set_title(&dir, "  Планування\n спринту  ").unwrap();
        assert_eq!(title(&dir).as_deref(), Some("Планування спринту"));
        assert_eq!(info(&dir).unwrap().title.as_deref(), Some("Планування спринту"));
        assert_eq!(list(&root)[0].title.as_deref(), Some("Планування спринту"));
        // The manifest is rewritten (the transcriber does that): the name stays.
        Manifest::load(&dir).unwrap().save(&dir).unwrap();
        assert_eq!(title(&dir).as_deref(), Some("Планування спринту"));
        // Long names are cut; an empty one brings the date back.
        set_title(&dir, &"я".repeat(500)).unwrap();
        assert_eq!(title(&dir).unwrap().chars().count(), TITLE_MAX_CHARS);
        set_title(&dir, "   ").unwrap();
        assert_eq!(title(&dir), None);
        assert!(!dir.join(TITLE_FILE).exists());
        set_title(&dir, "").unwrap();
        // Not a recording: refused.
        assert!(set_title(&root.join("missing"), "x").is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn newest_first_and_latest_skips_open_or_empty_ones() {
        let root = std::env::temp_dir().join(format!("mr-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        session(&root, "old", 60, Status::Completed, 90_000);
        session(&root, "crashed", 30, Status::CrashedRecovered, 5_000);
        session(&root, "empty", 20, Status::Completed, 0);
        session(&root, "now", 1, Status::Recording, 4_000);
        std::fs::create_dir_all(root.join("not-a-session")).unwrap();

        // Audio files that exist, except for "old".
        for name in ["crashed", "now"] {
            std::fs::write(root.join(name).join("part_0001.ogg"), b"x").unwrap();
        }
        let all = list(&root);
        let names: Vec<_> = all
            .iter()
            .map(|s| s.dir.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["now", "empty", "crashed", "old"]);
        assert!(all[0].recording && !all[0].transcribable());
        assert!(!all[1].transcribable());
        assert_eq!(all[3].seconds, 90);
        assert_eq!(all[3].transcript, TranscriptState::None);
        assert!(
            all[3].audio_missing && !all[3].transcribable(),
            "its file was deleted"
        );
        assert!(!all[2].audio_missing && all[2].transcribable());
        assert_eq!(latest(&root).unwrap().dir, root.join("crashed"));
        assert!(list(&root.join("missing")).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn pages_follow_the_folder_names_and_read_only_the_page() {
        let root = std::env::temp_dir().join(format!("mr-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (i, name) in [
            "2026-10-29_09-00-00_a",
            "2026-10-30_14-00-54_b",
            "2026-10-30_16-06-36_c",
            "2026-10-25_00-03-42_d",
            "2026-10-30_01-05-11_e",
        ]
        .iter()
        .enumerate()
        {
            session(&root, name, 10 * i as i64, Status::Completed, 1_000);
        }
        std::fs::create_dir_all(root.join("not-a-session")).unwrap();
        let names = |v: Vec<SessionInfo>| -> Vec<String> {
            v.iter()
                .map(|s| s.dir.file_name().unwrap().to_string_lossy()[..19].to_string())
                .collect()
        };
        let (first, total) = page(&root, 0, 2);
        assert_eq!(total, 5);
        assert_eq!(names(first), ["2026-10-30_16-06-36", "2026-10-30_14-00-54"]);
        let (last, _) = page(&root, 4, 2);
        assert_eq!(names(last), ["2026-10-25_00-03-42"]);
        assert!(page(&root, 6, 2).0.is_empty());
        assert_eq!(page(&root.join("missing"), 0, 2).1, 0);
        std::fs::remove_dir_all(root).ok();
    }
}
