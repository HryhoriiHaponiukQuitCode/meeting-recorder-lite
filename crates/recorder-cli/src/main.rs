//! `recorder` — console prototype of Meeting Recorder.
//!
//! ```text
//! recorder start            record mic (L) + system audio (R) until Ctrl+C
//! recorder devices          list audio devices
//! recorder recover          close sessions left open by a crash
//! recorder verify <dir>     decode a session and report levels; --wav exports me.wav / them.wav
//! ```

mod procstat;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use recorder_capture::{
    BlockConsumer, CaptureEvent, Source, SourceHandle, block_channel, host_now_ns, mic, output,
};
use recorder_core::decode::{decode_part, rms_dbfs, write_wav};
use recorder_core::engine::{DEFAULT_LATENCY_MS, Engine};
use recorder_core::manifest::{Manifest, OsInfo};
use recorder_core::opus_ogg::EncoderConfig;
use recorder_core::session::{SessionConfig, SessionWriter};
use recorder_core::{MIC, SYSTEM, recovery};

const APP_DIR: &str = "Meeting Recorder";

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

#[derive(Subcommand)]
enum Cmd {
    /// Почати запис; Ctrl+C — зупинити.
    Start {
        /// Довжина однієї частини, хвилин.
        #[arg(long, default_value_t = 10)]
        part_minutes: u32,
        /// Довжина частини в секундах (для тестів; перекриває --part-minutes).
        #[arg(long, hide = true)]
        part_seconds: Option<u32>,
        /// Мікрофон: частина назви пристрою (за замовчуванням — системний).
        #[arg(long)]
        mic: Option<String>,
        /// Windows: записувати звук лише цієї програми, напр. Zoom.exe.
        #[arg(long)]
        process: Option<String>,
        /// Не записувати системний звук (правий канал буде тишею).
        #[arg(long)]
        no_system: bool,
        /// Сумарний бітрейт, кбіт/с (обидва канали).
        #[arg(long, default_value_t = 32)]
        bitrate_kbps: u32,
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
    let _log_guard = init_logging()?;
    let root = cli.out.clone().unwrap_or_else(default_recordings_dir);
    match cli.cmd {
        Cmd::Start {
            part_minutes,
            part_seconds,
            mic,
            process,
            no_system,
            bitrate_kbps,
        } => {
            let part_seconds = part_seconds.unwrap_or(part_minutes.max(1) * 60);
            record(&root, part_seconds, mic, process, no_system, bitrate_kbps)
        }
        Cmd::Devices => {
            devices();
            Ok(())
        }
        Cmd::Recover => {
            let done = recovery::recover_all(&root);
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

fn default_recordings_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
        .join("recordings")
}

fn log_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .unwrap_or_default()
            .join("Library/Logs")
            .join(APP_DIR)
    } else {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_DIR)
            .join("logs")
    }
}

fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};
    let dir = log_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create log dir {}", dir.display()))?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("recorder")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("RECORDER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        )
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), log_dir = %dir.display(), "recorder started");
    Ok(guard)
}

fn os_info() -> OsInfo {
    let info = os_info::get();
    OsInfo {
        family: std::env::consts::OS.to_string(),
        version: info.version().to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
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

#[cfg(windows)]
fn start_system(
    producer: recorder_capture::SharedProducer,
    events: mpsc::Sender<CaptureEvent>,
    process: Option<String>,
) -> Option<(SourceHandle, &'static str)> {
    use recorder_capture::wasapi_loopback::{LoopbackOptions, start};
    let method = if process.is_some() {
        "wasapi_process_loopback"
    } else {
        "wasapi_loopback"
    };
    Some((start(producer, events, LoopbackOptions { process }), method))
}

#[cfg(not(windows))]
fn start_system(
    _producer: recorder_capture::SharedProducer,
    _events: mpsc::Sender<CaptureEvent>,
    _process: Option<String>,
) -> Option<(SourceHandle, &'static str)> {
    eprintln!(
        "Увага: захоплення системного звуку на цій ОС з'явиться на етапі 3; правий канал буде тишею."
    );
    None
}

#[derive(Default)]
struct SourceState {
    failing: bool,
    last_sound: Option<Instant>,
}

fn record(
    root: &Path,
    part_seconds: u32,
    mic_device: Option<String>,
    process: Option<String>,
    no_system: bool,
    bitrate_kbps: u32,
) -> Result<()> {
    std::fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    for r in recovery::recover_all(root) {
        println!(
            "Відновлено сесію після збою: {} ({} с)",
            r.dir.display(),
            r.duration_ms / 1000
        );
    }
    println!(
        "Нагадування: ви відповідаєте за отримання згоди всіх учасників на запис (GDPR, місцеве законодавство)."
    );

    let cfg = SessionConfig {
        root: root.to_path_buf(),
        part_seconds,
        encoder: EncoderConfig {
            bitrate: bitrate_kbps as i32 * 1000,
            complexity: 5,
        },
        os: os_info(),
    };
    let mut session = SessionWriter::create(cfg).context("cannot create session")?;
    let out = output::default_output();
    tracing::info!(dir = %session.dir().display(), output = ?out.name, kind = out.kind, echo_risk = out.echo_risk, "session created");

    let (ev_tx, ev_rx) = mpsc::channel();
    let (mic_p, mut mic_c) = block_channel(4);
    let (sys_p, mut sys_c) = block_channel(4);
    let t0 = host_now_ns();
    let mic_handle = mic::start(
        mic_p.into_shared(),
        ev_tx.clone(),
        mic::MicOptions { device: mic_device },
    );
    let system = if no_system {
        None
    } else {
        start_system(sys_p.into_shared(), ev_tx.clone(), process)
    };
    session.update_manifest(|m| {
        m.devices.output = out.name.clone();
        m.devices.output_kind = out.kind.into();
        m.devices.echo_risk = out.echo_risk.into();
        m.system_capture = system.as_ref().map_or("none", |s| s.1).into();
    })?;
    let mut engine = Engine::new(session, t0, DEFAULT_LATENCY_MS);
    if out.echo_risk == "likely" {
        println!(
            "Звук іде в колонки: мікрофон може записати й співрозмовників. Краще використовувати навушники."
        );
    }
    println!("Запис: {}", engine.session().dir().display());
    println!("Ctrl+C — зупинити.\n");

    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    ctrlc::set_handler(move || stop2.store(true, Ordering::SeqCst))
        .context("cannot install Ctrl+C handler")?;

    let mut buf = Vec::with_capacity(192_000);
    let mut states = [SourceState::default(), SourceState::default()];
    let started = Instant::now();
    let mut last_status = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_proc = (Instant::now(), procstat::sample());
    let mut peaks = [0f32; 2];
    let mut warned_silent_system = false;
    let mut write_error = None;

    while !stop.load(Ordering::SeqCst) {
        while let Ok(ev) = ev_rx.try_recv() {
            handle_event(&mut engine, &mut states, ev);
        }
        drain(&mut engine, &mut mic_c, MIC, &mut buf);
        drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
        if let Err(e) = engine.tick(host_now_ns()) {
            write_error = Some(e);
            break;
        }

        let p = engine.take_peaks();
        for ch in 0..2 {
            peaks[ch] = peaks[ch].max(p[ch]);
            if p[ch] > 0.003 {
                states[ch].last_sound = Some(Instant::now());
            }
        }
        if last_status.elapsed() >= Duration::from_millis(500) {
            last_status = Instant::now();
            print_status(&engine, started, peaks);
            peaks = [0.0; 2];
            let sys_quiet = states[SYSTEM]
                .last_sound
                .is_none_or(|t| t.elapsed() > Duration::from_secs(120));
            let mic_active = states[MIC]
                .last_sound
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            if system.is_some()
                && !warned_silent_system
                && sys_quiet
                && mic_active
                && started.elapsed() > Duration::from_secs(120)
            {
                warned_silent_system = true;
                eprintln!(
                    "\nУвага: 2 хв не чути системного звуку (співрозмовників), хоча мікрофон працює. Перевірте пристрій виводу."
                );
                tracing::warn!("system channel silent for 2 minutes while mic is active");
            }
        }
        if last_stats.elapsed() >= Duration::from_secs(60) {
            last_stats = Instant::now();
            log_stats(&engine, &mic_c, &sys_c, &mut last_proc);
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    eprintln!();
    mic_handle.stop();
    if let Some((h, _)) = system {
        h.stop();
    }
    drain(&mut engine, &mut mic_c, MIC, &mut buf);
    drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
    log_stats(&engine, &mic_c, &sys_c, &mut last_proc);
    if let Some(e) = write_error {
        tracing::error!(error = %e, "write failed, stopping");
        eprintln!(
            "Помилка запису на диск: {e}. Запис зупинено; записане до цього моменту збережено."
        );
    }
    let dir = engine.session().dir().to_path_buf();
    let manifest = engine
        .finish(host_now_ns())
        .context("cannot finish session")?;
    let total: u64 = manifest.parts.iter().map(|p| p.duration_ms).sum();
    println!(
        "Збережено: {} — {} частин, {}",
        dir.display(),
        manifest.parts.len(),
        hms(total / 1000)
    );
    tracing::info!(dir = %dir.display(), parts = manifest.parts.len(), seconds = total / 1000, "session completed");
    Ok(())
}

fn drain(engine: &mut Engine, consumer: &mut BlockConsumer, channel: usize, buf: &mut Vec<f32>) {
    while let Some(meta) = consumer.pop(buf) {
        engine.ingest(channel, meta.host_ns, meta.rate, buf);
    }
}

fn handle_event(engine: &mut Engine, states: &mut [SourceState; 2], ev: CaptureEvent) {
    let session = engine.session_mut();
    let result = match ev {
        CaptureEvent::Opened {
            source,
            device,
            rate,
            channels,
        } => {
            tracing::info!(source = source.as_str(), %device, rate, channels, "device opened");
            states[idx(source)].failing = false;
            // The manifest names the device in use most recently; the full
            // history is in the events.
            let name = device.clone();
            let r = session.update_manifest(|m| match source {
                Source::Mic => m.devices.mic = Some(name),
                Source::System if !m.system_capture.contains("process") => {
                    m.devices.output = Some(name)
                }
                Source::System => {}
            });
            r.and_then(|_| session.event("device_opened", serde_json::json!({ "source": source.as_str(), "device": device, "rate": rate, "channels": channels })))
        }
        CaptureEvent::Lost { source, reason } => {
            tracing::warn!(source = source.as_str(), %reason, "device lost, reconnecting");
            session.event(
                "device_lost",
                serde_json::json!({ "source": source.as_str(), "reason": reason }),
            )
        }
        CaptureEvent::Failed {
            source,
            error,
            permission_denied,
        } => {
            let st = &mut states[idx(source)];
            if st.failing {
                tracing::debug!(source = source.as_str(), %error, "device still unavailable");
                return;
            }
            st.failing = true;
            tracing::error!(source = source.as_str(), %error, permission_denied, "cannot open device, retrying");
            if permission_denied {
                eprintln!(
                    "\nНемає доступу до мікрофона. Windows: Параметри → Конфіденційність → Мікрофон → дозволити класичним програмам. macOS: Системні параметри → Конфіденційність → Мікрофон."
                );
            } else {
                eprintln!(
                    "\n{}: не вдається відкрити пристрій ({error}). Повторюю спробу…",
                    label(source)
                );
            }
            session.event("device_failed", serde_json::json!({ "source": source.as_str(), "error": error, "permission_denied": permission_denied }))
        }
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "cannot save manifest");
    }
}

fn idx(s: Source) -> usize {
    match s {
        Source::Mic => MIC,
        Source::System => SYSTEM,
    }
}

fn label(s: Source) -> &'static str {
    match s {
        Source::Mic => "Мікрофон",
        Source::System => "Системний звук",
    }
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

fn print_status(engine: &Engine, started: Instant, peaks: [f32; 2]) {
    use std::io::Write;
    let s = engine.session();
    eprint!(
        "\r● {}  мік {}  система {}  частина {:04}  ",
        hms(started.elapsed().as_secs()),
        meter(peaks[MIC]),
        meter(peaks[SYSTEM]),
        s.current_part(),
    );
    let _ = std::io::stderr().flush();
}

fn log_stats(
    engine: &Engine,
    mic_c: &BlockConsumer,
    sys_c: &BlockConsumer,
    last: &mut (Instant, procstat::ProcStat),
) {
    let now = (Instant::now(), procstat::sample());
    let wall = now.0.duration_since(last.0).as_secs_f64().max(1e-3);
    let cpu_core = (now.1.cpu.saturating_sub(last.1.cpu)).as_secs_f64() / wall * 100.0;
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    *last = now;
    for (name, ch, ring) in [("mic", MIC, mic_c), ("system", SYSTEM, sys_c)] {
        let st = engine.track_stats(ch);
        tracing::info!(
            source = name,
            rate = st.source_rate,
            error_ms = format!("{:.2}", st.error_ms),
            correction_ppm = format!("{:.1}", st.correction_ppm),
            inserted_ms = st.inserted_frames / 16,
            dropped_ms = st.dropped_frames / 16,
            resyncs = st.resyncs,
            ring_dropped = ring.dropped_samples(),
            "track stats"
        );
    }
    tracing::info!(
        elapsed_s = engine.session().elapsed_ms() / 1000,
        cpu_one_core_pct = format!("{cpu_core:.2}"),
        cpu_total_pct = format!("{:.2}", cpu_core / cores),
        rss_mb = format!("{:.1}", now.1.rss_bytes as f64 / 1e6),
        "process stats"
    );
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
