//! `transcript.partial.jsonl`: one line per decoded chunk, flushed to disk,
//! so a run that was cancelled or crashed resumes where it stopped.
//!
//! Voice detection and chunking are deterministic, so a new run finds the
//! same chunks again and only decodes those without a line. The first line
//! names the engine, model and settings; if they differ, the file is
//! discarded. A half-written last line (crash) is ignored.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Speaker;
use crate::transcript::{ChunkResult, Word};

pub const PARTIAL_FILE: &str = "transcript.partial.jsonl";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Header {
    signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    speaker: Speaker,
    start: u64,
    len: usize,
    /// A Parakeet chunk (`result`) or a Whisper region (`words`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    whisper: bool,
    #[serde(default)]
    result: ChunkResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    words: Vec<Word>,
}

type Key = (Speaker, u64, usize);

pub struct Checkpoint {
    path: PathBuf,
    file: File,
    done: HashMap<Key, ChunkResult>,
    whisper: HashMap<Key, Vec<Word>>,
}

impl Checkpoint {
    /// Opens or starts the checkpoint in `dir`. `signature` identifies the
    /// engine, model and settings.
    pub fn open(dir: &Path, signature: &str) -> io::Result<Self> {
        let path = dir.join(PARTIAL_FILE);
        let header = Header {
            signature: signature.to_string(),
        };
        let mut done = HashMap::new();
        let mut whisper = HashMap::new();
        let mut valid = false;
        if let Ok(f) = File::open(&path) {
            let mut lines = BufReader::new(f).lines();
            if let Some(Ok(first)) = lines.next() {
                valid = serde_json::from_str::<Header>(&first).ok().as_ref() == Some(&header);
            }
            if valid {
                for line in lines.map_while(Result::ok) {
                    if let Ok(e) = serde_json::from_str::<Entry>(&line) {
                        if e.whisper {
                            whisper.insert((e.speaker, e.start, e.len), e.words);
                        } else {
                            done.insert((e.speaker, e.start, e.len), e.result);
                        }
                    }
                }
            }
        }
        let file = if valid {
            // Drop a half-written last line before appending.
            let data = fs::read(&path)?;
            let keep = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(keep as u64)?;
            drop(f);
            OpenOptions::new().append(true).open(&path)?
        } else {
            let mut f = File::create(&path)?;
            writeln!(
                f,
                "{}",
                serde_json::to_string(&header).map_err(io::Error::other)?
            )?;
            f.sync_data()?;
            f
        };
        Ok(Self {
            path,
            file,
            done,
            whisper,
        })
    }

    /// Chunks and regions already decoded by an earlier run.
    pub fn resumed(&self) -> usize {
        self.done.len() + self.whisper.len()
    }

    pub fn get(&self, speaker: Speaker, start: u64, len: usize) -> Option<&ChunkResult> {
        self.done.get(&(speaker, start, len))
    }

    pub fn record(
        &mut self,
        speaker: Speaker,
        start: u64,
        len: usize,
        result: &ChunkResult,
    ) -> io::Result<()> {
        let e = Entry {
            speaker,
            start,
            len,
            whisper: false,
            result: result.clone(),
            words: Vec::new(),
        };
        self.append(&e)?;
        self.done.insert((speaker, start, len), result.clone());
        Ok(())
    }

    /// Whisper's words for a region decoded by an earlier run.
    pub fn get_whisper(&self, speaker: Speaker, start: u64, len: usize) -> Option<&Vec<Word>> {
        self.whisper.get(&(speaker, start, len))
    }

    pub fn record_whisper(
        &mut self,
        speaker: Speaker,
        start: u64,
        len: usize,
        words: &[Word],
    ) -> io::Result<()> {
        let e = Entry {
            speaker,
            start,
            len,
            whisper: true,
            result: ChunkResult::default(),
            words: words.to_vec(),
        };
        self.append(&e)?;
        self.whisper.insert((speaker, start, len), words.to_vec());
        Ok(())
    }

    fn append(&mut self, e: &Entry) -> io::Result<()> {
        writeln!(
            self.file,
            "{}",
            serde_json::to_string(e).map_err(io::Error::other)?
        )?;
        self.file.sync_data()
    }

    /// Removes the file after the transcript is written.
    pub fn remove(self) -> io::Result<()> {
        drop(self.file);
        fs::remove_file(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mr-ckpt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn r(text: &str) -> ChunkResult {
        ChunkResult {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn resumes_after_a_crash_mid_line() {
        let d = dir("resume");
        let mut c = Checkpoint::open(&d, "sig").unwrap();
        c.record(Speaker::Me, 100, 50, &r("a")).unwrap();
        c.record(Speaker::Others, 100, 50, &r("b")).unwrap();
        c.record_whisper(Speaker::Others, 100, 50, &[Word::new("слово", 1.0, 1.2)])
            .unwrap();
        drop(c);
        // Crash while writing the third line.
        let mut f = OpenOptions::new()
            .append(true)
            .open(d.join(PARTIAL_FILE))
            .unwrap();
        f.write_all(b"{\"speaker\":\"me\",\"sta").unwrap();
        drop(f);

        let mut c = Checkpoint::open(&d, "sig").unwrap();
        assert_eq!(c.resumed(), 3);
        assert_eq!(c.get(Speaker::Me, 100, 50).unwrap().text, "a");
        assert_eq!(
            c.get_whisper(Speaker::Others, 100, 50).unwrap()[0].text,
            "слово"
        );
        assert!(
            c.get_whisper(Speaker::Me, 100, 50).is_none(),
            "Parakeet and Whisper are kept apart"
        );
        assert!(c.get(Speaker::Me, 100, 51).is_none());
        c.record(Speaker::Me, 900, 10, &r("c")).unwrap();
        drop(c);
        let c = Checkpoint::open(&d, "sig").unwrap();
        assert_eq!(
            c.resumed(),
            4,
            "the broken line was cut off before appending"
        );
        c.remove().unwrap();
        assert!(!d.join(PARTIAL_FILE).exists());
        fs::remove_dir_all(d).ok();
    }

    #[test]
    fn other_settings_start_over() {
        let d = dir("sig");
        let mut c = Checkpoint::open(&d, "threads=2").unwrap();
        c.record(Speaker::Me, 1, 1, &r("a")).unwrap();
        drop(c);
        assert_eq!(Checkpoint::open(&d, "model-v2").unwrap().resumed(), 0);
        assert_eq!(
            Checkpoint::open(&d, "threads=2").unwrap().resumed(),
            0,
            "the old file was replaced"
        );
        fs::remove_dir_all(d).ok();
    }
}
