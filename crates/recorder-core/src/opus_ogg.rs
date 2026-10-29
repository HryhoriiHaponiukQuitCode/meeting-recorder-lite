//! One Ogg Opus file at 16 kHz in one of two layouts:
//!
//! - [`Layout::Split`] (`part_NNNN.ogg`): two channels coded as two
//!   independent mono streams (RFC 7845 channel mapping family 1, two
//!   streams, no coupled streams). Decoders see an ordinary two-channel file,
//!   but the encoder never mixes the channels, so the microphone never leaks
//!   into the system channel or the other way round.
//! - [`Layout::Mono`] (`listen.ogg`): one channel (mapping family 0), the
//!   mix of both sides for listening.
//!
//! Crash safety: a page is closed every second and the file is `fsync`ed, so a
//! crash or power loss loses at most the last unflushed second.
//!
//! Seamless rotation: the part is closed with one extra packet carrying the
//! first frame of the next part (it flushes the encoder look-ahead), and the
//! final granule position trims the decoder output to exactly the frames that
//! belong to this part. Every timeline sample therefore lands in exactly one
//! part.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use opus::{Application, Bitrate, MSEncoder, Signal};

use crate::{CHANNELS, FRAME_SAMPLES, FRAMES_PER_SECOND};

/// Opus granule positions always count 48 kHz samples.
const GRANULE_PER_FRAME: u64 = 960;
const GRANULE_PER_SAMPLE: u64 = 3;
const MAX_PACKET: usize = 1500;

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// Total bitrate for both channels together, bits per second.
    pub bitrate: i32,
    /// libopus complexity 0..=10.
    pub complexity: i32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            bitrate: 32_000,
            complexity: 5,
        }
    }
}

fn opus_err(e: opus::Error) -> io::Error {
    io::Error::other(format!("opus: {e}"))
}

/// Channel layout of an Ogg Opus file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Microphone (L) and system audio (R) as two independent mono streams.
    Split,
    /// One mono channel.
    Mono,
}

impl Layout {
    pub fn channels(self) -> usize {
        match self {
            Layout::Split => CHANNELS,
            Layout::Mono => 1,
        }
    }
}

pub struct PartWriter {
    path: PathBuf,
    w: PacketWriter<'static, BufWriter<File>>,
    enc: MSEncoder,
    channels: usize,
    serial: u32,
    preskip: u16,
    frames: u64,
    since_flush: u64,
    buf: Vec<u8>,
}

impl PartWriter {
    /// Creates the file (it must not exist) and writes the Opus headers.
    pub fn create(
        path: &Path,
        serial: u32,
        cfg: &EncoderConfig,
        layout: Layout,
        tags: &[(&str, String)],
    ) -> io::Result<Self> {
        let mut enc = match layout {
            Layout::Split => MSEncoder::new(crate::SAMPLE_RATE, 2, 0, &[0, 1], Application::Voip),
            Layout::Mono => MSEncoder::new(crate::SAMPLE_RATE, 1, 0, &[0], Application::Voip),
        }
        .map_err(opus_err)?;
        enc.set_bitrate(Bitrate::Bits(cfg.bitrate))
            .map_err(opus_err)?;
        enc.set_vbr(true).map_err(opus_err)?;
        enc.set_complexity(cfg.complexity).map_err(opus_err)?;
        enc.set_signal(Signal::Voice).map_err(opus_err)?;
        let lookahead = enc.get_lookahead().map_err(opus_err)? as u64;
        let preskip = u16::try_from(lookahead * GRANULE_PER_SAMPLE).map_err(io::Error::other)?;

        let file = File::create_new(path)?;
        let mut w = PacketWriter::new(BufWriter::with_capacity(64 * 1024, file));
        w.write_packet(
            opus_head(preskip, layout),
            serial,
            PacketWriteEndInfo::EndPage,
            0,
        )?;
        w.write_packet(opus_tags(tags), serial, PacketWriteEndInfo::EndPage, 0)?;
        let mut part = Self {
            path: path.to_path_buf(),
            w,
            enc,
            channels: layout.channels(),
            serial,
            preskip,
            frames: 0,
            since_flush: 0,
            buf: vec![0; MAX_PACKET],
        };
        part.flush_sync()?;
        Ok(part)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frames (20 ms each) that belong to this part.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn duration_ms(&self) -> u64 {
        self.frames * 20
    }

    fn encode(&mut self, interleaved: &[f32]) -> io::Result<Vec<u8>> {
        debug_assert_eq!(interleaved.len(), FRAME_SAMPLES * self.channels);
        let n = self
            .enc
            .encode_float(interleaved, &mut self.buf)
            .map_err(opus_err)?;
        Ok(self.buf[..n].to_vec())
    }

    /// Encodes one 20 ms frame: interleaved `[L, R, L, R, ...]` for
    /// [`Layout::Split`], plain samples for [`Layout::Mono`].
    pub fn write_frame(&mut self, interleaved: &[f32]) -> io::Result<()> {
        let packet = self.encode(interleaved)?;
        self.frames += 1;
        self.since_flush += 1;
        let end_page = self.since_flush >= FRAMES_PER_SECOND;
        let info = if end_page {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        self.w
            .write_packet(packet, self.serial, info, self.frames * GRANULE_PER_FRAME)?;
        if end_page {
            self.flush_sync()?;
        }
        Ok(())
    }

    /// Closes the part. `next` is the first frame of the following part (it
    /// carries the encoder look-ahead out); `None` at the end of the session.
    pub fn finish(mut self, next: Option<&[f32]>) -> io::Result<u64> {
        let silence = [0.0f32; FRAME_SAMPLES * CHANNELS];
        let silence = &silence[..FRAME_SAMPLES * self.channels];
        let packet = self.encode(next.unwrap_or(silence))?;
        let final_granule = self.preskip as u64 + self.frames * GRANULE_PER_FRAME;
        self.w.write_packet(
            packet,
            self.serial,
            PacketWriteEndInfo::EndStream,
            final_granule,
        )?;
        self.flush_sync()?;
        Ok(self.frames)
    }

    fn flush_sync(&mut self) -> io::Result<()> {
        self.since_flush = 0;
        let inner = self.w.inner_mut();
        inner.flush()?;
        inner.get_ref().sync_data()
    }
}

fn opus_head(preskip: u16, layout: Layout) -> Vec<u8> {
    let mut h = Vec::with_capacity(23);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(layout.channels() as u8);
    h.extend_from_slice(&preskip.to_le_bytes());
    h.extend_from_slice(&crate::SAMPLE_RATE.to_le_bytes()); // original input rate
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    match layout {
        Layout::Split => {
            h.push(1); // channel mapping family 1
            h.push(2); // stream count
            h.push(0); // coupled stream count
            h.extend_from_slice(&[0, 1]); // channel 0 ← stream 0, channel 1 ← stream 1
        }
        Layout::Mono => h.push(0), // channel mapping family 0: mono, no table
    }
    h
}

fn opus_tags(tags: &[(&str, String)]) -> Vec<u8> {
    let vendor = format!(
        "meeting-recorder {} ({})",
        env!("CARGO_PKG_VERSION"),
        opus::version()
    );
    let mut t = Vec::new();
    t.extend_from_slice(b"OpusTags");
    t.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    t.extend_from_slice(vendor.as_bytes());
    t.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for (k, v) in tags {
        let c = format!("{k}={v}");
        t.extend_from_slice(&(c.len() as u32).to_le_bytes());
        t.extend_from_slice(c.as_bytes());
    }
    t
}
