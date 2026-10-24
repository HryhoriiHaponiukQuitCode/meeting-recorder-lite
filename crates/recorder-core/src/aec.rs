//! Acoustic echo cancellation for the microphone channel, used when the
//! call plays through speakers and the microphone hears the remote side.
//!
//! Partitioned-block frequency-domain NLMS (overlap-save), adapted from
//! OpenCassava (MIT, © 2026 OpenCassava contributors,
//! `crates/opencassava-core/src/audio/echo_cancel.rs`). Changes:
//!
//! * 10 ms blocks, so a 20 ms frame is processed with no added latency;
//! * the reference comes from the session timeline, which already aligns the
//!   system channel with the microphone; the engine passes it
//!   [`LOOKAHEAD`] samples early so echo that appears slightly *before* the
//!   reference (timestamp offsets between devices) is still cancellable;
//! * the reference spectrum history advances during silence too, so the
//!   partitions stay aligned in time;
//! * adaptation freezes during double talk (Geigel detector), protecting the
//!   user's voice, and a divergence guard resets the filter;
//! * no allocation per block.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Samples per processing block (10 ms at 16 kHz).
pub const BLOCK: usize = 160;
const FFT: usize = 2 * BLOCK;
const SPEC: usize = FFT / 2 + 1;
/// How far ahead of the microphone the reference is read, in samples (40 ms).
pub const LOOKAHEAD: usize = 640;
/// Filter length in blocks: covers echo delays from −40 ms to +200 ms.
const PARTITIONS: usize = 24;
const MU: f32 = 0.4;
const POWER_SMOOTH: f32 = 0.8;
const REGULARIZATION: f32 = 1e-6;
/// Reference quieter than this (≈ −48 dBFS RMS) counts as silence.
const MIN_REF_RMS: f32 = 0.004;
/// Geigel double-talk threshold: mic peak above this fraction of the recent
/// reference peak means the user is talking.
const DOUBLE_TALK_RATIO: f32 = 0.5;

pub struct EchoCanceller {
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    filter: Vec<Vec<Complex32>>,
    /// Reference spectra, newest at `head`, as a ring of `PARTITIONS`.
    ref_spectra: Vec<Vec<Complex32>>,
    head: usize,
    ref_power: Vec<f32>,
    ref_peaks: Vec<f32>,
    prev_mic: Vec<f32>,
    prev_ref: Vec<f32>,
    time: Vec<f32>,
    mic_spec: Vec<Complex32>,
    err_spec: Vec<Complex32>,
    grad: Vec<Complex32>,
    scratch_spec: Vec<Complex32>,
    silent_blocks: usize,
    blocks: u64,
    /// Blocks where adaptation was frozen because of double talk.
    pub double_talk_blocks: u64,
    pub resets: u64,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoCanceller {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let zero = vec![Complex32::new(0.0, 0.0); SPEC];
        Self {
            fwd: planner.plan_fft_forward(FFT),
            inv: planner.plan_fft_inverse(FFT),
            filter: vec![zero.clone(); PARTITIONS],
            ref_spectra: vec![zero.clone(); PARTITIONS],
            head: 0,
            ref_power: vec![1e-4; SPEC],
            ref_peaks: vec![0.0; PARTITIONS + LOOKAHEAD / BLOCK],
            prev_mic: vec![0.0; BLOCK],
            prev_ref: vec![0.0; BLOCK],
            time: vec![0.0; FFT],
            mic_spec: zero.clone(),
            err_spec: zero.clone(),
            grad: zero.clone(),
            scratch_spec: zero,
            silent_blocks: 0,
            blocks: 0,
            double_talk_blocks: 0,
            resets: 0,
        }
    }

    /// Cancels echo in `mic` in place. `reference` is the system channel for
    /// the same timeline span, read [`LOOKAHEAD`] samples ahead. Both lengths
    /// must be equal multiples of [`BLOCK`].
    pub fn process(&mut self, mic: &mut [f32], reference: &[f32]) {
        debug_assert_eq!(mic.len(), reference.len());
        let refs = reference.as_chunks::<BLOCK>().0;
        for (m, r) in mic.as_chunks_mut::<BLOCK>().0.iter_mut().zip(refs) {
            self.process_block(m, r);
        }
    }

    fn forward(&mut self, prev: &[f32], cur: &[f32], out_is_mic: bool) {
        self.time[..BLOCK].copy_from_slice(prev);
        self.time[BLOCK..].copy_from_slice(cur);
        let out = if out_is_mic {
            &mut self.mic_spec
        } else {
            &mut self.scratch_spec
        };
        self.fwd
            .process(&mut self.time, out)
            .expect("fft sizes match");
    }

    fn process_block(&mut self, mic: &mut [f32], reference: &[f32]) {
        self.blocks += 1;
        // Reference history always advances, even through silence.
        let prev_ref = std::mem::take(&mut self.prev_ref);
        self.forward(&prev_ref, reference, false);
        self.prev_ref = prev_ref;
        self.prev_ref.copy_from_slice(reference);
        self.head = (self.head + PARTITIONS - 1) % PARTITIONS;
        std::mem::swap(&mut self.ref_spectra[self.head], &mut self.scratch_spec);
        let ref_peak = reference.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        self.ref_peaks.rotate_right(1);
        self.ref_peaks[0] = ref_peak;

        let ref_rms = (reference.iter().map(|s| s * s).sum::<f32>() / BLOCK as f32).sqrt();
        if ref_rms < MIN_REF_RMS {
            self.silent_blocks += 1;
        } else {
            self.silent_blocks = 0;
        }
        // Nothing has played within the filter span: there can be no echo.
        if self.silent_blocks > PARTITIONS + LOOKAHEAD / BLOCK {
            self.prev_mic.copy_from_slice(mic);
            return;
        }

        let prev_mic = std::mem::take(&mut self.prev_mic);
        self.forward(&prev_mic, mic, true);
        self.prev_mic = prev_mic;

        // Smoothed reference power over all partitions, for normalisation.
        for k in 0..SPEC {
            let total: f32 = REGULARIZATION
                + self
                    .ref_spectra
                    .iter()
                    .map(|x| x[k].norm_sqr())
                    .sum::<f32>();
            self.ref_power[k] = POWER_SMOOTH * self.ref_power[k] + (1.0 - POWER_SMOOTH) * total;
        }

        // Error = mic − Σ W_p · X_p.
        self.err_spec.copy_from_slice(&self.mic_spec);
        for p in 0..PARTITIONS {
            let x = &self.ref_spectra[(self.head + p) % PARTITIONS];
            for (e, (w, x)) in self.err_spec.iter_mut().zip(self.filter[p].iter().zip(x)) {
                *e -= w * x;
            }
        }
        self.scratch_spec.copy_from_slice(&self.err_spec);
        self.inv
            .process(&mut self.scratch_spec, &mut self.time)
            .expect("fft sizes match");
        let norm = 1.0 / FFT as f32;
        let out_energy: f32 = self.time[BLOCK..].iter().map(|s| (s * norm).powi(2)).sum();
        let mic_energy: f32 = mic.iter().map(|s| s * s).sum();

        if out_energy > 4.0 * mic_energy + 1e-6 {
            // Diverged: start over rather than add noise.
            for w in &mut self.filter {
                w.fill(Complex32::new(0.0, 0.0));
            }
            self.resets += 1;
            self.prev_mic.copy_from_slice(mic);
            return;
        }

        let mic_peak = mic.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        let recent_ref_peak = self.ref_peaks.iter().fold(0.0f32, |a, &s| a.max(s));
        let double_talk = mic_peak > DOUBLE_TALK_RATIO * recent_ref_peak;
        // Echo in this block comes from reference blocks up to the filter
        // length back, so adapt whenever any of them was audible.
        if double_talk {
            self.double_talk_blocks += 1;
        } else {
            self.adapt();
        }

        self.prev_mic.copy_from_slice(mic);
        for (m, &e) in mic.iter_mut().zip(&self.time[BLOCK..]) {
            *m = (e * norm).clamp(-1.0, 1.0);
        }
    }

    fn adapt(&mut self) {
        let norm = 1.0 / FFT as f32;
        for p in 0..PARTITIONS {
            let x = &self.ref_spectra[(self.head + p) % PARTITIONS];
            let terms = self.err_spec.iter().zip(x.iter().zip(&self.ref_power));
            for (g, (e, (xk, pk))) in self.grad.iter_mut().zip(terms) {
                *g = e * xk.conj() * (MU / pk);
            }
            // Gradient constraint: keep the filter causal within one block.
            self.inv
                .process(&mut self.grad, &mut self.time)
                .expect("fft sizes match");
            for s in &mut self.time[..BLOCK] {
                *s *= norm;
            }
            self.time[BLOCK..].fill(0.0);
            self.fwd
                .process(&mut self.time, &mut self.grad)
                .expect("fft sizes match");
            for (w, g) in self.filter[p].iter_mut().zip(&self.grad) {
                *w += g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic speech-like signal: noise shaped by a syllable envelope.
    fn speechish(seed: u64, n: usize, rate_hz: f32) -> Vec<f32> {
        let mut x = seed;
        let mut lp = 0.0f32;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let white = (x as f32 / u64::MAX as f32) * 2.0 - 1.0;
                lp = 0.7 * lp + 0.3 * white;
                let t = i as f32 / 16_000.0;
                let env = (std::f32::consts::TAU * rate_hz * t).sin().max(0.0);
                0.5 * lp * env
            })
            .collect()
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32
    }

    /// Room echo: `delay_ms` (may be negative) plus a short decaying tail, −10 dB.
    fn echo_of(reference: &[f32], delay_ms: i32) -> Vec<f32> {
        let d = delay_ms * 16;
        let taps = [0.3f32, 0.12, -0.06, 0.03];
        (0..reference.len())
            .map(|i| {
                taps.iter()
                    .enumerate()
                    .map(|(k, &h)| {
                        let j = i as i64 - d as i64 - (k as i64 * 8);
                        if j >= 0 && (j as usize) < reference.len() {
                            h * reference[j as usize]
                        } else {
                            0.0
                        }
                    })
                    .sum()
            })
            .collect()
    }

    /// Runs the canceller the way the engine does: reference read LOOKAHEAD early.
    fn run(mic: &[f32], reference: &[f32]) -> Vec<f32> {
        let mut aec = EchoCanceller::new();
        let mut out = mic.to_vec();
        let mut r = vec![0.0; 320];
        for (f, chunk) in out.as_chunks_mut::<320>().0.iter_mut().enumerate() {
            for (i, v) in r.iter_mut().enumerate() {
                *v = reference
                    .get(f * 320 + i + LOOKAHEAD)
                    .copied()
                    .unwrap_or(0.0);
            }
            aec.process(chunk, &r);
        }
        out
    }

    fn erle_db(mic: &[f32], out: &[f32], from: usize) -> f32 {
        10.0 * (energy(&mic[from..]) / energy(&out[from..]).max(1e-12)).log10()
    }

    #[test]
    fn cancels_echo_for_positive_and_negative_delays() {
        let n = 16_000 * 8;
        let far = speechish(7, n, 3.0);
        for delay_ms in [-30, 0, 25, 120, 180] {
            let mic = echo_of(&far, delay_ms);
            let out = run(&mic, &far);
            let erle = erle_db(&mic, &out, 16_000 * 4);
            println!("delay {delay_ms} ms: ERLE {erle:.1} dB");
            assert!(erle > 20.0, "delay {delay_ms} ms: ERLE {erle:.1} dB");
        }
    }

    #[test]
    fn passes_microphone_through_when_nothing_plays() {
        let near = speechish(3, 16_000 * 2, 4.0);
        let out = run(&near, &vec![0.0; near.len()]);
        let max_diff = out
            .iter()
            .zip(&near)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-5, "changed the microphone by {max_diff}");
    }

    #[test]
    fn keeps_the_users_voice_during_double_talk() {
        let n = 16_000 * 10;
        let far = speechish(11, n, 3.0);
        let near = speechish(99, n, 2.3);
        let echo = echo_of(&far, 40);
        // First 4 s far end only (convergence), then both talk.
        let mic: Vec<f32> = (0..n)
            .map(|i| echo[i] + if i >= 16_000 * 4 { near[i] } else { 0.0 })
            .collect();
        let out = run(&mic, &far);
        let from = 16_000 * 5;
        let residual: Vec<f32> = out[from..]
            .iter()
            .zip(&near[from..])
            .map(|(o, s)| o - s)
            .collect();
        let near_e = energy(&near[from..]);
        // The user's voice survives (error vs. clean near end ≥ 10 dB below it)…
        let snr = 10.0 * (near_e / energy(&residual)).log10();
        assert!(snr > 10.0, "near-end SNR after AEC {snr:.1} dB");
        // …and is not attenuated.
        let gain = 10.0 * (energy(&out[from..]) / near_e).log10();
        assert!(gain.abs() < 2.0, "near-end level changed by {gain:.1} dB");
    }
}
