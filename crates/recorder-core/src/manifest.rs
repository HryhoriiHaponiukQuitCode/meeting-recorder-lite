//! `manifest.json`: the description of one recording session. It is the
//! source of truth for crash recovery and, later, for the upload queue.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FILE_NAME: &str = "manifest.json";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Recording,
    Completed,
    CrashedRecovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartStatus {
    Open,
    Closed,
    /// Closed by crash recovery (truncated to the last complete page).
    Recovered,
    /// No decodable audio survived.
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStatus {
    /// The transcriber is working (or crashed; a new run resumes).
    Running,
    Done,
    Failed,
}

/// Written by the transcriber after the recording is closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptInfo {
    pub status: TranscriptStatus,
    pub updated_at_utc: DateTime<Utc>,
    /// e.g. `sherpa-onnx 1.13.8`.
    pub engine: String,
    /// e.g. `sherpa-onnx-nemo-parakeet-tdt-0.6b-ultra`.
    pub model: String,
    /// Output files in the session folder (`transcript.txt`, ...).
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OsInfo {
    pub family: String,
    pub version: String,
    pub arch: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioInfo {
    pub codec: String,
    pub container: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_map: ChannelMap,
    pub opus_layout: String,
    pub bitrate_bps: u32,
    pub frame_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ChannelMap {
    pub L: String,
    pub R: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub mic: Option<String>,
    pub output: Option<String>,
    /// `speakers`, `headphones`, `headset`, `bluetooth` or `unknown`.
    pub output_kind: String,
    /// `likely`, `unlikely` or `unknown`: can the microphone hear the remote side?
    pub echo_risk: String,
    pub aec: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartInfo {
    pub file: String,
    /// Position on the recording timeline (sleep periods are not counted).
    pub start_offset_ms: u64,
    /// Wall-clock start of the part. Gaps inside a part (sleep, pause) are
    /// the `sleep` and `pause` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_utc: Option<DateTime<Utc>>,
    pub duration_ms: u64,
    pub status: PartStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Milliseconds since session start.
    pub t_ms: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub session_id: Uuid,
    pub status: Status,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: Option<DateTime<Utc>>,
    pub app_version: String,
    pub os: OsInfo,
    pub audio: AudioInfo,
    pub devices: Devices,
    /// `wasapi_loopback`, `wasapi_process_loopback`, `coreaudio_process_tap`,
    /// `screencapturekit` or `none`.
    pub system_capture: String,
    /// Part limit in seconds; 0 = no limit (the session is one file).
    pub part_length_s: u32,
    pub parts: Vec<PartInfo>,
    /// Mono mix of both channels for listening (`listen.ogg`), one file for
    /// the whole session. Transcription uses the parts, where L and R stay
    /// separate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_file: Option<String>,
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptInfo>,
}

impl Manifest {
    pub fn new(
        session_id: Uuid,
        started_at_utc: DateTime<Utc>,
        os: OsInfo,
        bitrate_bps: u32,
        part_length_s: u32,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id,
            status: Status::Recording,
            started_at_utc,
            ended_at_utc: None,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            os,
            audio: AudioInfo {
                codec: "opus".into(),
                container: "ogg".into(),
                sample_rate: crate::SAMPLE_RATE,
                channels: crate::CHANNELS as u32,
                channel_map: ChannelMap {
                    L: "mic".into(),
                    R: "system".into(),
                },
                opus_layout: "multistream_2x_mono".into(),
                bitrate_bps,
                frame_ms: 20,
            },
            devices: Devices {
                output_kind: "unknown".into(),
                echo_risk: "unknown".into(),
                aec: "off".into(),
                ..Default::default()
            },
            system_capture: "none".into(),
            part_length_s,
            parts: Vec::new(),
            listen_file: None,
            events: Vec::new(),
            transcript: None,
        }
    }

    pub fn load(dir: &Path) -> io::Result<Self> {
        let data = fs::read(dir.join(FILE_NAME))?;
        serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Atomic replace: write a temporary file, fsync it, rename over the old one.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dir.join(FILE_NAME))?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    pub fn push_event(&mut self, t_ms: u64, kind: &str, data: serde_json::Value) {
        let data = match data {
            serde_json::Value::Object(m) => m,
            serde_json::Value::Null => Default::default(),
            other => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        self.events.push(Event {
            t_ms,
            kind: kind.to_string(),
            data,
        });
    }
}
