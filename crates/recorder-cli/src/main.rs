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
use recorder_core::manifest::Manifest;
use recorder_core::{MIC, SYSTEM};

#[derive(Parser)]
#[command(
    name = "recorder",
    version,
    about = "Meeting Recorder: записує мікрофон (L) і системний звук (R) в Ogg Opus"
)]
struct Cli {
    /// Папка із записами (за замовчуванням Documents/Meeting Recorder/recordings).
    #[arg(long, global = true)]
    out: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum SystemArg {
    /// Типовий спосіб для ОС.
    Auto,
    /// macOS 14.4+: Core Audio process tap.
    Tap,
    /// macOS: ScreenCaptureKit.
    Sck,
    /// Без системного звуку.
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum AecArg {
    /// Лише коли звук іде в динаміки.
    Auto,
    On,
    Off,
}

#[derive(Subcommand)]
enum Cmd {
    /// Почати запис; Ctrl+C — зупинити.
    Start {
        /// Довжина однієї частини, хвилин.
        #[arg(long)]
        part_minutes: Option<u32>,
        /// Довжина частини в секундах (для тестів; перекриває --part-minutes).
        #[arg(long, hide = true)]
        part_seconds: Option<u32>,
        /// Мікрофон: частина назви пристрою (за замовчуванням — системний).
        #[arg(long)]
        mic: Option<String>,
        /// Джерело системного звуку.
        #[arg(long, value_enum, default_value_t = SystemArg::Auto)]
        system: SystemArg,
        /// Windows: записувати звук лише цієї програми, напр. Zoom.exe.
        #[arg(long)]
        process: Option<String>,
        /// Не записувати системний звук (те саме, що --system off).
        #[arg(long, hide = true)]
        no_system: bool,
        /// Придушення луни в каналі мікрофона.
        #[arg(long, value_enum)]
        aec: Option<AecArg>,
        /// Сумарний бітрейт, кбіт/с (обидва канали).
        #[arg(long)]
        bitrate_kbps: Option<u32>,
    },
    /// Показати аудіопристрої.
    Devices,
    /// Закрити сесії, що лишилися відкритими після збою.
    Recover,
    /// Декодувати сесію і показати тривалість та рівні каналів.
    Verify {
        session: PathBuf,
        /// Також зберегти me.wav і them.wav (16 кГц моно) у папку сесії.
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
                println!("Незакритих сесій немає ({}).", root.display());
            }
            for r in done {
                println!(
                    "Відновлено: {} — частин: {}, {} с",
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
    println!("Нагадування: {CONSENT_REMINDER}");
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("cannot install Ctrl+C handler")?;
    }
    let recorder = Recorder::start(opts)?;
    let st = recorder.status();
    println!("Запис у {}", st.dir.display());
    println!("Ctrl+C — зупинити.");
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
                "Системний звук: {}, придушення луни: {}",
                st.system_capture,
                if st.aec_on {
                    "увімк."
                } else {
                    "вимк."
                }
            );
        }
        print_status(&st);
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!();
    let manifest = recorder.stop()?;
    let seconds: u64 = manifest.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
    println!(
        "Збережено: {} частин, {} — {}",
        manifest.parts.len(),
        hms(seconds),
        st.dir.display()
    );
    Ok(())
}

fn print_status(st: &recorder_app::Status) {
    use std::io::Write;
    eprint!(
        "\r● {}  мік {}  система {}  частина {:04}  ",
        hms(st.elapsed_ms / 1000),
        meter(st.levels[MIC]),
        meter(st.levels[SYSTEM]),
        st.part,
    );
    let _ = std::io::stderr().flush();
}

fn devices() {
    println!("Мікрофони:");
    for d in mic::list_inputs() {
        println!("  {d}");
    }
    println!("Пристрої виводу:");
    #[cfg(windows)]
    let outputs = recorder_capture::wasapi_loopback::list_render_devices();
    #[cfg(not(windows))]
    let outputs = output::list_outputs();
    for d in outputs {
        println!("  {d}");
    }
    let o = output::default_output();
    println!(
        "Вивід за замовчуванням: {} (тип: {}, ризик луни: {})",
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
        "Сесія {} — статус {:?}, система: {}",
        manifest.session_id, manifest.status, manifest.system_capture
    );
    println!(
        "Мікрофон: {}",
        manifest.devices.mic.as_deref().unwrap_or("?")
    );
    println!(
        "Вивід: {} ({}, луна: {})",
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
                let match_manifest = d.left.len() as u64 == p.duration_ms * 16;
                ok &= match_manifest;
                println!(
                    "  {}  {:>8.2} с  {:>6.2} МБ ({:.1} МБ/год)  L {:>6.1} dBFS  R {:>6.1} dBFS  {}{}",
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
                    if d.complete {
                        "закрита"
                    } else {
                        "без EOS"
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
                println!("  {}  ПОМИЛКА: {e}", p.file);
            }
        }
    }
    if wav {
        write_wav(&dir.join("me.wav"), &all_l)?;
        write_wav(&dir.join("them.wav"), &all_r)?;
        println!("Збережено me.wav і them.wav у {}", dir.display());
    }
    println!(
        "{}",
        if ok {
            "OK: усі частини декодуються, тривалості збігаються з manifest."
        } else {
            "Є проблеми — див. вище."
        }
    );
    Ok(())
}
