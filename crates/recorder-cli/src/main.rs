//! `recorder` — console front end of Meeting Recorder.
//!
//! ```text
//! recorder start            record mic (L) + system audio (R) until Ctrl+C
//! recorder devices          list audio devices
//! recorder recover          close sessions left open by a crash
//! recorder verify <dir>     decode a session and report levels; --wav exports me.wav / them.wav
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use recorder_app::settings::Settings;
use recorder_app::{
    AecMode, CONSENT_REMINDER, Recorder, RecorderOptions, SystemOption, logging, recover_all,
};
use recorder_capture::{mic, output};
use recorder_core::decode::{decode_part, rms_dbfs, write_wav};
use recorder_core::manifest::{Manifest, PartStatus};
use recorder_core::{MIC, SYSTEM};

#[derive(Parser)]
#[command(
    name = "recorder",
    version,
    about = "Meeting Recorder: records the microphone (L) and system audio (R) to Ogg Opus"
)]
struct Cli {
    /// Recordings folder (default: Documents/Meeting Recorder/recordings).
    #[arg(long, global = true)]
    out: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum SystemArg {
    /// The usual method for this OS.
    Auto,
    /// macOS 14.4+: Core Audio process tap.
    Tap,
    /// macOS: ScreenCaptureKit.
    Sck,
    /// No system audio.
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum AecArg {
    /// Only when sound plays through speakers.
    Auto,
    On,
    Off,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start recording; Ctrl+C stops.
    Start {
        /// Length of one part, minutes.
        #[arg(long)]
        part_minutes: Option<u32>,
        /// Part length in seconds (for tests; overrides --part-minutes).
        #[arg(long, hide = true)]
        part_seconds: Option<u32>,
        /// Microphone: part of the device name (default: the system default).
        #[arg(long)]
        mic: Option<String>,
        /// Source of system audio.
        #[arg(long, value_enum, default_value_t = SystemArg::Auto)]
        system: SystemArg,
        /// Windows: record only this program's audio, e.g. Zoom.exe.
        #[arg(long)]
        process: Option<String>,
        /// Do not record system audio (same as --system off).
        #[arg(long, hide = true)]
        no_system: bool,
        /// Echo cancellation of the microphone channel.
        #[arg(long, value_enum)]
        aec: Option<AecArg>,
        /// Total bitrate, kbit/s (both channels).
        #[arg(long)]
        bitrate_kbps: Option<u32>,
    },
    /// List audio devices.
    Devices,
    /// Close sessions left open by a crash.
    Recover,
    /// Decode a session and show durations and channel levels.
    Verify {
        session: PathBuf,
        /// Also save me.wav and them.wav (16 kHz mono) in the session folder.
        #[arg(long)]
        wav: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _log_guard = logging::init(Some(tracing::Level::WARN))?;
    let settings = Settings::load();
    let root = cli.out.clone().unwrap_or_else(|| settings.recordings_dir());
    match cli.cmd {
        Cmd::Start {
            part_minutes,
            part_seconds,
            mic,
            system,
            process,
            no_system,
            aec,
            bitrate_kbps,
        } => {
            let system = match (system, process) {
                _ if no_system => SystemOption::Off,
                (SystemArg::Off, _) => SystemOption::Off,
                (_, Some(p)) => SystemOption::Process(p),
                (SystemArg::Auto, None) => SystemOption::Auto,
                (SystemArg::Tap, None) => SystemOption::ProcessTap,
                (SystemArg::Sck, None) => SystemOption::ScreenCaptureKit,
            };
            let opts = RecorderOptions {
                root,
                part_seconds: part_seconds
                    .unwrap_or(part_minutes.unwrap_or(settings.part_minutes).max(1) * 60),
                bitrate_kbps: bitrate_kbps.unwrap_or(settings.bitrate_kbps),
                mic_device: mic.or(settings.mic_device),
                system,
                aec: match aec {
                    Some(AecArg::Auto) => AecMode::Auto,
                    Some(AecArg::On) => AecMode::On,
                    Some(AecArg::Off) => AecMode::Off,
                    None => settings.aec,
                },
            };
            record(opts)
        }
        Cmd::Devices => {
            devices();
            Ok(())
        }
        Cmd::Recover => {
            let done = recover_all(&root);
            if done.is_empty() {
                println!("No unfinished sessions ({}).", root.display());
            }
            for r in done {
                println!(
                    "Recovered: {}, {} parts, {} s",
                    r.dir.display(),
                    r.parts,
                    r.duration_ms / 1000
                );
            }
            Ok(())
        }
        Cmd::Verify { session, wav } => verify(&session, wav),
    }
}

fn record(opts: RecorderOptions) -> Result<()> {
    println!("Reminder: {CONSENT_REMINDER}");
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("cannot install Ctrl+C handler")?;
    }
    let recorder = Recorder::start(opts)?;
    let st = recorder.status();
    println!("Recording to {}", st.dir.display());
    println!("Press Ctrl+C to stop.");
    let mut announced = false;
    while !stop.load(Ordering::SeqCst) {
        while let Some(n) = recorder.try_notice() {
            eprintln!(
                "\r{}{}",
                if n.is_problem() { "⚠ " } else { "ℹ " },
                n.message()
            );
        }
        let st = recorder.status();
        if !st.running {
            break;
        }
        if !announced && !st.system_capture.is_empty() {
            announced = true;
            eprintln!(
                "System audio: {}, echo cancellation: {}",
                st.system_capture,
                if st.aec_on { "on" } else { "off" }
            );
        }
        print_status(&st);
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!();
    let manifest = recorder.stop()?;
    let seconds: u64 = manifest.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
    println!(
        "Saved: {} parts, {} in {}",
        manifest.parts.len(),
        hms(seconds),
        st.dir.display()
    );
    Ok(())
}

fn print_status(st: &recorder_app::Status) {
    use std::io::Write;
    eprint!(
        "\r● {}  mic {}  system {}  part {:04}  ",
        hms(st.elapsed_ms / 1000),
        meter(st.levels[MIC]),
        meter(st.levels[SYSTEM]),
        st.part,
    );
    let _ = std::io::stderr().flush();
}

fn devices() {
    println!("Microphones:");
    for d in mic::list_inputs() {
        println!("  {d}");
    }
    println!("Output devices:");
    #[cfg(windows)]
    let outputs = recorder_capture::wasapi_loopback::list_render_devices();
    #[cfg(not(windows))]
    let outputs = output::list_outputs();
    for d in outputs {
        println!("  {d}");
    }
    let o = output::default_output();
    println!(
        "Default output: {} (kind: {}, echo risk: {})",
        o.name.unwrap_or_default(),
        o.kind,
        o.echo_risk
    );
}

fn hms(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn meter(peak: f32) -> String {
    let db = if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        -120.0
    };
    let filled = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 12.0).round() as usize;
    format!(
        "{}{} {:>4.0} dB",
        "█".repeat(filled),
        "░".repeat(12 - filled),
        db.max(-99.0)
    )
}

fn verify(dir: &Path, wav: bool) -> Result<()> {
    let manifest =
        Manifest::load(dir).with_context(|| format!("no manifest.json in {}", dir.display()))?;
    println!(
        "Session {}: status {:?}, system: {}",
        manifest.session_id, manifest.status, manifest.system_capture
    );
    println!(
        "Microphone: {}",
        manifest.devices.mic.as_deref().unwrap_or("?")
    );
    println!(
        "Output: {} ({}, echo: {})",
        manifest.devices.output.as_deref().unwrap_or("?"),
        manifest.devices.output_kind,
        manifest.devices.echo_risk
    );
    let (mut all_l, mut all_r) = (Vec::new(), Vec::new());
    let mut ok = true;
    for p in &manifest.parts {
        let path = dir.join(&p.file);
        match decode_part(&path) {
            Ok(d) => {
                let secs = d.left.len() as f64 / 16_000.0;
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                // Recovered parts end on a page granule, so allow the 1 ms rounding.
                let match_manifest = (d.left.len() as i64 - p.duration_ms as i64 * 16).abs() < 16;
                ok &= match_manifest;
                println!(
                    "  {}  {:>8.2} s  {:>6.2} MB ({:.1} MB/h)  L {:>6.1} dBFS  R {:>6.1} dBFS  {}{}",
                    p.file,
                    secs,
                    size as f64 / 1e6,
                    if secs > 0.0 {
                        size as f64 / secs * 3600.0 / 1e6
                    } else {
                        0.0
                    },
                    rms_dbfs(&d.left),
                    rms_dbfs(&d.right),
                    match (d.complete, p.status) {
                        (true, _) => "closed",
                        (false, PartStatus::Recovered) => "recovered after a crash",
                        (false, _) => "no EOS",
                    },
                    if match_manifest {
                        ""
                    } else {
                        "  ≠ manifest!"
                    },
                );
                if wav {
                    all_l.extend(d.left);
                    all_r.extend(d.right);
                }
            }
            Err(e) => {
                ok = false;
                println!("  {}  ERROR: {e}", p.file);
            }
        }
    }
    if wav {
        write_wav(&dir.join("me.wav"), &all_l)?;
        write_wav(&dir.join("them.wav"), &all_r)?;
        println!("Saved me.wav and them.wav in {}", dir.display());
    }
    println!(
        "{}",
        if ok {
            "OK: all parts decode and durations match the manifest."
        } else {
            "Problems found, see above."
        }
    );
    Ok(())
}
