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

/// The transcript as data: what the window reads and edits. Named the same
/// in every recording.
pub const JSON_FILE: &str = "transcript.json";
/// The text and the subtitles are for people, so they carry the
/// recording's start time and name: "2026-11-13_12-00_transcript.txt", or
/// "<name>_2026-11-13_12-00_transcript.txt" once it is named (see
/// [`file_base`]).
pub const TEXT_SUFFIX: &str = "_transcript";
/// Their names before November 2026 (found and renamed by
/// [`sync_text_names`]).
pub const OLD_TXT_FILE: &str = "transcript.txt";
pub const OLD_SRT_FILE: &str = "transcript.srt";
/// The recording's name, one line, written by the app when the user names
/// it.
pub const TITLE_FILE: &str = "title.txt";
/// File names stay well under the 255-byte limit of most file systems
/// (Cyrillic takes two bytes a letter).
const BASE_MAX_BYTES: usize = 150;
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
    /// On the microphone (absent in transcripts made before it differed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_threshold: Option<f32>,
}

impl VoiceSettings {
    pub fn current() -> Self {
        Self {
            model: crate::model::VOICE_FILE.name.into(),
            window_s: crate::voices::WINDOW_S,
            step_s: crate::voices::STEP_S,
            threshold: crate::voices::THRESHOLD,
            mic_threshold: Some(crate::voices::MIC_THRESHOLD),
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
    /// Voices told apart on the microphone (several people in the room; 0 =
    /// one or not separated).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub me_voices: u32,
    /// Who speaks: the user's name on the microphone (from the settings) or
    /// one the user gave, else "Participant N". Names are never guessed
    /// from the conversation. Empty in transcripts made before names.
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
    /// Given by the user: their own name from the settings, or one typed
    /// in the window ([`rename_participant`]). (Transcripts made before
    /// November 2026 may also hold a `name` guessed from the conversation;
    /// it is no longer read.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub given: Option<String>,
    /// What the text shows: the given name or "Participant N".
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

/// Longer participant names are cut.
pub const NAME_MAX_CHARS: usize = 60;

impl Document {
    /// Names the participant `speaker`/`voice`; an empty `name` brings back
    /// "Participant N". False when no utterance is theirs.
    pub fn rename(&mut self, speaker: crate::Speaker, voice: Option<u32>, name: &str) -> bool {
        if !self
            .utterances
            .iter()
            .any(|u| u.speaker == speaker && u.voice == voice)
        {
            return false;
        }
        let name = clean_name(name);
        // The automatic label, as the transcriber made it.
        let found = participants(&self.utterances, None)
            .into_iter()
            .find(|p| p.speaker == speaker && p.voice == voice);
        let i = match self
            .participants
            .iter()
            .position(|p| p.speaker == speaker && p.voice == voice)
        {
            Some(i) => i,
            None => {
                // Transcripts made before names: every participant gets an
                // entry, so the others keep their labels.
                if self.participants.is_empty() {
                    self.participants = participants(&self.utterances, None);
                }
                match self
                    .participants
                    .iter()
                    .position(|p| p.speaker == speaker && p.voice == voice)
                {
                    Some(i) => i,
                    None => {
                        self.participants.push(found.clone().unwrap_or(Participant {
                            speaker,
                            voice,
                            given: None,
                            label: crate::transcript::label(speaker, voice),
                        }));
                        self.participants.len() - 1
                    }
                }
            }
        };
        let p = &mut self.participants[i];
        if name.is_empty() {
            p.given = None;
            p.label = found
                .map(|f| f.label)
                .unwrap_or_else(|| crate::transcript::label(speaker, voice));
        } else {
            p.given = Some(name.clone());
            p.label = name;
        }
        true
    }
}

/// Renames a participant of the transcript in `dir` and writes its three
/// files again ([`Document::rename`]).
pub fn rename_participant(
    dir: &Path,
    speaker: crate::Speaker,
    voice: Option<u32>,
    name: &str,
) -> io::Result<()> {
    let bytes = fs::read(dir.join(JSON_FILE))?;
    let mut doc: Document = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if !doc.rename(speaker, voice, name) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this participant is not in the transcript",
        ));
    }
    write_all(dir, &doc).map(|_| ())
}

/// Participants of a dialogue with their labels: `me` (the user's name
/// from the settings) for the microphone voice that says the most, else
/// "Participant N", numbered in order of appearance. Nothing is guessed
/// from what is said: guesses were often someone the people talked about
/// ("асайнила Андрія" made the user Андрій), and the user renames
/// participants in the window instead.
pub fn participants(dialog: &[Utterance], me: Option<&str>) -> Vec<Participant> {
    let me = me.map(clean_name).filter(|n| !n.is_empty());
    // With several people on the microphone the user is the one who says
    // the most (nearest to it).
    let mut said: Vec<(Option<u32>, usize)> = Vec::new();
    for u in dialog
        .iter()
        .filter(|u| u.speaker == crate::Speaker::Me && !u.echo)
    {
        let n = u.text.split_whitespace().count();
        match said.iter_mut().find(|(v, _)| *v == u.voice) {
            Some(s) => s.1 += n,
            None => said.push((u.voice, n)),
        }
    }
    let user_voice = said.iter().max_by_key(|(_, n)| *n).and_then(|(v, _)| *v);
    // Everyone else is "Participant N", numbered in order of appearance.
    let mut out: Vec<Participant> = Vec::new();
    let mut n = 0;
    for u in dialog.iter().filter(|u| !u.echo) {
        let (speaker, voice) = (u.speaker, u.voice);
        if out.iter().any(|p| p.speaker == speaker && p.voice == voice) {
            continue;
        }
        let given = (speaker == crate::Speaker::Me && voice == user_voice)
            .then(|| me.clone())
            .flatten();
        let label = given.clone().unwrap_or_else(|| {
            n += 1;
            format!("Participant {n}")
        });
        out.push(Participant {
            speaker,
            voice,
            given,
            label,
        });
    }
    out
}

/// One line, spaces collapsed, at most [`NAME_MAX_CHARS`].
fn clean_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(NAME_MAX_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
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

/// Writes all three files (the text and subtitles under the recording's
/// current name, older copies removed); returns their names.
pub fn write_all(dir: &Path, doc: &Document) -> io::Result<Vec<String>> {
    let json = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
    let (txt_name, srt_name) = (text_name(dir, "txt"), text_name(dir, "srt"));
    write_atomic(&dir.join(JSON_FILE), &json)?;
    write_atomic(&dir.join(&srt_name), srt(doc).as_bytes())?;
    write_atomic(&dir.join(&txt_name), txt(doc).as_bytes())?;
    sync_text_names(dir)?;
    Ok(vec![txt_name, JSON_FILE.into(), srt_name])
}

/// What the text files are named after: when the recording started, from
/// its folder ("2026-11-13_12-00"), always; with the recording's name
/// ([`TITLE_FILE`], made safe for a file name) in front once it has one
/// ("LA Client Meet_2026-11-13_12-00"). The date keeps the files of
/// recordings with the same name apart and in order.
pub fn file_base(dir: &Path) -> String {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Session folders: "2026-11-13_12-00-58_<id>".
    let date: String = folder.chars().take(16).collect();
    let looks_like_date = date.len() == 16
        && date.chars().enumerate().all(|(i, c)| {
            matches!(i, 4 | 7 | 13) && c == '-'
                || i == 10 && c == '_'
                || !matches!(i, 4 | 7 | 10 | 13) && c.is_ascii_digit()
        });
    let when = if looks_like_date {
        date
    } else if folder.is_empty() {
        "recording".into()
    } else {
        safe_file_name(&folder)
    };
    match fs::read_to_string(dir.join(TITLE_FILE))
        .ok()
        .map(|t| safe_file_name(&t))
        .filter(|t| !t.is_empty())
    {
        Some(name) => format!("{name}_{when}"),
        None => when,
    }
}

/// `name` as a file name on any system: one line, without / \ : * ? " < >
/// | and control characters (each becomes "-"), no dots or spaces at the
/// ends, at most [`BASE_MAX_BYTES`].
pub fn safe_file_name(name: &str) -> String {
    let one_line = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mapped: String = one_line
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let mut out = String::new();
    for c in mapped.trim_matches(|c: char| c == '.' || c == ' ').chars() {
        if out.len() + c.len_utf8() > BASE_MAX_BYTES {
            break;
        }
        out.push(c);
    }
    out.trim_end_matches(['.', ' ']).to_string()
}

/// "<name>_transcript.<ext>" for the recording in `dir`.
pub fn text_name(dir: &Path, ext: &str) -> String {
    format!("{}{TEXT_SUFFIX}.{ext}", file_base(dir))
}

/// Text or subtitle files of a transcript in `dir` under any name (an
/// older name of the recording, or the names before November 2026).
fn text_files(dir: &Path, ext: &str) -> Vec<std::path::PathBuf> {
    let ending = format!("{TEXT_SUFFIX}.{ext}");
    let old = format!("transcript.{ext}");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            !n.starts_with('.') && (n.ends_with(&ending) || n == old)
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// The transcript's text file in `dir`, under whatever name it has now.
pub fn txt_path(dir: &Path) -> Option<std::path::PathBuf> {
    let want = dir.join(text_name(dir, "txt"));
    if want.is_file() {
        return Some(want);
    }
    text_files(dir, "txt").into_iter().next()
}

/// Every text and subtitle file of the transcript in `dir`.
pub fn text_paths(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut all = text_files(dir, "txt");
    all.extend(text_files(dir, "srt"));
    all
}

/// Gives the text and subtitle files the recording's current name (after
/// it is renamed, or for transcripts from before names): one of each is
/// renamed, other copies removed. A change of letter case alone goes
/// through a temporary name, as the Mac's and Windows' file systems see
/// both as one file.
pub fn sync_text_names(dir: &Path) -> io::Result<()> {
    for ext in ["txt", "srt"] {
        let name = text_name(dir, ext);
        let want = dir.join(&name);
        let mut have = false;
        for f in text_files(dir, ext) {
            let current = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if current == name {
                have = true;
            } else if current.to_lowercase() == name.to_lowercase() {
                let tmp = dir.join(format!(".{name}.case"));
                fs::rename(&f, &tmp)?;
                fs::rename(&tmp, &want)?;
                have = true;
            }
        }
        for f in text_files(dir, ext) {
            if f == want {
                continue;
            }
            if have {
                fs::remove_file(&f)?;
            } else {
                fs::rename(&f, &want)?;
                have = true;
            }
        }
    }
    Ok(())
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
            me_voices: 0,
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
                given: None,
                label: "Participant 1".into(),
            },
            Participant {
                speaker: Speaker::Others,
                voice: Some(2),
                given: Some("Катя".into()),
                label: "Катя".into(),
            },
            Participant {
                speaker: Speaker::Me,
                voice: None,
                given: None,
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
    fn the_microphone_takes_the_users_name_never_a_guess() {
        let mut d = doc();
        // "Others" says "Андрію": a guess would make the user Андрій.
        d.utterances[0].text = "Андрію, привіт.".into();
        d.utterances[1].text = "Привіт, Андрію.".into();
        let p = participants(&d.utterances, None);
        let me = p.iter().find(|p| p.speaker == Speaker::Me).unwrap();
        assert_eq!(me.given.as_deref(), None);
        assert!(me.label.starts_with("Participant "), "{me:?}");
        let p = participants(&d.utterances, Some("  Григорій "));
        let me = p.iter().find(|p| p.speaker == Speaker::Me).unwrap();
        assert_eq!(me.label, "Григорій");
        assert_eq!(me.given.as_deref(), Some("Григорій"));
        assert!(
            participants(&d.utterances, Some(" "))
                .iter()
                .all(|p| p.given.is_none())
        );
    }

    /// What people call each other never names anyone: everyone is
    /// "Participant N" until the user renames them.
    #[test]
    fn names_said_in_the_call_are_not_taken() {
        let mut d = doc();
        d.utterances[0].text = "Дякую, Роман, за коментар. Каті, розкажи.".into();
        d.utterances[1].text = "Мене звати Оля, я з маркетингу.".into();
        let p = participants(&d.utterances, None);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|p| p.given.is_none()), "{p:?}");
        let labels: Vec<&str> = p.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Participant 1", "Participant 2"]);
        // The user's name from the settings does not shift the numbers.
        let p = participants(&d.utterances, Some("Григорій"));
        let labels: Vec<&str> = p.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Participant 1", "Григорій"]);
    }

    #[test]
    fn a_participant_can_be_renamed_and_back() {
        let mut d = doc();
        d.participants = participants(&d.utterances, None);
        let auto = d.label(Speaker::Me, None);
        assert!(d.rename(Speaker::Me, None, "  Григорій \n Гапонюк "));
        assert_eq!(d.label(Speaker::Me, None), "Григорій Гапонюк");
        assert!(
            txt(&d).contains("Григорій Гапонюк: Добрий день."),
            "{}",
            txt(&d)
        );
        let p = d
            .participants
            .iter()
            .find(|p| p.speaker == Speaker::Me)
            .unwrap();
        assert_eq!(p.given.as_deref(), Some("Григорій Гапонюк"));
        // Empty: the automatic label again.
        assert!(d.rename(Speaker::Me, None, " "));
        assert_eq!(d.label(Speaker::Me, None), auto);
        assert!(d.participants.iter().all(|p| p.given.is_none()));
        // Long names are cut; nobody with this voice: refused.
        assert!(d.rename(Speaker::Others, None, &"я".repeat(200)));
        assert_eq!(
            d.label(Speaker::Others, None).chars().count(),
            NAME_MAX_CHARS
        );
        assert!(!d.rename(Speaker::Others, Some(7), "Х"));
    }

    #[test]
    fn renaming_writes_all_three_files() {
        let dir = std::env::temp_dir().join(format!("mr-rename-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A transcript made before names: no participants yet.
        write_all(&dir, &doc()).unwrap();
        rename_participant(&dir, Speaker::Others, None, "Катя").unwrap();
        let back: Document =
            serde_json::from_slice(&fs::read(dir.join(JSON_FILE)).unwrap()).unwrap();
        assert_eq!(back.label(Speaker::Others, None), "Катя");
        assert_ne!(
            back.label(Speaker::Me, None),
            "Катя",
            "the other keeps its own"
        );
        assert!(
            fs::read_to_string(txt_path(&dir).unwrap())
                .unwrap()
                .contains("Катя: Привіт.")
        );
        assert!(
            fs::read_to_string(dir.join(text_name(&dir, "srt")))
                .unwrap()
                .contains("Катя: Привіт.")
        );
        assert!(rename_participant(&dir, Speaker::Others, Some(3), "X").is_err());
        fs::remove_dir_all(dir).ok();
    }

    /// The text and subtitles carry the recording's start time, and its
    /// name in front once it has one; they follow a rename.
    #[test]
    fn text_files_are_named_after_the_recording() {
        let root = std::env::temp_dir().join(format!("mr-names-{}", std::process::id()));
        let dir = root.join("2026-11-13_12-00-58_a55931a9-73");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&dir).unwrap();
        let names = |dir: &Path| {
            let mut v: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        let files = write_all(&dir, &doc()).unwrap();
        assert_eq!(
            files,
            [
                "2026-11-13_12-00_transcript.txt",
                "transcript.json",
                "2026-11-13_12-00_transcript.srt"
            ]
        );
        // Named: the files follow, and no older copy is left.
        fs::write(dir.join(TITLE_FILE), "LA Cleint Meet\n").unwrap();
        sync_text_names(&dir).unwrap();
        assert_eq!(
            names(&dir),
            [
                "LA Cleint Meet_2026-11-13_12-00_transcript.srt",
                "LA Cleint Meet_2026-11-13_12-00_transcript.txt",
                TITLE_FILE,
                JSON_FILE
            ]
        );
        assert!(
            txt_path(&dir)
                .unwrap()
                .ends_with("LA Cleint Meet_2026-11-13_12-00_transcript.txt")
        );
        // Only the case changes (one file to the Mac): still renamed.
        fs::write(dir.join(TITLE_FILE), "LA client meet").unwrap();
        sync_text_names(&dir).unwrap();
        assert!(
            names(&dir).contains(&"LA client meet_2026-11-13_12-00_transcript.txt".to_string())
        );
        assert_eq!(text_paths(&dir).len(), 2);
        // Written again after a rename: one pair, the new name.
        fs::write(dir.join(TITLE_FILE), "Нарада: план / Q4?").unwrap();
        write_all(&dir, &doc()).unwrap();
        assert_eq!(
            names(&dir)
                .iter()
                .filter(|n| n.contains(TEXT_SUFFIX))
                .collect::<Vec<_>>(),
            [
                "Нарада- план - Q4-_2026-11-13_12-00_transcript.srt",
                "Нарада- план - Q4-_2026-11-13_12-00_transcript.txt"
            ]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unsafe_names_become_safe_file_names() {
        assert_eq!(
            safe_file_name("  a/b\\c:d*e?f\"g<h>i|j \n k "),
            "a-b-c-d-e-f-g-h-i-j k"
        );
        assert_eq!(safe_file_name("..hidden."), "hidden");
        assert_eq!(safe_file_name(" ... "), "");
        let long = safe_file_name(&"Нарада ".repeat(40));
        assert!(
            long.len() <= BASE_MAX_BYTES && !long.ends_with(' '),
            "{long}"
        );
        assert!(long.is_char_boundary(long.len()));
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
