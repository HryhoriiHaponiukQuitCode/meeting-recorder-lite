//! Lock-free hand-off from audio callbacks to the engine thread. The
//! producer side never allocates or blocks: if the engine falls behind and
//! the ring is full, the block is dropped and counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

#[derive(Clone, Copy, Debug)]
pub struct BlockMeta {
    /// Host-clock time of the first sample, nanoseconds.
    pub host_ns: u64,
    pub frames: u32,
    pub rate: u32,
}

pub struct BlockProducer {
    meta: HeapProd<BlockMeta>,
    data: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
}

/// The producer is handed from one device stream to the next when a device
/// is reopened. Only one stream exists at a time, so `try_lock` in the
/// callback never contends.
pub type SharedProducer = Arc<Mutex<BlockProducer>>;

impl BlockProducer {
    /// Queues one mono block. Returns `false` (and counts the loss) when full.
    pub fn push(&mut self, host_ns: u64, rate: u32, mono: &[f32]) -> bool {
        if mono.is_empty() {
            return true;
        }
        if self.data.vacant_len() < mono.len() || self.meta.is_full() {
            self.dropped.fetch_add(mono.len() as u64, Ordering::Relaxed);
            return false;
        }
        self.data.push_slice(mono);
        let _ = self.meta.try_push(BlockMeta {
            host_ns,
            frames: mono.len() as u32,
            rate,
        });
        true
    }

    pub fn into_shared(self) -> SharedProducer {
        Arc::new(Mutex::new(self))
    }
}

/// Pushes through a shared producer from a real-time callback.
pub fn push_shared(p: &SharedProducer, host_ns: u64, rate: u32, mono: &[f32]) {
    if let Ok(mut p) = p.try_lock() {
        p.push(host_ns, rate, mono);
    }
}

pub struct BlockConsumer {
    meta: HeapCons<BlockMeta>,
    data: HeapCons<f32>,
    dropped: Arc<AtomicU64>,
}

impl BlockConsumer {
    /// Pops the next block into `buf`.
    pub fn pop(&mut self, buf: &mut Vec<f32>) -> Option<BlockMeta> {
        let meta = self.meta.try_pop()?;
        buf.resize(meta.frames as usize, 0.0);
        let n = self.data.pop_slice(buf);
        buf.truncate(n);
        Some(meta)
    }

    /// Samples lost because the ring was full.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A ring holding `seconds` of mono audio at up to 192 kHz.
pub fn block_channel(seconds: usize) -> (BlockProducer, BlockConsumer) {
    let (mp, mc) = HeapRb::<BlockMeta>::new(4096).split();
    let (dp, dc) = HeapRb::<f32>::new(192_000 * seconds).split();
    let dropped = Arc::new(AtomicU64::new(0));
    (
        BlockProducer {
            meta: mp,
            data: dp,
            dropped: dropped.clone(),
        },
        BlockConsumer {
            meta: mc,
            data: dc,
            dropped,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_round_trip_and_overflow_is_counted() {
        let (mut p, mut c) = block_channel(1);
        assert!(p.push(10, 48_000, &[0.5; 480]));
        assert!(p.push(20, 48_000, &[0.25; 480]));
        let mut buf = Vec::new();
        let m = c.pop(&mut buf).unwrap();
        assert_eq!(
            (m.host_ns, m.frames, buf.len(), buf[0]),
            (10, 480, 480, 0.5)
        );
        let m = c.pop(&mut buf).unwrap();
        assert_eq!((m.host_ns, buf[0]), (20, 0.25));
        assert!(c.pop(&mut buf).is_none());
        assert!(!p.push(30, 48_000, &vec![0.0; 192_001]));
        assert_eq!(c.dropped_samples(), 192_001);
    }
}
