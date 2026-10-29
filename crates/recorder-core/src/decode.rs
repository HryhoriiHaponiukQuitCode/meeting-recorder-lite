//! Decoder for our own parts, used by tests, by `recorder verify` (so the
//! recordings can be checked without ffmpeg) and by the transcriber.
//!
//! [`PartReader`] streams a file packet by packet, so memory does not grow
//! with the length of the call; [`decode_part`] collects a whole file.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use ogg::reading::PacketReader;
use opus::MSDecoder;

use crate::SAMPLE_RATE;

#[derive(Debug, Default)]
pub struct DecodedPart {
    /// The microphone, or the only channel of a mono file.
    pub left: Vec<f32>,
    /// System audio; empty for a mono file (`listen.ogg`).
    pub right: Vec<f32>,
    pub preskip: u16,
    /// The stream ended with an end-of-stream page (cleanly closed part).
    pub complete: bool,
    /// The file ended in the middle of a page (crash before recovery).
    pub truncated: bool,
}

/// Opus granule positions count 48 kHz samples; we decode at 16 kHz.
const GRANULE_PER_SAMPLE: u64 = 3;

/// Streams one of our Ogg Opus files at 16 kHz (a two-channel part or the
/// mono `listen.ogg`), applying pre-skip and end trimming exactly as a
/// conforming player would.
///
/// The samples of the last decoded packet are held back until the next
/// packet shows whether the stream ends there, because the final granule
/// position may trim them.
pub struct PartReader {
    reader: PacketReader<BufReader<File>>,
    dec: MSDecoder,
    channels: usize,
    preskip: u16,
    /// Samples per channel of pre-skip still to drop.
    skip_left: usize,
    pcm: Vec<f32>,
    /// Interleaved samples of the last packet, not yet handed out.
    held: Vec<f32>,
    /// Samples per channel handed out so far.
    emitted: u64,
    done: bool,
    complete: bool,
    truncated: bool,
}

impl PartReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut reader = PacketReader::new(BufReader::new(File::open(path)?));
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

        let head = reader
            .read_packet()
            .map_err(io::Error::other)?
            .ok_or_else(|| bad("empty file"))?;
        let h = &head.data;
        if h.len() < 19 || &h[..8] != b"OpusHead" {
            return Err(bad("not an Opus stream"));
        }
        let channels = h[9] as usize;
        let (streams, coupled, mapping) = match (channels, h[18]) {
            (1, 0) => (1, 0, vec![0]),
            (2, 1) if h.len() >= 23 => (h[19], h[20], vec![h[21], h[22]]),
            _ => {
                return Err(bad(
                    "neither a mono nor a two-channel mapping-family-1 stream",
                ));
            }
        };
        let preskip = u16::from_le_bytes([h[10], h[11]]);
        let dec = MSDecoder::new(SAMPLE_RATE, streams, coupled, &mapping)
            .map_err(|e| io::Error::other(e.to_string()))?;
        reader
            .read_packet()
            .map_err(io::Error::other)?
            .ok_or_else(|| bad("missing OpusTags"))?;
        Ok(Self {
            reader,
            dec,
            channels,
            preskip,
            skip_left: (preskip as u64 / GRANULE_PER_SAMPLE) as usize,
            pcm: vec![0.0; 5760 * channels],
            held: Vec::new(),
            emitted: 0,
            done: false,
            complete: false,
            truncated: false,
        })
    }

    /// 1 (mono) or 2 (left = microphone, right = system audio).
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn preskip(&self) -> u16 {
        self.preskip
    }

    /// The stream ended with an end-of-stream page. Known at the end.
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// The file ended in the middle of a page. Known at the end.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Appends the next interleaved samples (at most one packet) to `out`.
    /// Returns `false` once the file is exhausted.
    pub fn read_block(&mut self, out: &mut Vec<f32>) -> io::Result<bool> {
        if self.done {
            return Ok(false);
        }
        let packet = match self.reader.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(self.finish(out, None)),
            Err(_) => {
                self.truncated = true;
                return Ok(self.finish(out, None));
            }
        };
        let n = self
            .dec
            .decode_float(&packet.data, &mut self.pcm, false)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let skip = self.skip_left.min(n);
        self.skip_left -= skip;
        let fresh = &self.pcm[skip * self.channels..n * self.channels];
        if packet.last_in_stream() {
            self.complete = true;
            self.held.extend_from_slice(fresh);
            return Ok(self.finish(out, Some(packet.absgp_page())));
        }
        out.extend_from_slice(&self.held);
        self.emitted += (self.held.len() / self.channels) as u64;
        self.held.clear();
        self.held.extend_from_slice(fresh);
        Ok(true)
    }

    /// Hands out the held samples, trimmed to the final granule if known.
    fn finish(&mut self, out: &mut Vec<f32>, end_granule: Option<u64>) -> bool {
        self.done = true;
        let mut held = std::mem::take(&mut self.held);
        if let Some(g) = end_granule {
            let keep = g.saturating_sub(self.preskip as u64) / GRANULE_PER_SAMPLE;
            let allowed = keep.saturating_sub(self.emitted) as usize * self.channels;
            held.truncate(allowed);
        }
        self.emitted += (held.len() / self.channels) as u64;
        out.extend_from_slice(&held);
        !held.is_empty()
    }
}

/// Decodes a whole file; see [`PartReader`].
pub fn decode_part(path: &Path) -> io::Result<DecodedPart> {
    let mut reader = PartReader::open(path)?;
    let mut out = DecodedPart {
        preskip: reader.preskip(),
        ..Default::default()
    };
    let mut block = Vec::new();
    while reader.read_block(&mut block)? {
        if reader.channels() == 1 {
            out.left.extend_from_slice(&block);
        } else {
            for f in block.as_chunks::<2>().0 {
                out.left.push(f[0]);
                out.right.push(f[1]);
            }
        }
        block.clear();
    }
    out.complete = reader.complete();
    out.truncated = reader.truncated();
    Ok(out)
}

/// Writes mono 16-bit PCM WAV at 16 kHz.
pub fn write_wav(path: &Path, samples: &[f32]) -> io::Result<()> {
    let mut w = io::BufWriter::new(File::create(path)?);
    let data_len = (samples.len() * 2) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&SAMPLE_RATE.to_le_bytes())?;
    w.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        w.write_all(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())?;
    }
    w.flush()
}

/// RMS level in dBFS (−inf for digital silence is reported as −120).
pub fn rms_dbfs(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -120.0;
    }
    let ms = samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64;
    if ms <= 1e-12 {
        -120.0
    } else {
        10.0 * ms.log10()
    }
}
