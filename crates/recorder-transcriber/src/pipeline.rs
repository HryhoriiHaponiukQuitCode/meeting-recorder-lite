//! Session → transcript.
//!
//! Memory stays flat however long the call: the parts are decoded packet by
//! packet, each channel keeps at most one 30-second chunk, and decoded
//! chunks keep only their text and timing. Every decoded chunk goes to the
//! checkpoint first, so a cancelled or crashed run resumes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use recorder_core::decode::PartReader;
use recorder_core::manifest::{Manifest, PartStatus, Status, TranscriptInfo, TranscriptStatus};
use recorder_transcribe::checkpoint::Checkpoint;
use recorder_transcribe::chunk::{Chunk, Chunker, MAX_CHUNK_SECONDS};
use recorder_transcribe::model::{self, ModelPaths, ModelStatus};
use recorder_transcribe::output::{self, Document, FORMAT_VERSION, Settings, Speakers};
use recorder_transcribe::protocol::Message;
use recorder_transcribe::transcript::{self, ChunkResult, Utterance};
use recorder_transcribe::{SAMPLE_RATE, Speaker, dialog};
use sherpa_onnx::VoiceActivityDetector;

use crate::engine::{Engine, VAD_WINDOW, vad_settings};

/// Reported when the run was stopped on request.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transcription cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// One channel: voice detection, chunking, recognition.
pub struct Channel<'e> {
    engine: &'e Engine,
    speaker: Speaker,
    vad: VoiceActivityDetector,
    /// Samples not yet making up a full VAD window.
    pending: Vec<f32>,
    chunker: Chunker,
    /// Decoded chunks (timing only, samples dropped) and their results.
    decoded: Vec<(Chunk, ChunkResult)>,
}

impl<'e> Channel<'e> {
    pub fn new(engine: &'e Engine, speaker: Speaker) -> Result<Self> {
        Ok(Self {
            engine,
            speaker,
            vad: engine.vad()?,
            pending: Vec::with_capacity(VAD_WINDOW * 2),
            chunker: Chunker::default(),
            decoded: Vec::new(),
        })
    }

    pub fn feed(&mut self, samples: &[f32], ckpt: &mut Option<Checkpoint>) -> Result<()> {
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(samples);
        let full = pending.len() / VAD_WINDOW * VAD_WINDOW;
        let mut result = Ok(());
        for window in pending[..full].as_chunks::<VAD_WINDOW>().0 {
            self.vad.accept_waveform(window);
            result = self.take_segments(ckpt);
            if result.is_err() {
                break;
            }
        }
        pending.drain(..full);
        self.pending = pending;
        result
    }

    /// End of audio: the incomplete last window is dropped (under 32 ms),
    /// as in the benchmarks.
    pub fn finish(&mut self, ckpt: &mut Option<Checkpoint>) -> Result<()> {
        self.vad.flush();
        self.take_segments(ckpt)?;
        if let Some(chunk) = self.chunker.finish() {
            self.decode(chunk, ckpt)?;
        }
        Ok(())
    }

    fn take_segments(&mut self, ckpt: &mut Option<Checkpoint>) -> Result<()> {
        while let Some(seg) = self.vad.front() {
            let (start, s) = (seg.start() as u64, seg.samples().to_vec());
            drop(seg);
            self.vad.pop();
            if let Some(chunk) = self.chunker.push(start, &s) {
                self.decode(chunk, ckpt)?;
            }
        }
        Ok(())
    }

    fn decode(&mut self, mut chunk: Chunk, ckpt: &mut Option<Checkpoint>) -> Result<()> {
        let (start, len) = (chunk.start(), chunk.samples.len());
        let known = ckpt
            .as_ref()
            .and_then(|c| c.get(self.speaker, start, len).cloned());
        let result = match known {
            Some(r) => r,
            None => {
                let r = self.engine.decode(&chunk.samples)?;
                if let Some(c) = ckpt {
                    c.record(self.speaker, start, len, &r)
                        .context("cannot write the checkpoint")?;
                }
                r
            }
        };
        chunk.samples = Vec::new();
        self.decoded.push((chunk, result));
        Ok(())
    }

    pub fn chunks(&self) -> usize {
        self.decoded.len()
    }

    /// The recognized text, chunks joined by a space (as in the benchmarks).
    pub fn text(&self) -> String {
        self.decoded
            .iter()
            .map(|(_, r)| r.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn utterances(&self) -> Vec<Utterance> {
        self.decoded
            .iter()
            .flat_map(|(c, r)| transcript::utterances(self.speaker, c, r))
            .collect()
    }
}

/// Transcribes one mono signal (tests and `meeting-recorder-transcribe wav`).
pub fn transcribe_mono<'e>(
    engine: &'e Engine,
    speaker: Speaker,
    samples: &[f32],
) -> Result<Channel<'e>> {
    let mut ch = Channel::new(engine, speaker)?;
    let mut none = None;
    ch.feed(samples, &mut none)?;
    ch.finish(&mut none)?;
    Ok(ch)
}

pub struct Options {
    pub model_dir: PathBuf,
    pub threads: u32,
}

fn settings(threads: u32) -> Settings {
    Settings {
        threads,
        decoding: "greedy_search".into(),
        vad: vad_settings(),
        chunking: "speech segments joined without pauses".into(),
        max_chunk_speech_s: MAX_CHUNK_SECONDS as u32,
    }
}

/// Identifies results that can be reused (threads do not change the text).
fn signature() -> String {
    format!(
        "{}|{}|{:?}|{}",
        model::ENGINE,
        model::MODEL_NAME,
        vad_settings(),
        MAX_CHUNK_SECONDS
    )
}

/// The microphone can hear the other side unless the whole call went to
/// headphones with echo cancellation off. Old manifests without the device
/// fields count as possible.
fn echo_possible(m: &Manifest) -> bool {
    m.devices.echo_risk != "unlikely"
        || m.devices.aec == "on"
        || m.events.iter().any(|e| e.kind == "aec")
}

fn set_state(dir: &Path, state: Option<TranscriptInfo>) -> Result<()> {
    let mut m = Manifest::load(dir)?;
    m.transcript = state;
    m.save(dir).context("cannot update manifest.json")
}

fn info(status: TranscriptStatus, files: Vec<String>, error: Option<String>) -> TranscriptInfo {
    TranscriptInfo {
        status,
        updated_at_utc: Utc::now(),
        engine: model::ENGINE.into(),
        model: model::MODEL_NAME.into(),
        files,
        error,
    }
}

/// Transcribes the session in `dir`; returns the files written. `stop`
/// cancels between packets (the checkpoint keeps finished chunks).
pub fn transcribe_session(
    dir: &Path,
    opts: &Options,
    report: &mut dyn FnMut(Message),
    stop: &AtomicBool,
) -> Result<Vec<String>> {
    let manifest =
        Manifest::load(dir).with_context(|| format!("no recording in {}", dir.display()))?;
    if manifest.status == Status::Recording {
        bail!("this recording is still in progress");
    }
    if manifest.parts.is_empty() {
        bail!("the recording has no audio");
    }
    if !manifest.parts.iter().any(|p| dir.join(&p.file).is_file()) {
        bail!("the audio files of this recording are missing (deleted or moved?)");
    }
    if model::status(&opts.model_dir) != ModelStatus::Ready {
        bail!("the speech model is not downloaded");
    }
    let previous = manifest.transcript.clone();
    set_state(dir, Some(info(TranscriptStatus::Running, vec![], None)))?;
    match run(dir, &manifest, opts, report, stop) {
        Ok(files) => {
            set_state(dir, Some(info(TranscriptStatus::Done, files.clone(), None)))?;
            Ok(files)
        }
        Err(e) if e.is::<Cancelled>() => {
            let _ = set_state(dir, previous);
            Err(e)
        }
        Err(e) => {
            let _ = set_state(
                dir,
                Some(info(
                    TranscriptStatus::Failed,
                    vec![],
                    Some(format!("{e:#}")),
                )),
            );
            Err(e)
        }
    }
}

fn run(
    dir: &Path,
    manifest: &Manifest,
    opts: &Options,
    report: &mut dyn FnMut(Message),
    stop: &AtomicBool,
) -> Result<Vec<String>> {
    let started = Instant::now();
    let total_samples: u64 = manifest
        .parts
        .iter()
        .map(|p| p.start_offset_ms + p.duration_ms)
        .max()
        .unwrap_or(0)
        * SAMPLE_RATE as u64
        / 1000;
    let total_s = total_samples as f64 / SAMPLE_RATE as f64;

    let engine = Engine::load(&ModelPaths::in_dir(&opts.model_dir), opts.threads)?;
    let mut ckpt = Some(Checkpoint::open(dir, &signature())?);
    report(Message::Started { audio_s: total_s });

    let mut me = Channel::new(&engine, Speaker::Me)?;
    let mut others = Channel::new(&engine, Speaker::Others)?;
    let mut fed: u64 = 0;
    let mut last_report = Instant::now();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut block = Vec::new();
    let mut progress = |fed: u64, force: bool, report: &mut dyn FnMut(Message)| {
        if force || last_report.elapsed().as_millis() >= 500 {
            last_report = Instant::now();
            report(Message::Progress {
                done_s: (fed as f64 / SAMPLE_RATE as f64).min(total_s),
                total_s,
            });
        }
    };

    for part in &manifest.parts {
        // Keep the timeline exact: silence where a part is missing.
        let at = part.start_offset_ms * SAMPLE_RATE as u64 / 1000;
        while fed < at {
            let n = (at - fed).min(SAMPLE_RATE as u64) as usize;
            let zeros = vec![0.0; n];
            me.feed(&zeros, &mut ckpt)?;
            others.feed(&zeros, &mut ckpt)?;
            fed += n as u64;
        }
        // A missing or unreadable part stays silence; the rest is transcribed.
        if part.status == PartStatus::Unreadable || !dir.join(&part.file).is_file() {
            continue;
        }
        let mut reader = PartReader::open(&dir.join(&part.file))
            .with_context(|| format!("cannot read {}", part.file))?;
        if reader.channels() != 2 {
            return Err(anyhow!("{} is not a two-channel recording", part.file));
        }
        while reader.read_block(&mut block)? {
            if stop.load(Ordering::SeqCst) {
                return Err(Cancelled.into());
            }
            left.clear();
            right.clear();
            for f in block.as_chunks::<2>().0 {
                left.push(f[0]);
                right.push(f[1]);
            }
            block.clear();
            let before = me.chunks() + others.chunks();
            me.feed(&left, &mut ckpt)?;
            others.feed(&right, &mut ckpt)?;
            fed += left.len() as u64;
            progress(fed, me.chunks() + others.chunks() > before, report);
        }
    }
    me.finish(&mut ckpt)?;
    others.finish(&mut ckpt)?;
    progress(fed, true, report);

    let utterances = dialog::merge(
        me.utterances(),
        others.utterances(),
        echo_possible(manifest),
    );
    let doc = Document {
        format_version: FORMAT_VERSION,
        session_id: manifest.session_id.to_string(),
        created_at_utc: Utc::now(),
        engine: model::ENGINE.into(),
        model: model::MODEL_NAME.into(),
        settings: settings(opts.threads),
        audio_s: total_s,
        processing_s: started.elapsed().as_secs_f64(),
        speakers: Speakers::default(),
        utterances,
    };
    let files = output::write_all(dir, &doc).context("cannot write the transcript")?;
    if let Some(c) = ckpt.take() {
        let _ = c.remove();
    }
    report(Message::Done {
        files: files.clone(),
        processing_s: doc.processing_s,
    });
    Ok(files)
}
