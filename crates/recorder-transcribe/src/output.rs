//! The files written next to the recording:
//!
//! - `transcript.txt`: the dialogue, one turn per line with its time;
//! - `transcript.srt`: subtitles, one utterance each, to play along with
//!   `listen.ogg`;
//! - `transcript.json`: every utterance with word times, echo flags, engine,
//!   model and settings.
//!
//! Times are positions in the recording (pauses and sleep are not part of
//! it), so they match the audio files. Each file is written atomically:
//! temporary file, fsync, rename.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::dialog::{Turn, turns};
use crate::transcript::Utterance;

pub const TXT_FILE: &str = "transcript.txt";
pub const JSON_FILE: &str = "transcript.json";
pub const SRT_FILE: &str = "transcript.srt";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VadSettings {
    pub model: String,
    pub threshold: f32,
    pub min_silence_s: f32,
    pub min_speech_s: f32,
    pub max_speech_s: f32,
    pub window_samples: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub threads: u32,
    pub decoding: String,
    pub vad: VadSettings,
    pub chunking: String,
    pub max_chunk_speech_s: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub format_version: u32,
    pub session_id: String,
    pub created_at_utc: DateTime<Utc>,
    pub engine: String,
    pub model: String,
    pub settings: Settings,
    /// Length of the recording, seconds.
    pub audio_s: f64,
    /// Seconds the transcription took (this run).
    pub processing_s: f64,
    pub speakers: Speakers,
    pub utterances: Vec<Utterance>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Speakers {
    pub me: String,
    pub others: String,
}

impl Default for Speakers {
    fn default() -> Self {
        Self {
            me: "microphone (left channel)".into(),
            others: "system audio (right channel)".into(),
        }
    }
}

/// `hh:mm:ss`.
pub fn clock(t: f64) -> String {
    let s = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// `hh:mm:ss,mmm`.
fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

pub fn txt(doc: &Document) -> String {
    let mut s = format!(
        "Transcript of a {} recording. Me = microphone, Others = system audio.\n\
         Times are positions in the recording. Transcribed locally with {} ({}).\n\n",
        clock(doc.audio_s),
        doc.model,
        doc.engine
    );
    let t: Vec<Turn> = turns(&doc.utterances);
    if t.is_empty() {
        s.push_str("(no speech found)\n");
    }
    for turn in t {
        s.push_str(&format!(
            "[{}] {}: {}\n",
            clock(turn.start),
            turn.speaker.label(),
            turn.text
        ));
    }
    s
}

pub fn srt(doc: &Document) -> String {
    let mut s = String::new();
    for (i, u) in doc.utterances.iter().filter(|u| !u.echo).enumerate() {
        // Keep subtitles on screen for at least a moment.
        let end = u.end.max(u.start + 0.5);
        s.push_str(&format!(
            "{}\n{} --> {}\n{}: {}\n\n",
            i + 1,
            srt_time(u.start),
            srt_time(end),
            u.speaker.label(),
            u.text
        ));
    }
    s
}

/// Temporary file, fsync, rename over the target.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)
}

/// Writes all three files; returns their names.
pub fn write_all(dir: &Path, doc: &Document) -> io::Result<Vec<String>> {
    let json = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
    write_atomic(&dir.join(JSON_FILE), &json)?;
    write_atomic(&dir.join(SRT_FILE), srt(doc).as_bytes())?;
    write_atomic(&dir.join(TXT_FILE), txt(doc).as_bytes())?;
    Ok(vec![TXT_FILE.into(), JSON_FILE.into(), SRT_FILE.into()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Speaker;

    fn doc() -> Document {
        let u = |speaker, start, end, text: &str, echo| Utterance {
            speaker,
            start,
            end,
            text: text.into(),
            words: vec![],
            echo,
        };
        Document {
            format_version: FORMAT_VERSION,
            session_id: "s".into(),
            created_at_utc: Utc::now(),
            engine: "sherpa-onnx 1.13.8".into(),
            model: "parakeet".into(),
            settings: Settings {
                threads: 2,
                decoding: "greedy_search".into(),
                vad: VadSettings {
                    model: "silero".into(),
                    threshold: 0.5,
                    min_silence_s: 0.25,
                    min_speech_s: 0.25,
                    max_speech_s: 20.0,
                    window_samples: 512,
                },
                chunking: "speech only".into(),
                max_chunk_speech_s: 30,
            },
            audio_s: 3725.0,
            processing_s: 1.0,
            speakers: Speakers::default(),
            utterances: vec![
                u(Speaker::Others, 1.0, 2.0, "Привіт.", false),
                u(Speaker::Me, 1.2, 1.9, "привіт", true),
                u(Speaker::Me, 3661.25, 3662.0, "Добрий день.", false),
            ],
        }
    }

    #[test]
    fn text_and_subtitles_leave_echo_out() {
        let d = doc();
        let t = txt(&d);
        assert!(t.starts_with("Transcript of a 01:02:05 recording"));
        assert!(t.contains("[00:00:01] Others: Привіт.\n"));
        assert!(t.contains("[01:01:01] Me: Добрий день.\n"));
        assert!(!t.contains("привіт\n"));
        let s = srt(&d);
        assert!(
            s.starts_with("1\n00:00:01,000 --> 00:00:02,000\nOthers: Привіт.\n\n2\n01:01:01,250")
        );
        assert_eq!(s.matches(" --> ").count(), 2);
    }

    #[test]
    fn files_are_written_and_json_round_trips() {
        let dir = std::env::temp_dir().join(format!("mr-out-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let d = doc();
        let files = write_all(&dir, &d).unwrap();
        assert_eq!(files.len(), 3);
        let back: Document =
            serde_json::from_slice(&fs::read(dir.join(JSON_FILE)).unwrap()).unwrap();
        assert_eq!(back, d);
        assert!(
            fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        fs::remove_dir_all(dir).ok();
    }
}
