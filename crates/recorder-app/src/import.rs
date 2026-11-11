//! Importing an audio or video file as a recording, to transcribe it like
//! one made here.
//!
//! - The file is decoded with Symphonia (MP3, AAC/M4A, MP4/MOV video, WAV,
//!   FLAC, ALAC, Ogg Vorbis, AIFF, CAF, MKV) or, for Ogg Opus (voice
//!   messages: `.opus`, `.oga`), with our own Opus decoder. WebM is not
//!   offered: its sound is almost always Opus, which Symphonia cannot
//!   decode inside it.
//! - All channels are mixed to mono, resampled to 16 kHz and written as an
//!   ordinary session with the sound on the system-audio channel and a
//!   silent microphone: a file does not say who is who, so every voice in
//!   it is a participant ("Participant 1, 2…"), and the echo check has
//!   nothing to do.
//! - The session is built in `.import` inside the recordings folder and
//!   moved into place only when complete, so a half-imported file never
//!   shows up in the list (nor gets "recovered" after a crash). The file's
//!   name becomes the recording's name.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use recorder_core::SAMPLE_RATE;
use recorder_core::decode::PartReader;
use recorder_core::opus_ogg::EncoderConfig;
use recorder_core::session::{SessionConfig, SessionWriter};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::sessions;

/// File extensions offered in the import dialog.
pub const EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "aac", "mp4", "m4v", "mov", "wav", "flac", "ogg", "oga", "opus", "aif", "aiff",
    "caf", "mkv",
];

/// Whether `path` has one of [`EXTENSIONS`].
pub fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// An import on its own thread.
pub struct ImportJob {
    file: PathBuf,
    progress: Arc<Mutex<f32>>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<io::Result<PathBuf>>>>,
}

impl ImportJob {
    pub fn start(file: PathBuf, root: PathBuf) -> Self {
        let progress = Arc::new(Mutex::new(0.0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let (p, c, r, f) = (progress.clone(), cancel.clone(), result.clone(), file.clone());
        std::thread::Builder::new()
            .name("import".into())
            .spawn(move || {
                tracing::info!(file = %f.display(), "import started");
                let out = import(&f, &root, &mut |x| *p.lock().expect("progress") = x, &c);
                match &out {
                    Ok(dir) => tracing::info!(dir = %dir.display(), "imported"),
                    Err(e) => tracing::warn!(error = %e, "import failed"),
                }
                *r.lock().expect("result") = Some(out);
            })
            .expect("spawn the import thread");
        Self {
            file,
            progress,
            cancel,
            result,
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// 0..1 (stays 0 when the file's length is unknown).
    pub fn progress(&self) -> f32 {
        *self.progress.lock().expect("progress")
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// The outcome once finished: the new recording's folder or the error.
    pub fn poll(&self) -> Option<io::Result<PathBuf>> {
        self.result.lock().expect("result").take()
    }
}

/// Where sessions are built before they are moved into place.
const WORK_DIR: &str = ".import";
/// Samples per channel of one 20 ms frame at 16 kHz.
const FRAME: usize = SAMPLE_RATE as usize / 50;

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "import cancelled")
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Mono audio at any rate, block by block; `total_s` when the length is
/// known (for the progress).
trait Source {
    fn rate(&self) -> u32;
    fn total_s(&self) -> Option<f64>;
    /// Appends the next mono samples to `out`; `false` at the end.
    fn next(&mut self, out: &mut Vec<f32>) -> io::Result<bool>;
}

/// Ogg Opus through our own decoder (already at 16 kHz).
struct OpusSource {
    reader: PartReader,
    buf: Vec<f32>,
}

impl Source for OpusSource {
    fn rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn total_s(&self) -> Option<f64> {
        None
    }

    fn next(&mut self, out: &mut Vec<f32>) -> io::Result<bool> {
        self.buf.clear();
        let more = self.reader.read_block(&mut self.buf)?;
        let ch = self.reader.channels().max(1);
        out.extend(
            self.buf
                .chunks_exact(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32),
        );
        Ok(more)
    }
}

/// Everything else through Symphonia: the default audio track.
struct SymphoniaSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    track: u32,
    rate: u32,
    total_s: Option<f64>,
    buf: Vec<f32>,
}

impl SymphoniaSource {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| bad(format!("not an audio or video file this app can read ({e})")))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| bad("the file has no sound"))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| bad("the file has no sound"))?;
        let rate = params
            .sample_rate
            .ok_or_else(|| bad("the sound has no sample rate"))?;
        let total_s = track.num_frames.map(|n| n as f64 / rate as f64);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| bad(format!("this kind of sound is not supported ({e})")))?;
        Ok(Self {
            track: track.id,
            format,
            decoder,
            rate,
            total_s,
            buf: Vec::new(),
        })
    }
}

impl Source for SymphoniaSource {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn total_s(&self) -> Option<f64> {
        self.total_s
    }

    fn next(&mut self, out: &mut Vec<f32>) -> io::Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(false),
                // A cut-off file: keep what was read.
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(e) => return Err(bad(format!("cannot read the file ({e})"))),
            };
            if packet.track_id != self.track {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(audio) => {
                    let ch = audio.spec().channels().count().max(1);
                    self.buf.resize(audio.samples_interleaved(), 0.0);
                    audio.copy_to_slice_interleaved(&mut self.buf);
                    out.extend(
                        self.buf
                            .chunks_exact(ch)
                            .map(|f| f.iter().sum::<f32>() / ch as f32),
                    );
                    return Ok(true);
                }
                // A damaged packet: skip it, as players do.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(bad(format!("cannot decode the sound ({e})"))),
            }
        }
    }
}

fn is_ogg_opus(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("opus" | "oga" | "ogg")) && PartReader::open(path).is_ok()
}

fn open(path: &Path) -> io::Result<Box<dyn Source>> {
    if is_ogg_opus(path) {
        return Ok(Box::new(OpusSource {
            reader: PartReader::open(path)?,
            buf: Vec::new(),
        }));
    }
    Ok(Box::new(SymphoniaSource::open(path)?))
}

/// Resamples mono audio to 16 kHz in blocks.
struct To16k {
    rs: Option<Async<f32>>,
    fifo: Vec<f32>,
    out_buf: Vec<f32>,
    /// Output samples still to drop (the resampler's delay).
    skip: usize,
}

impl To16k {
    fn new(rate: u32) -> Self {
        let rs = (rate != SAMPLE_RATE).then(|| {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: None,
                oversampling_factor: 128,
                interpolation: SincInterpolationType::Cubic,
                window: WindowFunction::BlackmanHarris2,
            };
            Async::<f32>::new_sinc(
                SAMPLE_RATE as f64 / rate as f64,
                1.0,
                &params,
                (rate / 50).max(64) as usize,
                1,
                FixedAsync::Input,
            )
            .expect("valid resampler parameters")
        });
        let (out_buf, skip) = rs
            .as_ref()
            .map_or((Vec::new(), 0), |r| (vec![0.0; r.output_frames_max()], r.output_delay()));
        Self {
            rs,
            fifo: Vec::new(),
            out_buf,
            skip,
        }
    }

    /// Feeds `input`; appends what is ready at 16 kHz to `out`. `last`
    /// flushes the rest.
    fn push(&mut self, input: &[f32], last: bool, out: &mut Vec<f32>) {
        let Some(rs) = self.rs.as_mut() else {
            out.extend_from_slice(input);
            return;
        };
        self.fifo.extend_from_slice(input);
        if last {
            // Enough silence to push the tail and the delay out.
            let need = rs.input_frames_next() * 2 + rs.output_delay() * 4;
            self.fifo.resize(self.fifo.len() + need, 0.0);
        }
        let mut used = 0;
        loop {
            let need = rs.input_frames_next();
            if self.fifo.len() - used < need {
                break;
            }
            let src = InterleavedSlice::new(&self.fifo[used..used + need], 1, need)
                .expect("input adapter");
            let cap = self.out_buf.len();
            let mut dst =
                InterleavedSlice::new_mut(&mut self.out_buf, 1, cap).expect("output adapter");
            let (read, written) = rs
                .process_into_buffer(&src, &mut dst, None)
                .expect("buffers sized from the resampler's limits");
            used += read;
            let skip = self.skip.min(written);
            self.skip -= skip;
            out.extend_from_slice(&self.out_buf[skip..written]);
        }
        self.fifo.drain(..used);
    }
}

/// Removes what an interrupted import left behind.
pub fn clean(root: &Path) {
    let work = root.join(WORK_DIR);
    if work.exists() {
        let _ = fs::remove_dir_all(work);
    }
}

/// Imports `file` as a new recording in `root` and returns its folder.
/// `progress` gets 0..1 when the length is known. `cancel` stops it (the
/// error kind is `Interrupted`); nothing is left behind then.
pub fn import(
    file: &Path,
    root: &Path,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> io::Result<PathBuf> {
    let mut source = open(file)?;
    clean(root);
    let work = root.join(WORK_DIR);
    fs::create_dir_all(&work)?;
    let result = (|| -> io::Result<PathBuf> {
        let mut writer = SessionWriter::create(SessionConfig {
            root: work.clone(),
            part_seconds: 0,
            encoder: EncoderConfig::default(),
            os: crate::recorder::os_info(),
        })?;
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        writer.update_manifest(|m| {
            m.system_capture = "import".into();
            m.devices.output_kind = "none".into();
            // The microphone channel is silent: nothing can be echo.
            m.devices.echo_risk = "unlikely".into();
            m.devices.aec = "off".into();
        })?;
        writer.event("imported", serde_json::json!({ "file": name }))?;

        let mut resampler = To16k::new(source.rate());
        let total = source.total_s();
        let (mut block, mut mono, mut frame) = (Vec::new(), Vec::new(), vec![0.0f32; 2 * FRAME]);
        let mut done_in = 0usize;
        let mut written = 0usize;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(cancelled());
            }
            block.clear();
            let more = source.next(&mut block)?;
            done_in += block.len();
            resampler.push(&block, !more, &mut mono);
            let whole = mono.len() / FRAME * FRAME;
            let tail = if more { whole } else { mono.len() };
            for chunk in mono[..tail].chunks(FRAME) {
                frame.iter_mut().for_each(|s| *s = 0.0);
                for (i, &s) in chunk.iter().enumerate() {
                    // Left: the (silent) microphone; right: the file.
                    frame[2 * i + 1] = s.clamp(-1.0, 1.0);
                }
                writer.push_frame(&frame)?;
                written += 1;
            }
            mono.drain(..tail);
            if let Some(t) = total
                && t > 0.0
            {
                progress((done_in as f64 / source.rate() as f64 / t).min(1.0) as f32);
            }
            if !more {
                break;
            }
        }
        if written < 50 {
            return Err(bad("the file has less than a second of sound"));
        }
        let dir = writer.dir().to_path_buf();
        writer.finish()?;
        let dest = root.join(dir.file_name().ok_or_else(|| bad("no session folder"))?);
        fs::rename(&dir, &dest)?;
        if let Some(stem) = file.file_stem() {
            let _ = sessions::set_title(&dest, &stem.to_string_lossy());
        }
        progress(1.0);
        Ok(dest)
    })();
    let _ = fs::remove_dir_all(&work);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_core::decode::decode_part;
    use recorder_core::manifest::{Manifest, Status};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mr-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A 16-bit PCM WAV of `secs` of a 440 Hz tone at `rate` with
    /// `channels`, the second channel silent.
    fn wav(path: &Path, rate: u32, channels: u16, secs: f64) {
        let n = (rate as f64 * secs) as usize;
        let mut data = Vec::with_capacity(n * channels as usize * 2);
        for i in 0..n {
            let s = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin()
                * i16::MAX as f64) as i16;
            data.extend_from_slice(&s.to_le_bytes());
            for _ in 1..channels {
                data.extend_from_slice(&0i16.to_le_bytes());
            }
        }
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        h.extend_from_slice(&(channels * 2).to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&data);
        fs::write(path, h).unwrap();
    }

    #[test]
    fn a_wav_becomes_a_recording_with_the_sound_on_the_system_channel() {
        let root = temp("wav");
        let file = root.join("Нарада з клієнтом.wav");
        wav(&file, 44_100, 2, 3.0);
        let mut seen = Vec::new();
        let dir = import(&file, &root, &mut |p| seen.push(p), &AtomicBool::new(false)).unwrap();
        assert!(!root.join(WORK_DIR).exists());
        let m = Manifest::load(&dir).unwrap();
        assert_eq!(m.status, Status::Completed);
        assert_eq!(m.system_capture, "import");
        assert!(m.events.iter().any(|e| e.kind == "imported"));
        // 3 s at 16 kHz, give or take one frame.
        let ms: u64 = m.parts.iter().map(|p| p.duration_ms).sum();
        assert!((2960..=3040).contains(&ms), "{ms} ms");
        let part = decode_part(&dir.join(&m.parts[0].file)).unwrap();
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        assert!(rms(&part.left) < 0.001, "the microphone is silent");
        // A 0.5 tone mixed with a silent channel: 0.25 amplitude, rms ~0.18.
        let r = rms(&part.right[1600..part.right.len() - 1600]);
        assert!((0.12..0.24).contains(&r), "rms {r}");
        assert_eq!(sessions::title(&dir).as_deref(), Some("Нарада з клієнтом"));
        let info = sessions::info(&dir).unwrap();
        assert!(info.transcribable() && !info.recording);
        assert!(seen.last() == Some(&1.0));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn bad_files_and_cancelling_leave_nothing_behind() {
        let root = temp("bad");
        let junk = root.join("notes.mp3");
        fs::write(&junk, b"this is not audio at all").unwrap();
        assert!(import(&junk, &root, &mut |_| {}, &AtomicBool::new(false)).is_err());
        let short = root.join("short.wav");
        wav(&short, 16_000, 1, 0.5);
        assert!(import(&short, &root, &mut |_| {}, &AtomicBool::new(false)).is_err());
        let long = root.join("long.wav");
        wav(&long, 48_000, 1, 5.0);
        let e = import(&long, &root, &mut |_| {}, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        // Only the source files are left.
        let mut left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["long.wav", "notes.mp3", "short.wav"]);
        fs::remove_dir_all(root).ok();
    }
}
