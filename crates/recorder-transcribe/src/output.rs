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
    #[serde(default)]
    pub quality: crate::model::Quality,
    pub threads: u32,
    pub decoding: String,
    pub vad: VadSettings,
    pub chunking: String,
    pub max_chunk_speech_s: u32,
    /// How the voices of the system audio were told apart; absent when off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voices: Option<VoiceSettings>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub model: String,
    pub window_s: f64,
    pub step_s: f64,
    pub threshold: f32,
}

impl VoiceSettings {
    pub fn current() -> Self {
        Self {
            model: crate::model::VOICE_FILE.name.into(),
            window_s: crate::voices::WINDOW_S,
            step_s: crate::voices::STEP_S,
            threshold: crate::voices::THRESHOLD,
        }
    }
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
    /// Voices told apart on the system audio (0 = not separated or one).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub others_voices: u32,
    /// Who speaks: a name found in the conversation, else "Participant N"
    /// (see [`crate::names`]). Empty in transcripts made before names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<Participant>,
    pub utterances: Vec<Utterance>,
}

/// One participant: the microphone, or one voice of the system audio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub speaker: crate::Speaker,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<u32>,
    /// Found in the conversation ("Дякую, Роман, за коментар").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the text shows: the name or "Participant N".
    pub label: String,
}

impl Document {
    /// The label of a speaker and voice: the participant's, or the channel
    /// name in transcripts made before names.
    pub fn label(&self, speaker: crate::Speaker, voice: Option<u32>) -> String {
        self.participants
            .iter()
            .find(|p| p.speaker == speaker && p.voice == voice)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| crate::transcript::label(speaker, voice))
    }
}

/// Participants of a dialogue with their labels (names where found).
pub fn participants(dialog: &[Utterance]) -> Vec<Participant> {
    let names = crate::names::find(dialog);
    crate::names::labels(dialog, &names)
        .into_iter()
        .map(|((speaker, voice), label)| Participant {
            speaker,
            voice,
            name: names.get(&(speaker, voice)).cloned(),
            label,
        })
        .collect()
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// The dialogue as plain text, one turn per line: `[hh:mm:ss] Name: text`.
/// Nothing else: what the transcript was made with is in the JSON.
pub fn txt(doc: &Document) -> String {
    let mut s = String::new();
    let t: Vec<Turn> = turns(&doc.utterances);
    if t.is_empty() {
        s.push_str("(no speech found)\n");
    }
    for turn in t {
        s.push_str(&format!(
            "[{}] {}: {}\n",
            clock(turn.start),
            doc.label(turn.speaker, turn.voice),
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
            doc.label(u.speaker, u.voice),
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
            voice: None,
        };
        Document {
            format_version: FORMAT_VERSION,
            session_id: "s".into(),
            created_at_utc: Utc::now(),
            engine: "sherpa-onnx 1.13.8".into(),
            model: "parakeet".into(),
            settings: Settings {
                quality: crate::model::Quality::Best,
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
                voices: None,
            },
            audio_s: 3725.0,
            processing_s: 1.0,
            speakers: Speakers::default(),
            others_voices: 0,
            participants: vec![],
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
        assert!(t.starts_with("[00:00:01] Others: Привіт.\n"), "{t}");
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
    fn participants_label_the_text() {
        let mut d = doc();
        d.utterances[0].voice = Some(1);
        d.utterances.push(Utterance {
            voice: Some(2),
            ..d.utterances[0].clone()
        });
        d.others_voices = 2;
        d.participants = vec![
            Participant {
                speaker: Speaker::Others,
                voice: Some(1),
                name: None,
                label: "Participant 1".into(),
            },
            Participant {
                speaker: Speaker::Others,
                voice: Some(2),
                name: Some("Катя".into()),
                label: "Катя".into(),
            },
            Participant {
                speaker: Speaker::Me,
                voice: None,
                name: None,
                label: "Participant 2".into(),
            },
        ];
        let t = txt(&d);
        assert!(t.contains("[00:00:01] Participant 1: Привіт.\n"), "{t}");
        assert!(t.contains("] Катя: Привіт.\n"), "{t}");
        assert!(
            t.contains("[01:01:01] Participant 2: Добрий день.\n"),
            "{t}"
        );
        assert!(
            !t.contains("Transcribed") && !t.contains("Names come from"),
            "{t}"
        );
        assert!(srt(&d).contains("Катя: Привіт."));
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
