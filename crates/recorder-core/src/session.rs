//! A recording session on disk:
//!
//! ```text
//! recordings/2026-10-24_14-30-05_<uuid>/
//!   manifest.json
//!   session.lock      (held while recording; tells recovery the session is alive)
//!   part_0001.ogg
//!   part_0002.ogg
//! ```

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Local, Utc};
use uuid::Uuid;

use crate::FRAMES_PER_SECOND;
use crate::manifest::{Manifest, OsInfo, PartInfo, PartStatus, Status};
use crate::opus_ogg::{EncoderConfig, PartWriter};

pub const LOCK_FILE: &str = "session.lock";

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Directory holding all session folders.
    pub root: PathBuf,
    /// Length of one part file in seconds.
    pub part_seconds: u32,
    pub encoder: EncoderConfig,
    pub os: OsInfo,
}

pub fn part_file_name(index: u32) -> String {
    format!("part_{index:04}.ogg")
}

pub struct SessionWriter {
    dir: PathBuf,
    manifest: Manifest,
    cfg: SessionConfig,
    part: Option<PartWriter>,
    frames_per_part: u64,
    frames_total: u64,
    part_start_frame: u64,
    /// Time the session spent paused (system sleep), not on the timeline.
    paused_ms: u64,
    lock: Option<File>,
}

impl SessionWriter {
    pub fn create(cfg: SessionConfig) -> io::Result<Self> {
        let id = Uuid::new_v4();
        let name = format!("{}_{}", Local::now().format("%Y-%m-%d_%H-%M-%S"), id);
        let dir = cfg.root.join(name);
        fs::create_dir_all(&dir)?;
        let lock = File::create(dir.join(LOCK_FILE))?;
        lock.try_lock()
            .map_err(|e| io::Error::other(format!("cannot lock session: {e}")))?;
        let manifest = Manifest::new(
            id,
            Utc::now(),
            cfg.os.clone(),
            cfg.encoder.bitrate as u32,
            cfg.part_seconds,
        );
        manifest.save(&dir)?;
        Ok(Self {
            dir,
            manifest,
            frames_per_part: cfg.part_seconds as u64 * FRAMES_PER_SECOND,
            cfg,
            part: None,
            frames_total: 0,
            part_start_frame: 0,
            paused_ms: 0,
            lock: Some(lock),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Changes manifest fields (devices, capture method, ...) and saves it.
    pub fn update_manifest(&mut self, f: impl FnOnce(&mut Manifest)) -> io::Result<()> {
        f(&mut self.manifest);
        self.manifest.save(&self.dir)
    }

    /// Milliseconds of audio written so far.
    pub fn elapsed_ms(&self) -> u64 {
        self.frames_total * 20
    }

    pub fn current_part(&self) -> u32 {
        self.manifest.parts.len() as u32
    }

    /// Records an event at the current timeline position.
    pub fn event(&mut self, kind: &str, data: serde_json::Value) -> io::Result<()> {
        let t = self.elapsed_ms();
        self.manifest.push_event(t, kind, data);
        self.manifest.save(&self.dir)
    }

    fn open_part(&mut self) -> io::Result<()> {
        let index = self.manifest.parts.len() as u32 + 1;
        let file = part_file_name(index);
        let tags = [
            (
                "ENCODER",
                format!("meeting-recorder {}", env!("CARGO_PKG_VERSION")),
            ),
            ("SESSION_ID", self.manifest.session_id.to_string()),
            ("PART", index.to_string()),
            ("CHANNEL_0", "mic".to_string()),
            ("CHANNEL_1", "system".to_string()),
        ];
        let part = PartWriter::create(
            &self.dir.join(&file),
            rand_serial(index),
            &self.cfg.encoder,
            &tags,
        )?;
        self.part = Some(part);
        self.part_start_frame = self.frames_total;
        let wall_offset =
            chrono::Duration::milliseconds((self.frames_total * 20 + self.paused_ms) as i64);
        self.manifest.parts.push(PartInfo {
            started_at_utc: Some(self.manifest.started_at_utc + wall_offset),
            file,
            start_offset_ms: self.frames_total * 20,
            duration_ms: 0,
            status: PartStatus::Open,
        });
        self.manifest.save(&self.dir)?;
        tracing::info!(index, "part opened");
        Ok(())
    }

    fn close_part(&mut self, next: Option<&[f32]>) -> io::Result<()> {
        if let Some(part) = self.part.take() {
            let frames = part.finish(next)?;
            if let Some(info) = self.manifest.parts.last_mut() {
                info.duration_ms = frames * 20;
                info.status = PartStatus::Closed;
            }
            self.manifest.save(&self.dir)?;
            tracing::info!(
                part = self.manifest.parts.len(),
                seconds = frames / FRAMES_PER_SECOND,
                "part closed"
            );
        }
        Ok(())
    }

    /// Appends one 20 ms interleaved stereo frame, rotating parts as needed.
    pub fn push_frame(&mut self, interleaved: &[f32]) -> io::Result<()> {
        match &self.part {
            None => self.open_part()?,
            Some(p) if p.frames() >= self.frames_per_part => {
                self.close_part(Some(interleaved))?;
                self.open_part()?;
            }
            _ => {}
        }
        self.part
            .as_mut()
            .expect("part is open")
            .write_frame(interleaved)?;
        self.frames_total += 1;
        // Keep the open part's duration roughly current for anyone reading the manifest.
        if self.frames_total.is_multiple_of(30 * FRAMES_PER_SECOND) {
            if let Some(info) = self.manifest.parts.last_mut() {
                info.duration_ms = (self.frames_total - self.part_start_frame) * 20;
            }
            self.manifest.save(&self.dir)?;
        }
        Ok(())
    }

    /// Closes the current part after a pause of `paused_ms` (system sleep).
    /// The next frame starts a new part; the pause is recorded as an event.
    pub fn pause(&mut self, paused_ms: u64) -> io::Result<()> {
        self.close_part(None)?;
        self.paused_ms += paused_ms;
        self.event("sleep", serde_json::json!({ "duration_ms": paused_ms }))
    }

    pub fn finish(mut self) -> io::Result<Manifest> {
        self.close_part(None)?;
        self.manifest.status = Status::Completed;
        self.manifest.ended_at_utc = Some(Utc::now());
        self.manifest.save(&self.dir)?;
        if let Some(lock) = self.lock.take() {
            let _ = lock.unlock();
        }
        let _ = fs::remove_file(self.dir.join(LOCK_FILE));
        Ok(self.manifest)
    }
}

fn rand_serial(index: u32) -> u32 {
    let u = Uuid::new_v4();
    u32::from_le_bytes(u.as_bytes()[..4].try_into().expect("4 bytes")) ^ index
}
