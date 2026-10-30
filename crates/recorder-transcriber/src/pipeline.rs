//! Session → transcript.
//!
//! Pass 1, every quality: Parakeet on chunks of at most 30 s of speech.
//! Pass 2, Best quality (macOS): the Parakeet engine is dropped, the parts
//! are decoded again and Whisper runs on the speech regions found in pass 1;
//! the two are merged per region ([`recorder_transcribe::hybrid`]).
//!
//! Memory stays flat however long the call: the parts are decoded packet by
//! packet, each channel keeps at most one 30-second chunk (or one region of
//! at most 90 s), and decoded work keeps only words and times. Everything
//! decoded goes to the checkpoint first, so a cancelled or crashed run
//! resumes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use recorder_core::decode::PartReader;
use recorder_core::manifest::{Manifest, PartStatus, Status, TranscriptInfo, TranscriptStatus};
use recorder_transcribe::checkpoint::Checkpoint;
use recorder_transcribe::chunk::{Chunk, Chunker, MAX_CHUNK_SECONDS};
#[cfg(target_os = "macos")]
use recorder_transcribe::hybrid::{self, Region};
use recorder_transcribe::model::{self, ModelPaths, ModelStatus, Quality};
use recorder_transcribe::output::{
    self, Document, FORMAT_VERSION, Settings, Speakers, VoiceSettings,
};
use recorder_transcribe::protocol::Message;
use recorder_transcribe::transcript::{self, ChunkResult, Utterance, Word};
use recorder_transcribe::{SAMPLE_RATE, Speaker, dialog, voices};
use sherpa_onnx::VoiceActivityDetector;

use crate::engine::{Engine, VAD_WINDOW, vad_settings};
use crate::voices::Embedder;

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
    pub quality: Quality,
    /// Tell the voices of the system audio apart (Others 1, 2, …).
    pub voices: bool,
}

fn settings(opts: &Options) -> Settings {
    Settings {
        quality: opts.quality,
        threads: opts.threads,
        decoding: if opts.quality.needs_whisper() {
            "parakeet: greedy_search; whisper: beam search 5, uk, no context, word segments".into()
        } else {
            "greedy_search".into()
        },
        vad: vad_settings(),
        chunking: "speech segments joined without pauses".into(),
        max_chunk_speech_s: MAX_CHUNK_SECONDS as u32,
        voices: opts.voices.then(VoiceSettings::current),
    }
}

fn engine_name(q: Quality) -> String {
    if q.needs_whisper() {
        format!("{} + {}", model::ENGINE, model::WHISPER_ENGINE)
    } else {
        model::ENGINE.into()
    }
}

fn model_name(q: Quality) -> String {
    if q.needs_whisper() {
        format!("{} + {}", model::WHISPER_MODEL_NAME, model::MODEL_NAME)
    } else {
        model::MODEL_NAME.into()
    }
}

/// Identifies results that can be reused (threads do not change the text).
fn signature(q: Quality) -> String {
    format!(
        "{}|{}|{:?}|{}|{}",
        engine_name(q),
        model_name(q),
        vad_settings(),
        MAX_CHUNK_SECONDS,
        q.as_str()
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

fn info(
    q: Quality,
    status: TranscriptStatus,
    files: Vec<String>,
    error: Option<String>,
) -> TranscriptInfo {
    TranscriptInfo {
        status,
        updated_at_utc: Utc::now(),
        engine: engine_name(q),
        model: model_name(q),
        files,
        error,
    }
}

/// Transcribes the session in `dir`; returns the files written. `stop`
/// cancels between packets (the checkpoint keeps finished work).
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
    if !opts.quality.is_available() {
        bail!("the Best quality needs macOS; use Fast");
    }
    if model::status(&opts.model_dir, opts.quality) != ModelStatus::Ready {
        bail!("the speech model is not downloaded");
    }
    let q = opts.quality;
    let previous = manifest.transcript.clone();
    set_state(dir, Some(info(q, TranscriptStatus::Running, vec![], None)))?;
    match run(dir, &manifest, opts, report, stop) {
        Ok(files) => {
            set_state(
                dir,
                Some(info(q, TranscriptStatus::Done, files.clone(), None)),
            )?;
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
                    q,
                    TranscriptStatus::Failed,
                    vec![],
                    Some(format!("{e:#}")),
                )),
            );
            Err(e)
        }
    }
}

/// Both channels of a session on its timeline, silence for missing parts
/// (diarization checks).
pub fn load_channels(dir: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
    let manifest = Manifest::load(dir)?;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    stream_session(dir, &manifest, &AtomicBool::new(false), &mut |l, r, _| {
        left.extend_from_slice(l);
        right.extend_from_slice(r);
        Ok(())
    })?;
    Ok((left, right))
}

/// Receives `(left, right, timeline position)` blocks.
type BlockFn<'a> = dyn FnMut(&[f32], &[f32], u64) -> Result<()> + 'a;

/// Decodes the session's parts in order and hands out blocks, silence where
/// a part is missing or unreadable. Returns the number of samples per
/// channel.
fn stream_session(
    dir: &Path,
    manifest: &Manifest,
    stop: &AtomicBool,
    on_block: &mut BlockFn<'_>,
) -> Result<u64> {
    let mut fed: u64 = 0;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut block = Vec::new();
    for part in &manifest.parts {
        // Keep the timeline exact: silence where a part is missing.
        let at = part.start_offset_ms * SAMPLE_RATE as u64 / 1000;
        while fed < at {
            let n = (at - fed).min(SAMPLE_RATE as u64) as usize;
            let zeros = vec![0.0; n];
            on_block(&zeros, &zeros, fed)?;
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
            on_block(&left, &right, fed)?;
            fed += left.len() as u64;
        }
    }
    Ok(fed)
}

/// What the Parakeet pass leaves for the rest: words and speech segments.
struct ChannelWords {
    speaker: Speaker,
    words: Vec<Word>,
    /// Speech segments for the Whisper regions (macOS only).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    segments: Vec<(u64, usize)>,
}

impl Channel<'_> {
    fn into_words(self) -> ChannelWords {
        let mut segments: Vec<(u64, usize)> = self
            .decoded
            .iter()
            .flat_map(|(c, _)| c.pieces.iter().map(|p| (p.start, p.len)))
            .collect();
        segments.sort_unstable();
        ChannelWords {
            speaker: self.speaker,
            words: self
                .decoded
                .iter()
                .flat_map(|(c, r)| transcript::chunk_words(c, r))
                .collect(),
            segments,
        }
    }
}

/// Reports progress at most twice a second (or when forced).
struct Progress {
    total_s: f64,
    last: Instant,
}

impl Progress {
    fn report(&mut self, done_s: f64, force: bool, report: &mut dyn FnMut(Message)) {
        if force || self.last.elapsed().as_millis() >= 500 {
            self.last = Instant::now();
            report(Message::Progress {
                done_s: done_s.clamp(0.0, self.total_s),
                total_s: self.total_s,
            });
        }
    }
}

/// How the progress bar is split between the passes, from measured times:
/// Whisper takes about 3 times as long as Parakeet; the voice prints about
/// half as long as Parakeet (a minute per hour of call on 2 threads).
struct Shares {
    parakeet: f64,
    whisper: f64,
}

impl Shares {
    fn new(q: Quality, voices: bool) -> Self {
        let (p, w) = if q.needs_whisper() {
            (1.0, 3.0)
        } else {
            (1.0, 0.0)
        };
        let v = if voices { 0.5 } else { 0.0 };
        let total = p + w + v;
        Self {
            parakeet: p / total,
            whisper: w / total,
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
    let shares = Shares::new(opts.quality, opts.voices);
    let share = shares.parakeet;
    let after_whisper = shares.parakeet + shares.whisper;
    let mut progress = Progress {
        total_s,
        last: Instant::now(),
    };
    let mut ckpt = Some(Checkpoint::open(dir, &signature(opts.quality))?);

    // Pass 1: Parakeet. The engine is dropped before Whisper loads, so the
    // two models never sit in memory together.
    let channels = {
        let engine = Engine::load(&ModelPaths::in_dir(&opts.model_dir), opts.threads)?;
        report(Message::Started { audio_s: total_s });
        let mut me = Channel::new(&engine, Speaker::Me)?;
        let mut others = Channel::new(&engine, Speaker::Others)?;
        stream_session(dir, manifest, stop, &mut |l, r, pos| {
            let before = me.chunks() + others.chunks();
            me.feed(l, &mut ckpt)?;
            others.feed(r, &mut ckpt)?;
            let s = (pos + l.len() as u64) as f64 / SAMPLE_RATE as f64;
            progress.report(s * share, me.chunks() + others.chunks() > before, report);
            Ok(())
        })?;
        me.finish(&mut ckpt)?;
        others.finish(&mut ckpt)?;
        [me.into_words(), others.into_words()]
    };
    progress.report(total_s * share, true, report);
    let others_segments = channels[1].segments.clone();

    // Pass 2 (Best): Whisper on the speech regions, merged with Parakeet.
    let words: Vec<(Speaker, Vec<Word>)> = if opts.quality.needs_whisper() {
        whisper_pass(
            dir,
            manifest,
            opts,
            &channels,
            total_samples,
            &mut ckpt,
            &mut |done| progress.report(total_s * (share + shares.whisper * done), false, report),
            stop,
        )?
    } else {
        channels.into_iter().map(|c| (c.speaker, c.words)).collect()
    };
    progress.report(total_s * after_whisper, true, report);

    // Pass 3: the voices of the system audio.
    let mut words = words;
    let others_voices = if opts.voices {
        let others = &mut words[Speaker::Others.channel()].1;
        voice_pass(
            dir,
            manifest,
            opts,
            &others_segments,
            others,
            &mut |done| {
                progress.report(
                    total_s * (after_whisper + (1.0 - after_whisper) * done),
                    false,
                    report,
                )
            },
            stop,
        )?
    } else {
        0
    };
    progress.report(total_s, true, report);

    let mut per: Vec<Vec<Utterance>> = words
        .into_iter()
        .map(|(sp, w)| transcript::group(sp, w))
        .collect();
    let others = per.pop().unwrap_or_default();
    let me = per.pop().unwrap_or_default();
    let utterances = dialog::merge(me, others, echo_possible(manifest));
    let doc = Document {
        format_version: FORMAT_VERSION,
        session_id: manifest.session_id.to_string(),
        created_at_utc: Utc::now(),
        engine: engine_name(opts.quality),
        model: model_name(opts.quality),
        settings: settings(opts),
        audio_s: total_s,
        processing_s: started.elapsed().as_secs_f64(),
        speakers: Speakers::default(),
        others_voices,
        participants: output::participants(&utterances),
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

/// Voice prints of the system audio's speech windows (the session decoded
/// again; only the current window is kept), clustered into voices; sets
/// `voice` on `words`. Returns the number of voices told apart (0 for one).
#[allow(clippy::too_many_arguments)]
fn voice_pass(
    dir: &Path,
    manifest: &Manifest,
    opts: &Options,
    segments: &[(u64, usize)],
    words: &mut [Word],
    progress: &mut dyn FnMut(f64),
    stop: &AtomicBool,
) -> Result<u32> {
    let wins = voices::windows(segments);
    if wins.len() < 2 || words.is_empty() {
        return Ok(0);
    }
    let embedder = Embedder::load(&ModelPaths::in_dir(&opts.model_dir).voice, opts.threads)?;
    let (kept, prints) = voice_prints(&embedder, &wins, progress, &mut |on_block| {
        stream_session(dir, manifest, stop, &mut |_, r, pos| on_block(r, pos))
    })?;
    Ok(voices::assign(words, &kept, &prints))
}

/// Receives `(samples, timeline position)` blocks of one channel.
type ChannelFn<'a> = dyn FnMut(&[f32], u64) -> Result<()> + 'a;

/// Windows `(start, length)` that got a print, and the prints.
type Prints = (Vec<(u64, usize)>, Vec<Vec<f32>>);

/// Prints of `wins` (sorted) from a stream of one channel's blocks; windows
/// too short for the model are left out. Returns the windows kept and
/// their prints.
fn voice_prints(
    embedder: &Embedder,
    wins: &[(u64, usize)],
    progress: &mut dyn FnMut(f64),
    stream: &mut dyn FnMut(&mut ChannelFn<'_>) -> Result<u64>,
) -> Result<Prints> {
    let (mut kept, mut prints) = (Vec::new(), Vec::new());
    let mut buf: Vec<f32> = Vec::new();
    let mut base: u64 = 0;
    let mut next = 0;
    stream(&mut |r, pos| {
        if buf.is_empty() {
            base = pos;
        }
        buf.extend_from_slice(r);
        while next < wins.len() {
            let (s, len) = wins[next];
            if s < base {
                next += 1;
                continue;
            }
            if s + len as u64 > base + buf.len() as u64 {
                break;
            }
            let at = (s - base) as usize;
            if let Some(p) = embedder.embed(&buf[at..at + len]) {
                kept.push(wins[next]);
                prints.push(p);
            }
            next += 1;
            progress(next as f64 / wins.len() as f64);
        }
        // Keep only what the next window still needs.
        let keep_from = wins.get(next).map_or(base + buf.len() as u64, |w| w.0);
        let drop = (keep_from.saturating_sub(base) as usize).min(buf.len());
        buf.drain(..drop);
        base += drop as u64;
        Ok(())
    })?;
    Ok((kept, prints))
}

/// Tells the voices apart in an existing transcript (checks and older
/// transcripts): the system audio is decoded again, its speech found with
/// the same VAD, and the files rewritten. Returns the number of voices.
pub fn relabel_voices(dir: &Path, opts: &Options) -> Result<u32> {
    let manifest = Manifest::load(dir)?;
    let path = dir.join(output::JSON_FILE);
    let mut doc: Document = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("no transcript in {}", dir.display()))?,
    )?;
    let (_, right) = load_channels(dir)?;
    let paths = ModelPaths::in_dir(&opts.model_dir);
    let segments = crate::engine::speech_segments(&paths.vad, &right)?;
    let mut me = Vec::new();
    let mut others: Vec<Word> = Vec::new();
    for u in doc.utterances.drain(..) {
        match u.speaker {
            Speaker::Me => me.push(Utterance { voice: None, ..u }),
            Speaker::Others => others.extend(u.words),
        }
    }
    let wins = voices::windows(&segments);
    let embedder = Embedder::load(&paths.voice, opts.threads)?;
    let (kept, prints) = voice_prints(&embedder, &wins, &mut |_| {}, &mut |on_block| {
        for (i, block) in right.chunks(SAMPLE_RATE).enumerate() {
            on_block(block, (i * SAMPLE_RATE) as u64)?;
        }
        Ok(right.len() as u64)
    })?;
    doc.others_voices = voices::assign(&mut others, &kept, &prints);
    doc.settings.voices = Some(VoiceSettings::current());
    doc.utterances = dialog::merge(
        me,
        transcript::group(Speaker::Others, others),
        echo_possible(&manifest),
    );
    doc.participants = output::participants(&doc.utterances);
    output::write_all(dir, &doc).context("cannot write the transcript")?;
    Ok(doc.others_voices)
}

/// Merges per region; Parakeet words outside every region are kept as they are.
#[cfg(target_os = "macos")]
fn merge_regions(parakeet: &[Word], regions: &[Region], whisper: &[Vec<Word>]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut used = vec![false; parakeet.len()];
    for (r, w) in regions.iter().zip(whisper) {
        let mut p = Vec::new();
        for (i, x) in parakeet.iter().enumerate() {
            if !used[i] && x.start >= r.start_s() - 0.5 && x.start < r.end_s() + 0.5 {
                used[i] = true;
                p.push(x.clone());
            }
        }
        out.extend(hybrid::merge(&p, w));
    }
    let rest: Vec<Word> = parakeet
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(w, _)| w.clone())
        .collect();
    if !rest.is_empty() {
        out.extend(rest);
        out.sort_by(|a, b| a.start.total_cmp(&b.start));
    }
    out
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn whisper_pass(
    dir: &Path,
    manifest: &Manifest,
    opts: &Options,
    channels: &[ChannelWords; 2],
    total_samples: u64,
    ckpt: &mut Option<Checkpoint>,
    progress: &mut dyn FnMut(f64),
    stop: &AtomicBool,
) -> Result<Vec<(Speaker, Vec<Word>)>> {
    use crate::whisper::WhisperEngine;

    let whisper = WhisperEngine::load(&ModelPaths::in_dir(&opts.model_dir).whisper, opts.threads)?;
    let regions: Vec<Vec<Region>> = channels
        .iter()
        .map(|c| hybrid::regions(&c.segments, total_samples))
        .collect();
    let work: f64 = regions
        .iter()
        .flatten()
        .map(|r| r.len() as f64)
        .sum::<f64>()
        .max(1.0);
    let mut done_work = 0.0;
    let mut results: Vec<Vec<Vec<Word>>> =
        regions.iter().map(|r| vec![Vec::new(); r.len()]).collect();
    // Per channel: the region being filled and its samples.
    let mut next = [0usize; 2];
    let mut bufs: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let mut finish_region =
        |ch: usize, k: usize, samples: &[f32], ckpt: &mut Option<Checkpoint>| -> Result<()> {
            let r = regions[ch][k];
            let sp = channels[ch].speaker;
            let known = ckpt
                .as_ref()
                .and_then(|c| c.get_whisper(sp, r.start, r.len()).cloned());
            let words = match known {
                Some(w) => w,
                None => {
                    let w = whisper.transcribe(samples, r.start_s())?;
                    if let Some(c) = ckpt {
                        c.record_whisper(sp, r.start, r.len(), &w)
                            .context("cannot write the checkpoint")?;
                    }
                    w
                }
            };
            results[ch][k] = words;
            done_work += r.len() as f64;
            progress(done_work / work);
            Ok(())
        };
    stream_session(dir, manifest, stop, &mut |l, r, pos| {
        let end = pos + l.len() as u64;
        for (ch, block) in [l, r].into_iter().enumerate() {
            while let Some(reg) = regions[ch].get(next[ch]).copied() {
                if reg.start >= end {
                    break;
                }
                let a = reg.start.max(pos);
                let b = reg.end.min(end);
                if a < b {
                    bufs[ch].extend_from_slice(&block[(a - pos) as usize..(b - pos) as usize]);
                }
                if reg.end > end {
                    break; // continues in the next block
                }
                let samples = std::mem::take(&mut bufs[ch]);
                finish_region(ch, next[ch], &samples, ckpt)?;
                next[ch] += 1;
            }
        }
        Ok(())
    })?;
    // Regions running to the very end of the recording.
    for ch in 0..2 {
        while next[ch] < regions[ch].len() {
            let samples = std::mem::take(&mut bufs[ch]);
            finish_region(ch, next[ch], &samples, ckpt)?;
            next[ch] += 1;
        }
    }
    Ok(channels
        .iter()
        .enumerate()
        .map(|(ch, c)| {
            (
                c.speaker,
                merge_regions(&c.words, &regions[ch], &results[ch]),
            )
        })
        .collect())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn whisper_pass(
    _dir: &Path,
    _manifest: &Manifest,
    _opts: &Options,
    _channels: &[ChannelWords; 2],
    _total_samples: u64,
    _ckpt: &mut Option<Checkpoint>,
    _progress: &mut dyn FnMut(f64),
    _stop: &AtomicBool,
) -> Result<Vec<(Speaker, Vec<Word>)>> {
    bail!("the Best quality needs macOS")
}

/// The Best quality on one mono signal (checks and `wav --quality best`):
/// Parakeet words, Whisper per region, merged.
#[cfg(target_os = "macos")]
pub fn hybrid_mono(
    engine: &Engine,
    whisper: &crate::whisper::WhisperEngine,
    speaker: Speaker,
    samples: &[f32],
) -> Result<Vec<Word>> {
    let ch = transcribe_mono(engine, speaker, samples)?.into_words();
    let regions = hybrid::regions(&ch.segments, samples.len() as u64);
    let mut results = Vec::with_capacity(regions.len());
    for r in &regions {
        results.push(whisper.transcribe(&samples[r.start as usize..r.end as usize], r.start_s())?);
    }
    Ok(merge_regions(&ch.words, &regions, &results))
}
