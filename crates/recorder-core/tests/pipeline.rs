//! End-to-end tests of the recording pipeline with simulated devices and
//! clocks: engine → encoder → part files → decoder.
//!
//! Run with `cargo test --release` (debug builds of libopus and the
//! resampler make the long simulations slow).

use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use recorder_core::decode::{decode_part, rms_dbfs};
use recorder_core::engine::Engine;
use recorder_core::manifest::{Manifest, OsInfo, PartStatus, Status};
use recorder_core::opus_ogg::EncoderConfig;
use recorder_core::session::{SessionConfig, SessionWriter};
use recorder_core::{MIC, SYSTEM, recovery};

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mr-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn new_engine(root: &Path, part_seconds: u32) -> Engine {
    let cfg = SessionConfig {
        root: root.to_path_buf(),
        part_seconds,
        encoder: EncoderConfig::default(),
        os: OsInfo {
            family: "test".into(),
            version: "0".into(),
            arch: "x".into(),
        },
    };
    Engine::new(SessionWriter::create(cfg).unwrap(), 0, 400)
}

/// A simulated capture device. The signal is defined in host time; the
/// device clock runs `drift` fast, so it delivers `rate * (1 + drift)` real
/// samples per second while claiming `rate`.
struct SimSource {
    channel: usize,
    rate: u32,
    drift: f64,
    block: u64,
    n: u64,
    /// Host-time intervals (seconds) during which the device delivers nothing.
    gaps: Vec<(f64, f64)>,
    signal: fn(f64) -> f32,
}

impl SimSource {
    fn true_time(&self, n: u64) -> f64 {
        n as f64 / (self.rate as f64 * (1.0 + self.drift))
    }

    /// Delivers every block that is complete by host time `t`.
    fn feed_until(&mut self, engine: &mut Engine, t: f64) {
        while self.true_time(self.n + self.block) <= t {
            let t0 = self.true_time(self.n);
            let silent = self.gaps.iter().any(|&(a, b)| t0 >= a && t0 < b);
            if !silent {
                let samples: Vec<f32> = (0..self.block)
                    .map(|i| (self.signal)(self.true_time(self.n + i)))
                    .collect();
                engine.ingest(self.channel, (t0 * 1e9) as u64, self.rate, &samples);
            }
            self.n += self.block;
        }
    }
}

fn simulate(engine: &mut Engine, sources: &mut [SimSource], seconds: f64) {
    let step = 0.01;
    let mut t = 0.0;
    while t < seconds {
        t += step;
        for s in sources.iter_mut() {
            s.feed_until(engine, t);
        }
        engine.tick((t * 1e9) as u64).unwrap();
    }
}

fn decode_session(dir: &Path) -> (Manifest, Vec<f32>, Vec<f32>) {
    let manifest = Manifest::load(dir).unwrap();
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for p in &manifest.parts {
        let d = decode_part(&dir.join(&p.file)).unwrap();
        assert_eq!(
            d.left.len() as u64,
            p.duration_ms * 16,
            "{} decoded length vs manifest",
            p.file
        );
        l.extend(d.left);
        r.extend(d.right);
    }
    (manifest, l, r)
}

fn sine300(t: f64) -> f32 {
    (0.3 * (2.0 * PI * 300.0 * t).sin()) as f32
}

fn sine700(t: f64) -> f32 {
    (0.3 * (2.0 * PI * 700.0 * t).sin()) as f32
}

/// 1 kHz burst of 10 ms at 0.5 s past every whole second.
fn clicks(t: f64) -> f32 {
    let ph = t.fract();
    if (0.5..0.51).contains(&ph) {
        (0.5 * (2.0 * PI * 1000.0 * t).sin()) as f32
    } else {
        0.0
    }
}

/// Lag (in samples) that best aligns `got` with `reference` around `center`.
fn best_lag(
    got: &[f32],
    reference: &dyn Fn(usize) -> f32,
    center: usize,
    half: usize,
    max_lag: i64,
) -> i64 {
    let mut best = (f64::MIN, 0);
    for lag in -max_lag..=max_lag {
        let mut acc = 0.0;
        for i in center - half..center + half {
            let j = i as i64 + lag;
            if j >= 0 && (j as usize) < got.len() {
                acc += got[j as usize] as f64 * reference(i) as f64;
            }
        }
        if acc > best.0 {
            best = (acc, lag);
        }
    }
    best.1
}

#[test]
fn parts_are_seamless_with_exact_lengths() {
    let root = temp_root("seamless");
    let mut engine = new_engine(&root, 2);
    let dir = engine.session().dir().to_path_buf();
    let mut sources = [
        SimSource {
            channel: MIC,
            rate: 48_000,
            drift: 0.0,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: sine300,
        },
        SimSource {
            channel: SYSTEM,
            rate: 44_100,
            drift: 0.0,
            block: 441,
            n: 0,
            gaps: vec![],
            signal: sine700,
        },
    ];
    simulate(&mut engine, &mut sources, 5.3);
    let m = engine.finish((5.3e9) as u64).unwrap();
    assert_eq!(m.status, Status::Completed);
    assert_eq!(m.parts.len(), 3);
    assert_eq!(m.parts[0].duration_ms, 2000);
    assert_eq!(m.parts[1].duration_ms, 2000);
    assert_eq!(m.parts[1].start_offset_ms, 2000);
    assert!(m.parts.iter().all(|p| p.status == PartStatus::Closed));

    let (_, l, r) = decode_session(&dir);
    assert_eq!(l.len(), r.len());
    // The codec shifts the phase of a pure tone by a few samples everywhere;
    // at a part boundary the shift must be the same as inside a part, i.e.
    // nothing was lost or duplicated at the seam.
    let lag_at = |x: &[f32], f: fn(f64) -> f32, at: usize| {
        best_lag(x, &|i| f(i as f64 / 16_000.0), at, 400, 20)
    };
    for (x, f, name) in [
        (&l, sine300 as fn(f64) -> f32, "mic"),
        (&r, sine700 as fn(f64) -> f32, "system"),
    ] {
        let inside = [16_000usize, 48_000, 80_000].map(|at| lag_at(x, f, at));
        assert!(
            inside.iter().all(|&g| g == inside[0]),
            "{name}: lag varies inside parts {inside:?}"
        );
        for boundary in [32_000usize, 64_000] {
            let lag = lag_at(x, f, boundary);
            assert!(
                (lag - inside[0]).abs() <= 1,
                "{name}: lag {lag} at boundary {boundary}, {} inside",
                inside[0]
            );
            assert!(
                rms_dbfs(&x[boundary - 160..boundary + 160]) > -15.0,
                "{name}: dropout at boundary {boundary}"
            );
        }
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn channels_do_not_leak_into_each_other() {
    let root = temp_root("isolation");
    let mut engine = new_engine(&root, 600);
    let dir = engine.session().dir().to_path_buf();
    fn loud(t: f64) -> f32 {
        // Speech-band chirp, as hard as possible on a joint-stereo encoder.
        (0.5 * (2.0 * PI * (200.0 + 300.0 * t) * t).sin()) as f32
    }
    fn silent(_: f64) -> f32 {
        0.0
    }
    let mut sources = [
        SimSource {
            channel: MIC,
            rate: 48_000,
            drift: 0.0,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: loud,
        },
        SimSource {
            channel: SYSTEM,
            rate: 48_000,
            drift: 0.0,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: silent,
        },
    ];
    simulate(&mut engine, &mut sources, 4.0);
    engine.finish(4_000_000_000).unwrap();
    let (_, l, r) = decode_session(&dir);
    assert!(rms_dbfs(&l) > -12.0);
    assert!(
        rms_dbfs(&r) < -90.0,
        "system channel picked up {} dBFS of the mic",
        rms_dbfs(&r)
    );
    std::fs::remove_dir_all(root).ok();
}

/// System clock 300 ppm fast, loopback silent for 3 s and 45 s: the clicks
/// played simultaneously into both channels stay aligned.
#[test]
fn channels_stay_aligned_with_drift_and_loopback_silence() {
    let root = temp_root("align");
    let mut engine = new_engine(&root, 120);
    let dir = engine.session().dir().to_path_buf();
    let minutes = 6.0;
    let mut sources = [
        SimSource {
            channel: MIC,
            rate: 48_000,
            drift: -100e-6,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: clicks,
        },
        SimSource {
            channel: SYSTEM,
            rate: 44_100,
            drift: 300e-6,
            block: 441,
            n: 0,
            gaps: vec![(30.0, 33.0), (120.0, 165.0)],
            signal: clicks,
        },
    ];
    simulate(&mut engine, &mut sources, minutes * 60.0);
    let stats = engine.track_stats(SYSTEM);
    engine.finish((minutes * 60e9) as u64).unwrap();
    assert!(stats.resyncs >= 2, "gaps should cause resyncs: {stats:?}");
    let (m, l, r) = decode_session(&dir);
    assert_eq!(m.parts.len(), 3);

    let onset = |x: &[f32], sec: usize| -> Option<i64> {
        let start = sec * 16_000 + 7_000;
        x[start..start + 2_000]
            .iter()
            .position(|s| s.abs() > 0.1)
            .map(|p| (start + p) as i64)
    };
    let mut worst = 0i64;
    for sec in 1..(minutes as usize * 60 - 1) {
        let expected = sec as i64 * 16_000 + 8_000;
        let in_gap = sources[1]
            .gaps
            .iter()
            .any(|&(a, b)| (sec as f64 + 0.5) >= a && (sec as f64 + 0.5) < b);
        let lo = onset(&l, sec).expect("mic click present");
        assert!(
            (lo - expected).abs() < 48,
            "mic click at {sec}s off by {} samples",
            lo - expected
        );
        match onset(&r, sec) {
            Some(ro) => {
                assert!(!in_gap, "system click inside a silent gap at {sec}s");
                worst = worst.max((ro - lo).abs());
            }
            None => assert!(in_gap, "system click missing at {sec}s"),
        }
    }
    // < 5 ms between channels, the spec allows 100 ms per hour.
    assert!(
        worst < 80,
        "worst L/R misalignment {} ms",
        worst as f64 / 16.0
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn crash_keeps_flushed_audio_and_recovery_closes_session() {
    let root = temp_root("crash");
    let mut engine = new_engine(&root, 600);
    let dir = engine.session().dir().to_path_buf();
    let mut sources = [SimSource {
        channel: MIC,
        rate: 48_000,
        drift: 0.0,
        block: 480,
        n: 0,
        gaps: vec![],
        signal: sine300,
    }];
    simulate(&mut engine, &mut sources, 5.0);
    // Simulated crash: no finish(). Complete pages are already fsynced; the
    // crash leaves a half-written page at the end of the file.
    drop(engine);
    let part = dir.join("part_0001.ogg");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&part)
        .unwrap();
    std::io::Write::write_all(&mut f, b"OggS\0\0half-written page").unwrap();
    drop(f);

    // Not recovered yet: still "recording".
    assert_eq!(Manifest::load(&dir).unwrap().status, Status::Recording);
    let rec = recovery::recover_all(&root);
    assert_eq!(rec.len(), 1);
    let m = Manifest::load(&dir).unwrap();
    assert_eq!(m.status, Status::CrashedRecovered);
    assert_eq!(m.parts[0].status, PartStatus::Recovered);
    // 4.6 s were committed (5 s − 400 ms horizon); the four closed one-second
    // pages survive (minus the 6.5 ms pre-skip), the unclosed 0.6 s is lost.
    assert_eq!(m.parts[0].duration_ms, 3_993);
    assert_eq!(rec[0].truncated_bytes, 23);
    let d = decode_part(&part).unwrap();
    assert!(!d.truncated);
    assert!(d.left.len() as u64 >= m.parts[0].duration_ms * 16 - 320);
    // Idempotent.
    assert!(recovery::recover_all(&root).is_empty());
    std::fs::remove_dir_all(root).ok();
}

/// Speech-like signal (noise with a syllable-rate envelope, pauses) to
/// estimate file size per hour.
#[test]
fn file_size_per_hour_is_within_budget() {
    let root = temp_root("size");
    let mut engine = new_engine(&root, 600);
    let dir = engine.session().dir().to_path_buf();
    fn speechish(t: f64) -> f32 {
        let mut x = (t * 1e4).to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 29;
        let noise = (x as f64 / u64::MAX as f64) * 2.0 - 1.0;
        let syllables = (2.0 * PI * 4.0 * t).sin().max(0.0);
        let talking = if (t / 6.0).fract() < 0.6 { 1.0 } else { 0.0 };
        (0.2 * noise * syllables * talking
            + 0.15 * (2.0 * PI * 180.0 * t).sin() * syllables * talking) as f32
    }
    fn speechish_late(t: f64) -> f32 {
        speechish(t + 3.0)
    }
    let mut sources = [
        SimSource {
            channel: MIC,
            rate: 48_000,
            drift: 0.0,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: speechish,
        },
        SimSource {
            channel: SYSTEM,
            rate: 48_000,
            drift: 0.0,
            block: 480,
            n: 0,
            gaps: vec![],
            signal: speechish_late,
        },
    ];
    let seconds = 60.0;
    simulate(&mut engine, &mut sources, seconds);
    engine.finish((seconds * 1e9) as u64).unwrap();
    let bytes = std::fs::metadata(dir.join("part_0001.ogg")).unwrap().len() as f64;
    let mb_per_hour = bytes * 3600.0 / seconds / 1e6;
    println!("estimated size: {mb_per_hour:.1} MB/h");
    assert!(mb_per_hour < 16.0, "{mb_per_hour:.1} MB/h");
    std::fs::remove_dir_all(root).ok();
}
