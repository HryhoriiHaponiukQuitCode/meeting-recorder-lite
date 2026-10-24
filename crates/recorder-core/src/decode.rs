//! Decoder for our own parts, used by tests and by `recorder verify` so the
//! recordings can be checked without ffmpeg.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use ogg::reading::PacketReader;
use opus::MSDecoder;

use crate::SAMPLE_RATE;

#[derive(Debug, Default)]
pub struct DecodedPart {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub preskip: u16,
    /// The stream ended with an end-of-stream page (cleanly closed part).
    pub complete: bool,
    /// The file ended in the middle of a page (crash before recovery).
    pub truncated: bool,
}

/// Decodes a two-channel Ogg Opus file at 16 kHz, applying pre-skip and
/// end trimming exactly as a conforming player would.
pub fn decode_part(path: &Path) -> io::Result<DecodedPart> {
    let mut reader = PacketReader::new(BufReader::new(File::open(path)?));
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

    let head = reader
        .read_packet()
        .map_err(io::Error::other)?
        .ok_or_else(|| bad("empty file"))?;
    let h = &head.data;
    if h.len() < 21 || &h[..8] != b"OpusHead" || h[9] != 2 || h[18] != 1 {
        return Err(bad("not a two-channel Opus mapping-family-1 stream"));
    }
    let preskip = u16::from_le_bytes([h[10], h[11]]);
    let (streams, coupled, mapping) = (h[19], h[20], [h[21], h[22]]);
    let mut dec = MSDecoder::new(SAMPLE_RATE, streams, coupled, &mapping)
        .map_err(|e| io::Error::other(e.to_string()))?;
    reader
        .read_packet()
        .map_err(io::Error::other)?
        .ok_or_else(|| bad("missing OpusTags"))?;

    let mut out = DecodedPart {
        preskip,
        ..Default::default()
    };
    let mut pcm = vec![0.0f32; 5760 * 2];
    let mut end_granule = None;
    loop {
        let packet = match reader.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => {
                out.truncated = true;
                break;
            }
        };
        let n = dec
            .decode_float(&packet.data, &mut pcm, false)
            .map_err(|e| io::Error::other(e.to_string()))?;
        for f in pcm[..n * 2].as_chunks::<2>().0 {
            out.left.push(f[0]);
            out.right.push(f[1]);
        }
        if packet.last_in_stream() {
            out.complete = true;
            end_granule = Some(packet.absgp_page());
            break;
        }
    }
    let skip = (preskip as usize / 3).min(out.left.len());
    out.left.drain(..skip);
    out.right.drain(..skip);
    if let Some(g) = end_granule {
        let keep = (g.saturating_sub(preskip as u64) / 3) as usize;
        out.left.truncate(keep);
        out.right.truncate(keep);
    }
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
