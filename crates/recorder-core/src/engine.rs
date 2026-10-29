//! Glue between capture and disk: places both sources on the session
//! timeline and commits 20 ms stereo frames once they are older than the
//! commit horizon. Pure logic driven by explicit host-clock timestamps, so it
//! runs identically in tests with simulated clocks.

use std::io;

use crate::aec::{self, EchoCanceller};
use crate::manifest::Manifest;
use crate::session::{Gap, SessionWriter};
use crate::track::{Track, TrackStats};
use crate::{CHANNELS, FRAME_SAMPLES, MIC, SYSTEM, ns_to_frames};

/// Default distance between "now" and the newest committed frame. Must exceed
/// the longest capture callback period (Bluetooth, ScreenCaptureKit) so audio
/// is on the timeline before it is committed.
pub const DEFAULT_LATENCY_MS: u64 = 400;

pub struct Engine {
    t0_ns: u64,
    tracks: [Track; CHANNELS],
    committed: u64,
    latency_frames: u64,
    session: SessionWriter,
    mono: [Vec<f32>; CHANNELS],
    reference: Vec<f32>,
    interleaved: Vec<f32>,
    aec: Option<EchoCanceller>,
}

impl Engine {
    /// `t0_ns` is the host-clock time of timeline position zero.
    pub fn new(session: SessionWriter, t0_ns: u64, latency_ms: u64) -> Self {
        Self {
            t0_ns,
            tracks: [Track::new(), Track::new()],
            committed: 0,
            latency_frames: latency_ms * crate::SAMPLE_RATE as u64 / 1000,
            session,
            mono: [vec![0.0; FRAME_SAMPLES], vec![0.0; FRAME_SAMPLES]],
            reference: vec![0.0; FRAME_SAMPLES],
            interleaved: vec![0.0; FRAME_SAMPLES * CHANNELS],
            aec: None,
        }
    }

    /// Turns echo cancellation of the microphone channel on or off.
    pub fn set_aec(&mut self, enabled: bool) {
        match (enabled, self.aec.is_some()) {
            (true, false) => self.aec = Some(EchoCanceller::new()),
            (false, true) => self.aec = None,
            _ => {}
        }
    }

    pub fn aec_enabled(&self) -> bool {
        self.aec.is_some()
    }

    /// Call after a gap of `paused_ms` (system sleep or a user pause) during
    /// which nothing was ingested. Commits what was captured before
    /// `before_ns` (the last host time before the gap), records the gap as an
    /// event (the part continues) and re-anchors the timeline at `now_ns`,
    /// so the gap is not recorded as silence even on hosts whose clock keeps
    /// running.
    pub fn resume_after_pause(
        &mut self,
        gap: Gap,
        before_ns: u64,
        now_ns: u64,
        paused_ms: u64,
    ) -> io::Result<()> {
        let before = self.pos(before_ns);
        let captured = self.tracks.iter().map(Track::end_pos).max().unwrap_or(0) as f64;
        let frame = FRAME_SAMPLES as f64;
        self.commit_until((captured.min(before) / frame).ceil() * frame)?;
        self.session.pause(gap, paused_ms)?;
        self.tracks = [Track::new(), Track::new()];
        if self.aec.is_some() {
            self.aec = Some(EchoCanceller::new());
        }
        let ahead_ns = (self.committed + self.latency_frames) as u128 * 1_000_000_000
            / crate::SAMPLE_RATE as u128;
        self.t0_ns = now_ns.saturating_sub(ahead_ns as u64);
        Ok(())
    }

    pub fn session(&self) -> &SessionWriter {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionWriter {
        &mut self.session
    }

    pub fn track_stats(&self, channel: usize) -> TrackStats {
        self.tracks[channel].stats()
    }

    /// Peak level per channel since the previous call.
    pub fn take_peaks(&mut self) -> [f32; CHANNELS] {
        [self.tracks[0].take_peak(), self.tracks[1].take_peak()]
    }

    fn pos(&self, host_ns: u64) -> f64 {
        ns_to_frames(host_ns as i128 - self.t0_ns as i128)
    }

    /// Adds a captured mono block whose first sample was taken at `host_ns`.
    pub fn ingest(&mut self, channel: usize, host_ns: u64, rate: u32, samples: &[f32]) {
        let pos = self.pos(host_ns);
        self.tracks[channel].push(pos, rate, samples);
    }

    /// Commits every frame older than `now - latency`.
    pub fn tick(&mut self, now_ns: u64) -> io::Result<()> {
        let horizon = self.pos(now_ns) - self.latency_frames as f64;
        self.commit_until(horizon)
    }

    fn commit_until(&mut self, until: f64) -> io::Result<()> {
        while (self.committed + FRAME_SAMPLES as u64) as f64 <= until {
            if self.aec.is_some() {
                self.tracks[SYSTEM]
                    .peek(self.committed + aec::LOOKAHEAD as u64, &mut self.reference);
            }
            for (track, mono) in self.tracks.iter_mut().zip(self.mono.iter_mut()) {
                track.take(self.committed, mono);
            }
            if let Some(aec) = self.aec.as_mut() {
                aec.process(&mut self.mono[MIC], &self.reference);
            }
            for (i, frame) in self
                .interleaved
                .as_chunks_mut::<CHANNELS>()
                .0
                .iter_mut()
                .enumerate()
            {
                frame[0] = self.mono[0][i];
                frame[1] = self.mono[1][i];
            }
            self.session.push_frame(&self.interleaved)?;
            self.committed += FRAME_SAMPLES as u64;
        }
        Ok(())
    }

    /// Commits everything captured up to `now_ns` (no horizon) and closes the session.
    pub fn finish(mut self, now_ns: u64) -> io::Result<Manifest> {
        let captured = self.tracks.iter().map(Track::end_pos).max().unwrap_or(0) as f64;
        let end = captured.min(self.pos(now_ns)).max(self.committed as f64);
        let frame = FRAME_SAMPLES as f64;
        self.commit_until((end / frame).ceil() * frame)?;
        self.session.finish()
    }
}
