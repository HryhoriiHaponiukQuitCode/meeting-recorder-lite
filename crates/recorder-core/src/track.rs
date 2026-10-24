//! One capture source placed on the session timeline.
//!
//! A source delivers blocks of mono samples at its own rate, each stamped with
//! the host-clock time of its first sample. `Track` resamples them to 16 kHz
//! and keeps the output locked to the host clock:
//!
//! * small timing errors (clock drift between the device crystal and the host
//!   clock) are removed smoothly by a PI controller that nudges the resampling
//!   ratio by at most ±0.5 %;
//! * large errors are gaps (WASAPI loopback delivers nothing while the system
//!   is silent, a device is being reconnected, a tap is idle) and are filled
//!   with silence at once; data that arrives too early is dropped.
//!
//! The writer pulls fixed windows from the track with [`Track::take`]; any part
//! of the window the source did not deliver reads as silence.

use std::collections::VecDeque;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::SAMPLE_RATE;

/// A block arriving later than expected by more than this is a gap → silence.
const GAP_FRAMES: f64 = 0.040 * SAMPLE_RATE as f64;
/// A block arriving earlier than expected by more than this → drop samples.
const LATE_FRAMES: f64 = 0.060 * SAMPLE_RATE as f64;
/// Gaps above this are not pushed through the resampler sample by sample.
const DIRECT_GAP_FRAMES: f64 = 0.5 * SAMPLE_RATE as f64;
/// Longest silence inserted in one go while unconsumed audio is still queued.
const MAX_QUEUED_GAP_FRAMES: usize = 10 * SAMPLE_RATE as usize;

/// Time constant of the timing-error low-pass filter.
const ERR_TAU_S: f64 = 2.0;
/// Proportional term: an error of N frames is corrected over this many seconds.
const KP_TIME_S: f64 = 8.0;
/// Integral time; with `KP_TIME_S` gives an over-damped loop (ζ ≈ 1.4).
const KI_TIME_S: f64 = 64.0;
/// Largest total ratio correction (5000 ppm, far above real crystal drift).
const MAX_CORRECTION: f64 = 0.005;
/// Largest integral contribution (anti-windup).
const MAX_INTEGRAL: f64 = 0.003;

/// Counters for logs and the manifest.
#[derive(Clone, Debug, Default)]
pub struct TrackStats {
    /// Silence inserted for gaps, in 16 kHz frames.
    pub inserted_frames: u64,
    /// Input dropped because it arrived too early or after the commit horizon, in 16 kHz frames.
    pub dropped_frames: u64,
    /// Number of hard re-synchronisations (gap fill or drop).
    pub resyncs: u64,
    /// Last measured (filtered) timing error, in milliseconds.
    pub error_ms: f64,
    /// Current ratio correction, in parts per million.
    pub correction_ppm: f64,
    /// Source sample rate currently being resampled.
    pub source_rate: u32,
}

pub struct Track {
    src_rate: u32,
    resampler: Option<Async<f32>>,
    /// Source-rate samples waiting for the next full resampler chunk.
    fifo: Vec<f32>,
    rs_out: Vec<f32>,
    /// Output frames still to discard to compensate the resampler delay.
    skip_out: usize,
    delay: usize,
    /// Resampled audio; `out[0]` sits at timeline position `out_pos`.
    out: VecDeque<f32>,
    out_pos: u64,
    locked: bool,
    err_filt: f64,
    integral: f64,
    correction: f64,
    applied_correction: f64,
    peak: f32,
    stats: TrackStats,
}

impl Default for Track {
    fn default() -> Self {
        Self::new()
    }
}

impl Track {
    pub fn new() -> Self {
        Self {
            src_rate: 0,
            resampler: None,
            fifo: Vec::new(),
            rs_out: Vec::new(),
            skip_out: 0,
            delay: 0,
            out: VecDeque::new(),
            out_pos: 0,
            locked: false,
            err_filt: 0.0,
            integral: 0.0,
            correction: 0.0,
            applied_correction: 0.0,
            peak: 0.0,
            stats: TrackStats::default(),
        }
    }

    pub fn stats(&self) -> TrackStats {
        let mut s = self.stats.clone();
        s.error_ms = self.err_filt / SAMPLE_RATE as f64 * 1000.0;
        s.correction_ppm = self.correction * 1e6;
        s.source_rate = self.src_rate;
        s
    }

    /// Peak absolute sample value since the previous call.
    pub fn take_peak(&mut self) -> f32 {
        std::mem::take(&mut self.peak)
    }

    /// Timeline position just past the last resampled frame.
    pub fn end_pos(&self) -> u64 {
        self.out_pos + self.out.len() as u64
    }

    fn ratio(&self) -> f64 {
        SAMPLE_RATE as f64 / self.src_rate as f64 * (1.0 + self.applied_correction)
    }

    /// Timeline position where the next pushed input sample will end up.
    fn cursor(&self) -> f64 {
        self.end_pos() as f64
            + self.fifo.len() as f64 * self.ratio()
            + (self.delay - self.skip_out) as f64
    }

    fn ensure_resampler(&mut self, rate: u32) {
        if self.resampler.is_some() && self.src_rate == rate {
            return;
        }
        let params = SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: None,
            oversampling_factor: 64,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        };
        let chunk = (rate / 100).max(16) as usize;
        let ratio = SAMPLE_RATE as f64 / rate as f64;
        let rs = Async::<f32>::new_sinc(
            ratio,
            1.0 + 2.0 * MAX_CORRECTION,
            &params,
            chunk,
            1,
            FixedAsync::Input,
        )
        .expect("valid resampler parameters");
        if self.resampler.is_some() {
            tracing::info!(
                from = self.src_rate,
                to = rate,
                "source rate changed, rebuilding resampler"
            );
        }
        self.delay = rs.output_delay();
        self.skip_out = self.delay;
        self.rs_out = vec![0.0; rs.output_frames_max()];
        self.resampler = Some(rs);
        self.src_rate = rate;
        self.fifo.clear();
        self.locked = false;
        self.err_filt = 0.0;
        self.integral = 0.0;
        self.correction = 0.0;
        self.applied_correction = 0.0;
    }

    /// Adds one captured block whose first sample was taken at timeline position `pos`.
    pub fn push(&mut self, pos: f64, rate: u32, samples: &[f32]) {
        if samples.is_empty() || rate == 0 {
            return;
        }
        self.ensure_resampler(rate);
        for &s in samples {
            self.peak = self.peak.max(s.abs());
        }

        let (gap_th, late_th) = if self.locked {
            (GAP_FRAMES, LATE_FRAMES)
        } else {
            (0.5, 0.5)
        };
        let err = pos - self.cursor();
        let mut samples = samples;

        if err > gap_th {
            self.fill_gap(err);
            if self.locked {
                self.stats.resyncs += 1;
                tracing::debug!(gap_ms = err / 16.0, "gap filled with silence");
            }
            self.err_filt = 0.0;
        } else if err < -late_th {
            let drop_in = ((-err / self.ratio()).round() as usize).min(samples.len());
            samples = &samples[drop_in..];
            self.stats.dropped_frames += (drop_in as f64 * self.ratio()) as u64;
            if self.locked {
                self.stats.resyncs += 1;
                tracing::debug!(early_ms = -err / 16.0, "early block, dropped samples");
            }
            self.err_filt = 0.0;
        } else if self.locked {
            self.adjust_ratio(err, samples.len() as f64 / rate as f64);
        }
        self.locked = true;

        self.fifo.extend_from_slice(samples);
        self.run_resampler();
    }

    fn adjust_ratio(&mut self, err: f64, dt: f64) {
        let alpha = (dt / ERR_TAU_S).min(1.0);
        self.err_filt += alpha * (err - self.err_filt);
        let sr = SAMPLE_RATE as f64;
        self.integral = (self.integral + self.err_filt / (sr * KP_TIME_S * KI_TIME_S) * dt)
            .clamp(-MAX_INTEGRAL, MAX_INTEGRAL);
        self.correction = (self.err_filt / (sr * KP_TIME_S) + self.integral)
            .clamp(-MAX_CORRECTION, MAX_CORRECTION);
        if (self.correction - self.applied_correction).abs() > 1e-7 {
            let rs = self.resampler.as_mut().expect("resampler exists");
            if rs
                .set_resample_ratio_relative(1.0 + self.correction, true)
                .is_ok()
            {
                self.applied_correction = self.correction;
            }
        }
    }

    /// Inserts `frames` of silence (16 kHz frames) before the next input sample.
    fn fill_gap(&mut self, frames: f64) {
        self.stats.inserted_frames += frames.round() as u64;
        if frames <= DIRECT_GAP_FRAMES {
            let n_in = (frames / self.ratio()).round() as usize;
            self.fifo.resize(self.fifo.len() + n_in, 0.0);
            self.run_resampler();
            return;
        }
        // Long gap: flush the resampler with a little silence, then account for
        // the rest directly on the timeline instead of resampling zeros.
        let flush_in = self.resampler.as_ref().map_or(0, |r| r.input_frames_next()) * 2;
        let before = self.cursor();
        self.fifo.resize(self.fifo.len() + flush_in, 0.0);
        self.run_resampler();
        let remaining = (frames - (self.cursor() - before)).max(0.0).round() as usize;
        if self.out.is_empty() {
            self.out_pos += remaining as u64;
        } else {
            let n = remaining.min(MAX_QUEUED_GAP_FRAMES);
            if n < remaining {
                tracing::warn!(
                    gap_s = remaining as f64 / 16000.0,
                    "clock jump while audio queued, capping inserted silence"
                );
            }
            self.out.extend(std::iter::repeat_n(0.0, n));
        }
    }

    fn run_resampler(&mut self) {
        let Some(rs) = self.resampler.as_mut() else {
            return;
        };
        let mut consumed = 0;
        loop {
            let need = rs.input_frames_next();
            if self.fifo.len() - consumed < need {
                break;
            }
            let input = &self.fifo[consumed..consumed + need];
            let in_adapter = InterleavedSlice::new(input, 1, need).expect("input adapter");
            let cap = self.rs_out.len();
            let mut out_adapter =
                InterleavedSlice::new_mut(&mut self.rs_out, 1, cap).expect("output adapter");
            let (read, written) = rs
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
                .expect("resampler buffers sized from its own limits");
            consumed += read;
            let skip = self.skip_out.min(written);
            self.skip_out -= skip;
            self.out.extend(&self.rs_out[skip..written]);
        }
        self.fifo.drain(..consumed);
    }

    /// Copies timeline frames `[pos, pos + dst.len())` into `dst` without
    /// consuming them (zeros where nothing was delivered yet).
    pub fn peek(&self, pos: u64, dst: &mut [f32]) {
        for (i, d) in dst.iter_mut().enumerate() {
            let t = pos + i as u64;
            *d = if t >= self.out_pos {
                self.out
                    .get((t - self.out_pos) as usize)
                    .copied()
                    .unwrap_or(0.0)
            } else {
                0.0
            };
        }
    }

    /// Copies timeline frames `[pos, pos + dst.len())` into `dst` and forgets
    /// everything before `pos + dst.len()`. Frames the source did not deliver
    /// are zero. `pos` must never decrease between calls.
    pub fn take(&mut self, pos: u64, dst: &mut [f32]) {
        let end = pos + dst.len() as u64;
        if self.out_pos < pos {
            let stale = ((pos - self.out_pos) as usize).min(self.out.len());
            self.out.drain(..stale);
            self.stats.dropped_frames += stale as u64;
            self.out_pos = if self.out.is_empty() {
                self.out_pos.max(pos)
            } else {
                self.out_pos + stale as u64
            };
            if self.out_pos < pos {
                self.out_pos = pos;
            }
        }
        for (i, d) in dst.iter_mut().enumerate() {
            let t = pos + i as u64;
            *d = if t >= self.out_pos {
                self.out
                    .get((t - self.out_pos) as usize)
                    .copied()
                    .unwrap_or(0.0)
            } else {
                0.0
            };
        }
        if end > self.out_pos {
            let used = ((end - self.out_pos) as usize).min(self.out.len());
            self.out.drain(..used);
            self.out_pos += used as u64;
            if self.out.is_empty() && self.out_pos < end {
                self.out_pos = end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulated device: produces a sine at `rate * (1 + drift)` real samples
    /// per second but claims `rate`, delivering blocks of `block` frames.
    struct SimSource {
        rate: u32,
        drift: f64,
        block: usize,
        n: u64,
        freq: f64,
    }

    impl SimSource {
        /// Returns (timeline position of first sample, samples).
        fn next_block(&mut self) -> (f64, Vec<f32>) {
            let true_rate = self.rate as f64 * (1.0 + self.drift);
            let t0 = self.n as f64 / true_rate;
            let samples = (0..self.block)
                .map(|i| {
                    let t = (self.n + i as u64) as f64 / true_rate;
                    (2.0 * std::f64::consts::PI * self.freq * t).sin() as f32 * 0.5
                })
                .collect();
            self.n += self.block as u64;
            (t0 * SAMPLE_RATE as f64, samples)
        }
    }

    fn run(track: &mut Track, src: &mut SimSource, seconds: f64, committed: &mut u64) {
        let mut buf = vec![0.0; 320];
        let blocks = (seconds * src.rate as f64 / src.block as f64) as usize;
        for _ in 0..blocks {
            let (pos, s) = src.next_block();
            track.push(pos, src.rate, &s);
            // Writer keeps a 400 ms horizon behind the newest data.
            while *committed + 320 + 6400 <= pos as u64 {
                track.take(*committed, &mut buf);
                *committed += 320;
            }
        }
    }

    /// The track's position error stays bounded under realistic crystal drift.
    #[test]
    fn drift_is_compensated_over_three_hours() {
        for (rate, drift) in [
            (48_000, 300e-6),
            (44_100, -300e-6),
            (16_000, 150e-6),
            (48_000, 0.0),
        ] {
            let mut track = Track::new();
            let mut src = SimSource {
                rate,
                drift,
                block: (rate / 100) as usize,
                n: 0,
                freq: 440.0,
            };
            let mut committed = 0;
            run(&mut track, &mut src, 3.0 * 3600.0, &mut committed);
            let (pos, _) = src.next_block();
            let err_ms = (pos - track.cursor()) / 16.0;
            let st = track.stats();
            assert!(
                err_ms.abs() < 20.0,
                "rate {rate} drift {drift}: error {err_ms} ms"
            );
            assert_eq!(
                st.resyncs, 0,
                "rate {rate} drift {drift}: unexpected hard resync"
            );
            // A fast device clock delivers too many samples: the ratio must shrink.
            assert!(
                (st.correction_ppm + drift * 1e6).abs() < 50.0,
                "correction {} ppm for drift {}",
                st.correction_ppm,
                drift * 1e6
            );
        }
    }

    /// Silence periods where the source delivers nothing are filled and the
    /// audio after the gap lands at the right timeline position.
    #[test]
    fn gaps_are_filled_and_alignment_restored() {
        let mut track = Track::new();
        let rate = 48_000;
        let mut committed = 0;
        let mut buf = vec![0.0; 320];
        // 1 s of tone, 5 s nothing (loopback silence), 1 s of tone.
        let tone = |n: usize| vec![0.5f32; n];
        for b in 0..100 {
            track.push(b as f64 * 160.0, rate, &tone(480));
        }
        // Writer runs through the silent period.
        while committed + 320 <= 6 * 16_000 - 6400 {
            track.take(committed, &mut buf);
            committed += 320;
        }
        for b in 0..100 {
            track.push(6.0 * 16_000.0 + b as f64 * 160.0, rate, &tone(480));
        }
        let mut all = Vec::new();
        while committed < 7 * 16_000 {
            track.take(committed, &mut buf);
            all.extend_from_slice(&buf);
            committed += 320;
        }
        // Timeline from (6 s - 400 ms): first 400 ms silent, then tone from 6.0 s.
        let start = 6 * 16_000 - 6400;
        // Skip the few frames of resampler tail from the previous burst.
        let first_loud =
            all.iter().skip(100).position(|s| s.abs() > 0.25).unwrap() as i64 + 100 + start as i64;
        assert!(
            all[100..6300].iter().all(|s| s.abs() < 0.01),
            "gap not silent"
        );
        // The resampler delay is compensated, so the tone starts exactly at 6 s.
        let expected = 6 * 16_000;
        assert!(
            (first_loud - expected).abs() < 40,
            "tone resumed at {first_loud}, expected ~{expected}"
        );
        assert!(track.stats().inserted_frames > 0);
    }

    #[test]
    fn take_returns_silence_when_nothing_delivered() {
        let mut track = Track::new();
        let mut buf = vec![1.0; 320];
        track.take(0, &mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
        track.take(320, &mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn rate_change_mid_stream_keeps_position() {
        let mut track = Track::new();
        let mut committed = 0;
        let mut buf = vec![0.0; 320];
        for b in 0..200 {
            track.push(b as f64 * 160.0, 48_000, &vec![0.1; 480]);
        }
        // Bluetooth HFP switch: device now delivers 16 kHz, after a 300 ms pause.
        let resume = 200.0 * 160.0 + 4800.0;
        for b in 0..200 {
            track.push(resume + b as f64 * 160.0, 16_000, &vec![0.1; 160]);
        }
        let err_ms = ((resume + 200.0 * 160.0) - track.cursor()) / 16.0;
        assert!(err_ms.abs() < 5.0, "error after rate change {err_ms} ms");
        while committed + 320 <= track.end_pos() {
            track.take(committed, &mut buf);
            committed += 320;
        }
    }
}
