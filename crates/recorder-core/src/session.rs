//! A recording session on disk:
//!
//! ```text
//! recordings/2026-10-24_14-30-05_<uuid>/
//!   manifest.json
//!   session.lock      (held while recording; tells recovery the session is alive)
//!   part_0001.ogg     (L = microphone, R = system audio: for transcription)
//!   listen.ogg        (both sides mixed to mono: for listening)
//! ```
//!
//! By default the whole session is one part. A part limit (`part_seconds`)
//! splits it into `part_0001.ogg`, `part_0002.ogg`, … with seamless seams;
//! `listen.ogg` always covers the whole session.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Local, Utc};
use uuid::Uuid;

use crate::manifest::{Manifest, OsInfo, PartInfo, PartStatus, Status};
use crate::opus_ogg::{EncoderConfig, Layout, PartWriter};
use crate::{FRAME_SAMPLES, FRAMES_PER_SECOND};

pub const LOCK_FILE: &str = "session.lock";
/// Mono mix of both channels, written next to the parts.
pub const LISTEN_FILE: &str = "listen.ogg";

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Directory holding all session folders.
    pub root: PathBuf,
    /// Length of one part file in seconds; 0 = no limit, one file per session.
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
    /// `listen.ogg`, opened with the first frame and never rotated.
    listen: Option<PartWriter>,
    mix: Vec<f32>,
    /// `None` = never rotate.
    frames_per_part: Option<u64>,
    frames_total: u64,
    part_start_frame: u64,
    /// Time the session spent asleep or paused, not on the timeline.
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
            frames_per_part: (cfg.part_seconds > 0)
                .then(|| cfg.part_seconds as u64 * FRAMES_PER_SECOND),
            cfg,
            part: None,
            listen: None,
            mix: Vec::with_capacity(FRAME_SAMPLES),
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

    fn encoder_tag() -> (&'static str, String) {
        (
            "ENCODER",
            format!("meeting-recorder {}", env!("CARGO_PKG_VERSION")),
        )
    }

    fn open_part(&mut self) -> io::Result<()> {
        let index = self.manifest.parts.len() as u32 + 1;
        let file = part_file_name(index);
        let tags = [
            Self::encoder_tag(),
            ("SESSION_ID", self.manifest.session_id.to_string()),
            ("PART", index.to_string()),
            ("CHANNEL_0", "mic".to_string()),
            ("CHANNEL_1", "system".to_string()),
        ];
        let part = PartWriter::create(
            &self.dir.join(&file),
            rand_serial(index),
            &self.cfg.encoder,
            Layout::Split,
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

    fn open_listen(&mut self) -> io::Result<()> {
        let tags = [
            Self::encoder_tag(),
            ("SESSION_ID", self.manifest.session_id.to_string()),
            ("CHANNEL_0", "mic+system".to_string()),
        ];
        // One voice at a time needs half the bitrate of the two channels.
        let encoder = EncoderConfig {
            bitrate: self.cfg.encoder.bitrate / 2,
            ..self.cfg.encoder.clone()
        };
        let listen = PartWriter::create(
            &self.dir.join(LISTEN_FILE),
            rand_serial(0),
            &encoder,
            Layout::Mono,
            &tags,
        )?;
        self.listen = Some(listen);
        self.manifest.listen_file = Some(LISTEN_FILE.into());
        self.manifest.save(&self.dir)
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

    /// Appends one 20 ms interleaved stereo frame to the part (rotating parts
    /// if a part limit is set) and its mono mix to `listen.ogg`.
    pub fn push_frame(&mut self, interleaved: &[f32]) -> io::Result<()> {
        if self.listen.is_none() {
            self.open_listen()?;
        }
        match (&self.part, self.frames_per_part) {
            (None, _) => self.open_part()?,
            (Some(p), Some(limit)) if p.frames() >= limit => {
                self.close_part(Some(interleaved))?;
                self.open_part()?;
            }
            _ => {}
        }
        self.part
            .as_mut()
            .expect("part is open")
            .write_frame(interleaved)?;
        mix_to_mono(interleaved, &mut self.mix);
        self.listen
            .as_mut()
            .expect("listen file is open")
            .write_frame(&self.mix)?;
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

    /// Records a gap of `paused_ms` as a `sleep` or `pause` event. The same
    /// part continues, so the call stays one file; the event's `t_ms` and
    /// `duration_ms` place the gap on the wall clock.
    pub fn pause(&mut self, gap: Gap, paused_ms: u64) -> io::Result<()> {
        self.paused_ms += paused_ms;
        self.event(gap.event(), serde_json::json!({ "duration_ms": paused_ms }))
    }

    pub fn finish(mut self) -> io::Result<Manifest> {
        self.close_part(None)?;
        if let Some(listen) = self.listen.take() {
            listen.finish(None)?;
        }
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

/// Why the recording has a gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// The computer slept.
    Sleep,
    /// The user paused the recording.
    Pause,
}

impl Gap {
    fn event(self) -> &'static str {
        match self {
            Gap::Sleep => "sleep",
            Gap::Pause => "pause",
        }
    }
}

/// Both sides in both ears: `mic + system` per sample. People rarely talk at
/// once, so the sum keeps each voice at its recorded level; when they do, the
/// soft limiter rounds the peaks off instead of clipping them.
pub fn mix_to_mono(interleaved: &[f32], out: &mut Vec<f32>) {
    out.clear();
    out.extend(
        interleaved
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[mic, system]| soft_limit(mic + system)),
    );
}

/// Linear up to the knee, then bends smoothly towards ±1.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

fn rand_serial(index: u32) -> u32 {
    let u = Uuid::new_v4();
    u32::from_le_bytes(u.as_bytes()[..4].try_into().expect("4 bytes")) ^ index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_sums_sides_and_limits_peaks_softly() {
        let mut out = Vec::new();
        mix_to_mono(
            &[0.3, 0.0, 0.0, -0.4, 0.2, 0.3, 0.9, 0.9, -1.0, -1.0],
            &mut out,
        );
        assert_eq!(&out[..3], &[0.3, -0.4, 0.5], "one voice keeps its level");
        assert!(
            out[3] > 0.8 && out[3] < 1.0,
            "loud overlap is limited: {}",
            out[3]
        );
        assert!(
            out[4] < -out[3] && out[4] > -1.0,
            "and stays monotonic: {}",
            out[4]
        );
    }
}
