//! From recognizer tokens to timed utterances.
//!
//! Parakeet returns sub-word tokens; a token that starts a word begins with
//! a space. Each token has a start time and a duration inside its chunk,
//! which [`Chunk::to_timeline`] maps to the recording.

use serde::{Deserialize, Serialize};

use crate::Speaker;
use crate::chunk::Chunk;

/// What the recognizer returned for one chunk (also stored in the
/// checkpoint, so it must survive a JSON round trip).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkResult {
    pub text: String,
    pub tokens: Vec<String>,
    /// Token start times inside the chunk, seconds.
    pub timestamps: Vec<f32>,
    /// Token durations, seconds (may be empty).
    #[serde(default)]
    pub durations: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    /// Seconds on the recording timeline.
    pub start: f64,
    pub end: f64,
    /// In the Best quality: which model wrote it (`whisper`, `parakeet`
    /// for a stretch Whisper skipped, `parakeet-en` for English).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Which voice of the system audio (1, 2, …) when several were told
    /// apart (see [`crate::voices`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<u32>,
}

impl Word {
    pub fn new(text: impl Into<String>, start: f64, end: f64) -> Self {
        Self {
            text: text.into(),
            start,
            end,
            source: None,
            voice: None,
        }
    }
}

/// One sentence (or a phrase ended by a pause) of one speaker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Utterance {
    pub speaker: Speaker,
    /// Seconds on the recording timeline (pauses and sleep not counted, so
    /// they match positions in the audio files).
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub words: Vec<Word>,
    /// The microphone picked up the other side (see [`crate::dialog`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub echo: bool,
    /// Voice number on the system audio, when several were told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<u32>,
}

impl Utterance {
    /// "Me", "Others" or "Others 2".
    pub fn label(&self) -> String {
        label(self.speaker, self.voice)
    }
}

/// "Me", "Others" or "Others 2".
pub fn label(speaker: Speaker, voice: Option<u32>) -> String {
    match voice {
        Some(v) => format!("{} {v}", speaker.label()),
        None => speaker.label().to_string(),
    }
}

/// A pause this long between words starts a new utterance.
const PAUSE_SPLIT_S: f64 = 1.5;
/// Token duration when the recognizer gives none (one encoder frame).
const DEFAULT_TOKEN_S: f64 = 0.08;

fn ends_sentence(word: &str) -> bool {
    word.ends_with(['.', '?', '!', '…'])
}

/// Splits a chunk's tokens into words and utterances on the timeline.
pub fn utterances(speaker: Speaker, chunk: &Chunk, result: &ChunkResult) -> Vec<Utterance> {
    group(speaker, chunk_words(chunk, result))
}

/// A chunk's tokens as words on the recording timeline.
pub fn chunk_words(chunk: &Chunk, result: &ChunkResult) -> Vec<Word> {
    // Tokens -> words. Punctuation tokens have no leading space and attach
    // to the previous word. Before digits (and some words) the model emits
    // a token that is only a space: the next token starts a new word.
    let mut words: Vec<Word> = Vec::new();
    let mut space = false;
    for (i, token) in result.tokens.iter().enumerate() {
        let Some(&t) = result.timestamps.get(i) else {
            break;
        };
        if token.trim().is_empty() {
            space = true;
            continue;
        }
        // The model's "unknown" token is not a word.
        if token.trim() == "<unk>" {
            space = true;
            continue;
        }
        let d = result
            .durations
            .get(i)
            .map_or(DEFAULT_TOKEN_S, |&d| d as f64);
        let start = chunk.to_timeline(t as f64);
        let end = start + d.max(0.0);
        let starts_word = space || token.starts_with(' ');
        space = false;
        match words.last_mut() {
            Some(w) if !starts_word => {
                w.text.push_str(token);
                w.end = w.end.max(end);
            }
            _ => words.push(Word::new(token.trim_start(), start, end)),
        }
    }
    words
}

/// Words of one speaker as utterances: a new one after a sentence end, a
/// pause of more than [`PAUSE_SPLIT_S`] or a change of voice.
pub fn group(speaker: Speaker, words: Vec<Word>) -> Vec<Utterance> {
    let mut out: Vec<Utterance> = Vec::new();
    let mut new_utterance = true;
    for w in words {
        match out.last_mut() {
            Some(u) if !new_utterance && w.start - u.end <= PAUSE_SPLIT_S && w.voice == u.voice => {
                u.text.push(' ');
                u.text.push_str(&w.text);
                u.end = u.end.max(w.end);
                new_utterance = ends_sentence(&w.text);
                u.words.push(w);
            }
            _ => {
                new_utterance = ends_sentence(&w.text);
                out.push(Utterance {
                    speaker,
                    start: w.start,
                    end: w.end,
                    text: w.text.clone(),
                    voice: w.voice,
                    words: vec![w],
                    echo: false,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Chunker;

    fn result(tokens: &[(&str, f32)]) -> ChunkResult {
        ChunkResult {
            text: tokens.iter().map(|t| t.0).collect(),
            tokens: tokens.iter().map(|t| t.0.to_string()).collect(),
            timestamps: tokens.iter().map(|t| t.1).collect(),
            durations: vec![0.08; tokens.len()],
        }
    }

    #[test]
    fn tokens_become_words_and_sentences_on_the_timeline() {
        let mut c = Chunker::default();
        // Speech at 10 s (2 s long) and at 100 s.
        c.push(160_000, &[0.0; 32_000]);
        c.push(1_600_000, &[0.0; 32_000]);
        let chunk = c.finish().unwrap();
        let r = result(&[
            (" Пр", 0.0),
            ("ивіт", 0.08),
            (".", 0.24),
            (" Як", 0.4),
            (" спра", 0.6),
            ("ви", 0.7),
            ("?", 0.8),
            // After the removed pause: 2.1 s in the chunk = 100.1 s.
            (" Доб", 2.1),
            ("ре", 2.2),
        ]);
        let u = utterances(Speaker::Others, &chunk, &r);
        assert_eq!(u.len(), 3);
        assert_eq!(u[0].text, "Привіт.");
        assert_eq!(u[1].text, "Як справи?");
        assert_eq!(u[1].words.len(), 2);
        assert!((u[0].start - 10.0).abs() < 1e-9);
        assert!((u[2].start - 100.1).abs() < 1e-6);
        assert_eq!(u[2].text, "Добре");
        assert!(u.iter().all(|x| x.speaker == Speaker::Others));
    }

    #[test]
    fn a_long_pause_splits_a_sentence() {
        let mut c = Chunker::default();
        c.push(0, &[0.0; 16_000]);
        c.push(16_000 * 20, &[0.0; 16_000]);
        let chunk = c.finish().unwrap();
        let r = result(&[(" і", 0.2), (" потім", 0.5), (" ми", 1.2)]);
        let u = utterances(Speaker::Me, &chunk, &r);
        assert_eq!(u.len(), 2, "{u:?}");
        assert_eq!(u[0].text, "і потім");
        assert_eq!(u[1].text, "ми");
    }

    /// Real tokens: `[" чи", " на", " ", "1", "4", ","]` is "чи на 14,".
    #[test]
    fn a_space_token_separates_numbers_and_words() {
        let mut c = Chunker::default();
        c.push(0, &[0.0; 48_000]);
        let chunk = c.finish().unwrap();
        let r = result(&[
            (" чи", 0.0),
            (" на", 0.2),
            (" ", 0.4),
            ("1", 0.45),
            ("4", 0.5),
            (",", 0.6),
            (" нов", 0.8),
            ("ий", 0.9),
            (" ", 1.0),
            ("ю", 1.05),
            ("зер", 1.1),
        ]);
        let u = utterances(Speaker::Me, &chunk, &r);
        assert_eq!(u[0].text, "чи на 14, новий юзер");
        assert_eq!(u[0].words.len(), 5);
        assert!((u[0].words[2].start - 0.45).abs() < 1e-9);
    }

    /// Real tokens: `["2", "5", " <unk>", " сторони"]`.
    #[test]
    fn the_unknown_token_is_dropped() {
        let mut c = Chunker::default();
        c.push(0, &[0.0; 32_000]);
        let chunk = c.finish().unwrap();
        let r = result(&[(" 2", 0.0), ("5", 0.1), (" <unk>", 0.3), (" сторони", 0.5)]);
        let u = utterances(Speaker::Me, &chunk, &r);
        assert_eq!(u[0].text, "25 сторони");
    }

    #[test]
    fn empty_result_has_no_utterances() {
        let mut c = Chunker::default();
        c.push(0, &[0.0; 100]);
        let chunk = c.finish().unwrap();
        assert!(utterances(Speaker::Me, &chunk, &ChunkResult::default()).is_empty());
    }
}
