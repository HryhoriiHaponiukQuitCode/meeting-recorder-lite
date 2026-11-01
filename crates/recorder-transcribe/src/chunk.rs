//! Chunking for Parakeet, as measured in the benchmarks:
//!
//! - a whole recording at once silently drops long stretches and needs
//!   gigabytes of memory;
//! - a chunk with pauses inside loses phrases after the pauses;
//! - single short segments get words of another language (the model has no
//!   language setting).
//!
//! What works: voice-activity segments joined **without the pauses between
//! them** into chunks of at most 30 s of speech, each decoded on its own.
//! A [`Chunk`] remembers where each piece came from, so recognizer times
//! inside the chunk map back to the recording timeline.
//!
//! Each segment takes [`SEGMENT_PAD_S`] of audio from before and after it
//! ([`Padder`]): the detector cuts close to the words, and Parakeet then
//! garbles or invents words at the cut. Measured on FLEURS sentences joined
//! into 90 s clips with pauses (Parakeet fp32, greedy): Ukrainian WER 10.9%
//! → 7.4%, English 8.2% → 7.0%; 0.4 s was no better.

use crate::SAMPLE_RATE;

/// At most this much speech per chunk.
pub const MAX_CHUNK_SECONDS: usize = 30;
const MAX_CHUNK_SAMPLES: usize = MAX_CHUNK_SECONDS * SAMPLE_RATE;

/// Audio added before and after each voice-activity segment.
pub const SEGMENT_PAD_S: f64 = 0.2;

/// Widens voice-activity segments by [`SEGMENT_PAD_S`] on each side, from
/// the audio around them. Feed it every sample given to the detector, in
/// order ([`Padder::push`]), then pass each segment the detector returns
/// ([`Padder::widen`]). The padding never reaches into the previous
/// segment, and after a segment only as far as the audio already pushed
/// (a segment cut at the detector's maximum length gets none there).
#[derive(Debug)]
pub struct Padder {
    /// Recent audio; `buf[0]` is sample `base` of the signal.
    buf: Vec<f32>,
    base: u64,
    /// End of the last widened segment.
    prev_end: u64,
    pad: u64,
    keep: usize,
}

impl Padder {
    /// `keep_s`: how far back a segment can start, relative to the newest
    /// pushed sample (the detector's maximum segment length and its delay).
    pub fn new(keep_s: f64) -> Self {
        Self {
            buf: Vec::new(),
            base: 0,
            prev_end: 0,
            pad: (SEGMENT_PAD_S * SAMPLE_RATE as f64) as u64,
            keep: (keep_s * SAMPLE_RATE as f64) as usize,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        if self.buf.len() > 2 * self.keep {
            let drop = self.buf.len() - self.keep;
            self.buf.drain(..drop);
            self.base += drop as u64;
        }
    }

    /// The segment at `start` with `samples` (as the detector gave them),
    /// widened: its new start and samples.
    pub fn widen(&mut self, start: u64, samples: &[f32]) -> (u64, Vec<f32>) {
        let end = start + samples.len() as u64;
        let have = self.base + self.buf.len() as u64;
        if start < self.base || end > have {
            // Not in the kept audio (should not happen): as it is.
            self.prev_end = self.prev_end.max(end);
            return (start, samples.to_vec());
        }
        let a = start
            .saturating_sub(self.pad)
            .max(self.prev_end.min(start))
            .max(self.base);
        let b = (end + self.pad).min(have);
        self.prev_end = b;
        let (i, j) = ((a - self.base) as usize, (b - self.base) as usize);
        (a, self.buf[i..j].to_vec())
    }
}

/// One voice-activity segment inside a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    /// Sample offset inside the chunk.
    pub offset: usize,
    /// Sample position on the recording timeline.
    pub start: u64,
    pub len: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub samples: Vec<f32>,
    pub pieces: Vec<Piece>,
}

impl Chunk {
    /// Timeline sample where the chunk's first piece starts.
    pub fn start(&self) -> u64 {
        self.pieces.first().map_or(0, |p| p.start)
    }

    /// Maps a time inside the chunk (seconds) to the recording timeline
    /// (seconds). Times past a piece's end stay at that end.
    pub fn to_timeline(&self, t: f64) -> f64 {
        let pos = (t.max(0.0) * SAMPLE_RATE as f64).round() as usize;
        let piece = self
            .pieces
            .iter()
            .rev()
            .find(|p| p.offset <= pos)
            .or(self.pieces.first());
        match piece {
            Some(p) => {
                (p.start + (pos - p.offset.min(pos)).min(p.len) as u64) as f64 / SAMPLE_RATE as f64
            }
            None => t,
        }
    }
}

/// Collects segments into chunks; a chunk is handed out when the next
/// segment would not fit, and the last one by [`Chunker::finish`].
#[derive(Debug, Default)]
pub struct Chunker {
    current: Option<Chunk>,
}

impl Chunker {
    /// Adds a segment that starts at timeline sample `start`. Returns the
    /// previous chunk if this segment did not fit into it.
    pub fn push(&mut self, start: u64, samples: &[f32]) -> Option<Chunk> {
        if samples.is_empty() {
            return None;
        }
        if let Some(c) = &mut self.current
            && c.samples.len() + samples.len() <= MAX_CHUNK_SAMPLES
        {
            c.pieces.push(Piece {
                offset: c.samples.len(),
                start,
                len: samples.len(),
            });
            c.samples.extend_from_slice(samples);
            return None;
        }
        self.current.replace(Chunk {
            samples: samples.to_vec(),
            pieces: vec![Piece {
                offset: 0,
                start,
                len: samples.len(),
            }],
        })
    }

    pub fn finish(&mut self) -> Option<Chunk> {
        self.current.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> usize {
        (s * SAMPLE_RATE as f64) as usize
    }

    #[test]
    fn segments_join_without_pauses_up_to_30_s() {
        let mut c = Chunker::default();
        // 12 s at 0 s, 12 s at 20 s, 10 s at 40 s: the third does not fit.
        assert!(c.push(0, &vec![0.1; secs(12.0)]).is_none());
        assert!(c.push(secs(20.0) as u64, &vec![0.2; secs(12.0)]).is_none());
        let first = c.push(secs(40.0) as u64, &vec![0.3; secs(10.0)]).unwrap();
        assert_eq!(first.samples.len(), secs(24.0), "pauses removed");
        assert_eq!(first.pieces.len(), 2);
        assert_eq!(first.pieces[1].offset, secs(12.0));
        let second = c.finish().unwrap();
        assert_eq!(second.start(), secs(40.0) as u64);
        assert!(c.finish().is_none());
    }

    #[test]
    fn exactly_30_s_still_fits() {
        let mut c = Chunker::default();
        assert!(c.push(0, &vec![0.0; secs(20.0)]).is_none());
        assert!(c.push(secs(25.0) as u64, &vec![0.0; secs(10.0)]).is_none());
        assert_eq!(c.finish().unwrap().samples.len(), secs(30.0));
    }

    /// Signal whose sample i has the value i (exact in f32 up to 2^24).
    fn ramp(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|i| i as f32).collect()
    }

    #[test]
    fn segments_widen_by_the_pad_without_overlap() {
        let pad = secs(SEGMENT_PAD_S);
        let mut p = Padder::new(30.0);
        p.push(&ramp(0, secs(10.0)));
        // A segment from 2 s to 3 s gets 0.2 s on each side.
        let (s1, x1) = p.widen(secs(2.0) as u64, &ramp(secs(2.0), secs(1.0)));
        assert_eq!(s1, (secs(2.0) - pad) as u64);
        assert_eq!(x1.len(), secs(1.0) + 2 * pad);
        assert_eq!(x1[0], (secs(2.0) - pad) as f32, "real audio, not silence");
        // The next one starts 0.3 s after: its padding stops where the
        // previous widened segment ended (3.2 s).
        let (s2, x2) = p.widen(secs(3.3) as u64, &ramp(secs(3.3), secs(1.0)));
        assert_eq!(s2, (secs(3.0) + pad) as u64);
        assert_eq!(x2.len(), secs(4.3) + pad - (secs(3.0) + pad));
        // Starting before the previous padding ended (4.5 s): no padding
        // before, and the segment itself is never cut.
        let (s2b, _) = p.widen(secs(4.4) as u64, &ramp(secs(4.4), secs(1.0)));
        assert_eq!(s2b, secs(4.4) as u64);
        // At the end of what was pushed: no padding after.
        let (_, x3) = p.widen(secs(9.5) as u64, &ramp(secs(9.5), secs(0.5)));
        assert_eq!(*x3.last().unwrap(), (secs(10.0) - 1) as f32);
        // At the very start: no padding before.
        let mut q = Padder::new(30.0);
        q.push(&ramp(0, secs(2.0)));
        assert_eq!(q.widen(secs(0.1) as u64, &ramp(secs(0.1), secs(1.0))).0, 0);
    }

    #[test]
    fn old_audio_is_dropped_but_recent_segments_still_widen() {
        let mut p = Padder::new(1.0);
        for k in 0..10 {
            p.push(&ramp(k * secs(1.0), secs(1.0)));
        }
        assert!(p.buf.len() <= 2 * secs(1.0));
        let (s, x) = p.widen(secs(9.0) as u64, &ramp(secs(9.0), secs(0.5)));
        assert_eq!(s, (secs(9.0) - secs(SEGMENT_PAD_S)) as u64);
        assert_eq!(x[0], s as f32);
        // Too old to widen: returned as given.
        let old = ramp(secs(1.0), secs(0.5));
        assert_eq!(p.widen(secs(1.0) as u64, &old), (secs(1.0) as u64, old));
    }

    #[test]
    fn times_map_back_across_removed_pauses() {
        let mut c = Chunker::default();
        c.push(secs(5.0) as u64, &vec![0.0; secs(2.0)]);
        c.push(secs(60.0) as u64, &vec![0.0; secs(3.0)]);
        let chunk = c.finish().unwrap();
        assert_eq!(chunk.to_timeline(0.0), 5.0);
        assert_eq!(chunk.to_timeline(1.5), 6.5);
        // 2.5 s into the chunk is 0.5 s into the second piece.
        assert_eq!(chunk.to_timeline(2.5), 60.5);
        // Past the end stays at the end of the last piece.
        assert_eq!(chunk.to_timeline(99.0), 63.0);
    }
}
