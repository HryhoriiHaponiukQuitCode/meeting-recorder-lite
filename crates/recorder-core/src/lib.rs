//! Platform-independent recording core: timeline alignment with clock-drift
//! compensation, Opus/Ogg encoding with seamless part rotation, the session
//! manifest and crash recovery.
//!
//! Nothing in this crate touches audio devices, so all of it is unit-tested
//! with synthetic sources and simulated clocks.

pub mod aec;
pub mod decode;
pub mod engine;
pub mod manifest;
pub mod ogg_scan;
pub mod opus_ogg;
pub mod recovery;
pub mod session;
pub mod track;

/// Output sample rate of every recording.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per channel in one Opus frame (20 ms at 16 kHz).
pub const FRAME_SAMPLES: usize = 320;
/// Recorded channels: 0 = microphone (L), 1 = system audio (R).
pub const CHANNELS: usize = 2;
/// Opus frames per second.
pub const FRAMES_PER_SECOND: u64 = 50;

/// Index of the microphone channel (left).
pub const MIC: usize = 0;
/// Index of the system-audio channel (right).
pub const SYSTEM: usize = 1;

/// Converts a nanosecond duration to a fractional position on the 16 kHz timeline.
pub fn ns_to_frames(ns: i128) -> f64 {
    ns as f64 * SAMPLE_RATE as f64 / 1e9
}
