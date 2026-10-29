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
use crate::transcript::ChunkResult;

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
    result: ChunkResult,
}

type Key = (Speaker, u64, usize);

pub struct Checkpoint {
    path: PathBuf,
    file: File,
    done: HashMap<Key, ChunkResult>,
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
        let mut valid = false;
        if let Ok(f) = File::open(&path) {
            let mut lines = BufReader::new(f).lines();
            if let Some(Ok(first)) = lines.next() {
                valid = serde_json::from_str::<Header>(&first).ok().as_ref() == Some(&header);
            }
            if valid {
                for line in lines.map_while(Result::ok) {
                    if let Ok(e) = serde_json::from_str::<Entry>(&line) {
                        done.insert((e.speaker, e.start, e.len), e.result);
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
        Ok(Self { path, file, done })
    }

    /// Chunks already decoded by an earlier run.
    pub fn resumed(&self) -> usize {
        self.done.len()
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
            result: result.clone(),
        };
        writeln!(
            self.file,
            "{}",
            serde_json::to_string(&e).map_err(io::Error::other)?
        )?;
        self.file.sync_data()?;
        self.done.insert((speaker, start, len), result.clone());
        Ok(())
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
        drop(c);
        // Crash while writing the third line.
        let mut f = OpenOptions::new()
            .append(true)
            .open(d.join(PARTIAL_FILE))
            .unwrap();
        f.write_all(b"{\"speaker\":\"me\",\"sta").unwrap();
        drop(f);

        let mut c = Checkpoint::open(&d, "sig").unwrap();
        assert_eq!(c.resumed(), 2);
        assert_eq!(c.get(Speaker::Me, 100, 50).unwrap().text, "a");
        assert!(c.get(Speaker::Me, 100, 51).is_none());
        c.record(Speaker::Me, 900, 10, &r("c")).unwrap();
        drop(c);
        let c = Checkpoint::open(&d, "sig").unwrap();
        assert_eq!(
            c.resumed(),
            3,
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
