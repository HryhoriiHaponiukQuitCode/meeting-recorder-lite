//! Audio capture sources. Each source runs on its own supervisor thread,
//! delivers mono blocks stamped with the host monotonic clock into a lock-free
//! ring buffer, and reopens its device after errors or default-device
//! changes. Recording never stops because of a device.

pub mod clock;
pub mod mic;
pub mod output;
pub mod ring;
#[cfg(windows)]
pub mod wasapi_loopback;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub use clock::host_now_ns;
pub use ring::{BlockConsumer, BlockMeta, BlockProducer, SharedProducer, block_channel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Mic,
    System,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::System => "system",
        }
    }
}

#[derive(Clone, Debug)]
pub enum CaptureEvent {
    /// A device was opened and is delivering audio.
    Opened {
        source: Source,
        device: String,
        rate: u32,
        channels: u16,
    },
    /// The device went away or changed; the supervisor is reopening.
    Lost { source: Source, reason: String },
    /// Opening failed; the supervisor retries with back-off.
    Failed {
        source: Source,
        error: String,
        permission_denied: bool,
    },
}

/// A running capture supervisor.
pub struct SourceHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SourceHandle {
    pub(crate) fn new(stop: Arc<AtomicBool>, thread: JoinHandle<()>) -> Self {
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stops capture and waits for the device to be closed.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Averages interleaved frames into `out` (mono). `out` is cleared first.
pub(crate) fn downmix<T: Copy>(
    data: &[T],
    channels: usize,
    out: &mut Vec<f32>,
    to_f32: impl Fn(T) -> f32,
) {
    out.clear();
    let channels = channels.max(1);
    let scale = 1.0 / channels as f32;
    for frame in data.chunks_exact(channels) {
        out.push(frame.iter().map(|&s| to_f32(s)).sum::<f32>() * scale);
    }
}
