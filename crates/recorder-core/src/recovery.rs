//! Crash recovery: on start-up, sessions still marked `recording` whose lock
//! is not held by a live process are closed: the last part is truncated to
//! its last complete page, part durations are recomputed from the files, and
//! the session is marked `crashed_recovered`. Running it twice is harmless.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::manifest::{Manifest, PartInfo, PartStatus, Status};
use crate::ogg_scan;
use crate::session::LOCK_FILE;

#[derive(Debug)]
pub struct Recovered {
    pub dir: PathBuf,
    pub parts: usize,
    pub duration_ms: u64,
    pub truncated_bytes: u64,
}

/// Recovers every crashed session under `root`. Errors for one session are
/// logged and do not stop the others.
pub fn recover_all(root: &Path) -> Vec<Recovered> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(manifest) = Manifest::load(&dir) else {
            continue;
        };
        if manifest.status != Status::Recording {
            continue;
        }
        match recover_session(&dir) {
            Ok(Some(r)) => {
                tracing::warn!(dir = %dir.display(), parts = r.parts, seconds = r.duration_ms / 1000, "recovered crashed session");
                out.push(r);
            }
            Ok(None) => {
                tracing::info!(dir = %dir.display(), "session is still being recorded by another process")
            }
            Err(e) => tracing::error!(dir = %dir.display(), error = %e, "session recovery failed"),
        }
    }
    out
}

/// Returns `Ok(None)` when another live process holds the session lock.
pub fn recover_session(dir: &Path) -> io::Result<Option<Recovered>> {
    let lock_path = dir.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    if lock.try_lock().is_err() {
        return Ok(None);
    }

    let mut manifest = Manifest::load(dir)?;
    let mut files: Vec<String> = fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("part_") && n.ends_with(".ogg"))
        .collect();
    files.sort();

    let mut parts = Vec::new();
    let mut offset = 0u64;
    let mut truncated_bytes = 0;
    let mut last_mtime = None;
    for file in files {
        let path = dir.join(&file);
        let known = manifest.parts.iter().find(|p| p.file == file).cloned();
        let start = known.as_ref().map_or(offset, |p| p.start_offset_ms);
        let (duration_ms, status) = if known
            .as_ref()
            .is_some_and(|p| p.status == PartStatus::Closed)
        {
            (
                known.as_ref().map_or(0, |p| p.duration_ms),
                PartStatus::Closed,
            )
        } else {
            let scan = ogg_scan::scan_file(&path)?;
            if scan.valid_len < scan.file_len {
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(scan.valid_len)?;
                truncated_bytes += scan.file_len - scan.valid_len;
            }
            let d = scan.duration_ms();
            (
                d,
                if scan.preskip.is_some() && d > 0 {
                    PartStatus::Recovered
                } else {
                    PartStatus::Unreadable
                },
            )
        };
        if let Ok(m) = fs::metadata(&path).and_then(|m| m.modified()) {
            last_mtime = Some(m);
        }
        parts.push(PartInfo {
            file,
            start_offset_ms: start,
            duration_ms,
            status,
        });
        offset = start + duration_ms;
    }

    let total = parts.iter().map(|p| p.duration_ms).sum();
    let count = parts.len();
    manifest.parts = parts;
    manifest.status = Status::CrashedRecovered;
    manifest.ended_at_utc = last_mtime.map(DateTime::<Utc>::from).or(Some(Utc::now()));
    manifest.push_event(
        offset,
        "crash_recovered",
        serde_json::json!({ "truncated_bytes": truncated_bytes }),
    );
    manifest.save(dir)?;
    let _ = lock.unlock();
    drop(lock);
    let _ = fs::remove_file(lock_path);
    Ok(Some(Recovered {
        dir: dir.to_path_buf(),
        parts: count,
        duration_ms: total,
        truncated_bytes,
    }))
}
