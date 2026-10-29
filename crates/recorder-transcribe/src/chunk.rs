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

use crate::SAMPLE_RATE;

/// At most this much speech per chunk.
pub const MAX_CHUNK_SECONDS: usize = 30;
const MAX_CHUNK_SAMPLES: usize = MAX_CHUNK_SECONDS * SAMPLE_RATE;

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
