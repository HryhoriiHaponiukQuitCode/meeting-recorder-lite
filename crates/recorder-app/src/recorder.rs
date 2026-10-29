//! One recording session running on its own thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use recorder_capture::power::KeepAwake;
use recorder_capture::{
    BlockConsumer, CaptureEvent, SharedProducer, Source, SourceHandle, block_channel, host_now_ns,
    mic, output,
};
use recorder_core::engine::{DEFAULT_LATENCY_MS, Engine};
use recorder_core::manifest::{Manifest, OsInfo};
use recorder_core::opus_ogg::EncoderConfig;
use recorder_core::recovery::{self, Recovered};
use recorder_core::session::{Gap, SessionConfig, SessionWriter};
use recorder_core::{MIC, SYSTEM};
use serde::{Deserialize, Serialize};

use crate::procstat;

/// Echo cancellation of the microphone channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AecMode {
    /// On while the output device is a loudspeaker.
    #[default]
    Auto,
    On,
    Off,
}

/// Where the right channel comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SystemOption {
    /// The platform default: WASAPI loopback, Core Audio tap or ScreenCaptureKit.
    #[default]
    Auto,
    /// No system audio (right channel silent).
    Off,
    /// Windows: only this executable and its children, e.g. `Zoom.exe`.
    Process(String),
    /// macOS: force the Core Audio process tap.
    ProcessTap,
    /// macOS: force ScreenCaptureKit.
    ScreenCaptureKit,
}

#[derive(Clone, Debug)]
pub struct RecorderOptions {
    pub root: PathBuf,
    /// Split the recording into parts of this many seconds (at least 10);
    /// 0 = one file per session.
    pub part_seconds: u32,
    pub bitrate_kbps: u32,
    pub mic_device: Option<String>,
    pub system: SystemOption,
    pub aec: AecMode,
}

/// Something the user should be told about.
#[derive(Clone, Debug)]
pub enum Notice {
    Recovered(Recovered),
    PermissionDenied(Source),
    DeviceFailed {
        source: Source,
        error: String,
    },
    DeviceChanged {
        source: Source,
        device: String,
    },
    /// The output is a loudspeaker: the microphone may pick up the remote side.
    EchoRisk {
        aec_on: bool,
    },
    /// No system audio for two minutes while the microphone is active.
    SystemSilent,
    /// The system-audio capture delivers nothing at all (not even silence):
    /// the other participants are not being recorded.
    SystemMissing,
    /// System audio arrives again after [`Notice::SystemMissing`].
    SystemBack,
    /// The computer slept; recording continued in the same file.
    Resumed {
        slept_s: u64,
    },
    /// Writing failed (disk full?); the session was closed.
    WriteFailed(String),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub running: bool,
    pub dir: PathBuf,
    pub elapsed_ms: u64,
    /// Smoothed peak level per channel, 0.0..=1.0.
    pub levels: [f32; 2],
    pub part: u32,
    /// The user paused the recording; nothing is written until resumed.
    pub paused: bool,
    pub system_capture: String,
    pub aec_on: bool,
    pub mic_device: Option<String>,
    pub output_device: Option<String>,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    notices: Receiver<Notice>,
    thread: Option<JoinHandle<Result<Manifest>>>,
}

impl Recorder {
    /// Recovers crashed sessions, creates a new session and starts capture.
    pub fn start(opts: RecorderOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.root)
            .with_context(|| format!("cannot create {}", opts.root.display()))?;
        let (notice_tx, notices) = mpsc::channel();
        for r in recovery::recover_all(&opts.root) {
            let _ = notice_tx.send(Notice::Recovered(r));
        }
        let cfg = SessionConfig {
            root: opts.root.clone(),
            part_seconds: match opts.part_seconds {
                0 => 0,
                s => s.max(10),
            },
            encoder: EncoderConfig {
                bitrate: opts.bitrate_kbps.clamp(24, 64) as i32 * 1000,
                complexity: 5,
            },
            os: os_info(),
        };
        let session = SessionWriter::create(cfg).context("cannot create the session folder")?;
        let status = Arc::new(Mutex::new(Status {
            running: true,
            dir: session.dir().to_path_buf(),
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, pause, status) = (stop.clone(), pause.clone(), status.clone());
            std::thread::Builder::new()
                .name("recorder".into())
                .spawn(move || {
                    let result = run(session, opts, stop, pause, status.clone(), notice_tx);
                    status.lock().expect("status").running = false;
                    if let Err(e) = &result {
                        tracing::error!(error = %e, "recording thread failed");
                    }
                    result
                })
                .context("cannot start the recording thread")?
        };
        Ok(Self {
            stop,
            pause,
            status,
            notices,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> Status {
        self.status.lock().expect("status").clone()
    }

    /// Pauses or resumes. Capture keeps running, but nothing is written
    /// while paused; the recording continues in the same file, and the
    /// paused time is not filled with silence.
    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::SeqCst);
    }

    pub fn try_notice(&self) -> Option<Notice> {
        self.notices.try_recv().ok()
    }

    /// Stops capture, writes everything captured and closes the session.
    pub fn stop(mut self) -> Result<Manifest> {
        self.stop.store(true, Ordering::SeqCst);
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(r)) => r,
            Some(Err(_)) => anyhow::bail!("recording thread panicked"),
            None => anyhow::bail!("already stopped"),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn os_info() -> OsInfo {
    let info = os_info::get();
    OsInfo {
        family: std::env::consts::OS.to_string(),
        version: info.version().to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

pub(crate) fn start_system(
    opt: &SystemOption,
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
) -> Option<(SourceHandle, &'static str)> {
    if *opt == SystemOption::Off {
        return None;
    }
    #[cfg(windows)]
    {
        use recorder_capture::wasapi_loopback::{LoopbackOptions, start};
        let process = match opt {
            SystemOption::Process(p) => Some(p.clone()),
            _ => None,
        };
        let method = if process.is_some() {
            "wasapi_process_loopback"
        } else {
            "wasapi_loopback"
        };
        Some((start(producer, events, LoopbackOptions { process }), method))
    }
    #[cfg(target_os = "macos")]
    {
        use recorder_capture::macos::{SystemCapture, start_system};
        let mode = match opt {
            SystemOption::ProcessTap => SystemCapture::ProcessTap,
            SystemOption::ScreenCaptureKit => SystemCapture::ScreenCaptureKit,
            _ => SystemCapture::Auto,
        };
        Some(start_system(producer, events, mode))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (producer, events);
        None
    }
}

/// Feeds every captured block to the engine; returns how many there were.
fn drain(
    engine: &mut Engine,
    consumer: &mut BlockConsumer,
    channel: usize,
    buf: &mut Vec<f32>,
) -> usize {
    let mut blocks = 0;
    while let Some(meta) = consumer.pop(buf) {
        engine.ingest(channel, meta.host_ns, meta.rate, buf);
        blocks += 1;
    }
    blocks
}

/// No system-audio block for this long means the capture is broken (only
/// for backends that deliver blocks in silence too).
const NO_SYSTEM_AUDIO_AFTER: Duration = Duration::from_secs(5);

/// Tracks whether system audio arrives at all and says when that changes.
#[derive(Debug)]
pub(crate) struct SystemWatch {
    /// Silence still arrives as blocks with this backend.
    enabled: bool,
    last_block: Instant,
    missing: bool,
}

impl SystemWatch {
    pub(crate) fn new(method: &str, now: Instant) -> Self {
        Self {
            enabled: method == "coreaudio_process_tap",
            last_block: now,
            missing: false,
        }
    }

    /// After a pause or sleep the wait starts again.
    pub(crate) fn restart(&mut self, now: Instant) {
        self.last_block = now;
    }

    /// `Some(true)`: system audio just went missing; `Some(false)`: it is
    /// back; `None`: no change.
    pub(crate) fn update(&mut self, blocks: usize, now: Instant) -> Option<bool> {
        if !self.enabled {
            return None;
        }
        if blocks > 0 {
            self.last_block = now;
            if self.missing {
                self.missing = false;
                return Some(false);
            }
        } else if !self.missing && now.duration_since(self.last_block) >= NO_SYSTEM_AUDIO_AFTER {
            self.missing = true;
            return Some(true);
        }
        None
    }
}

/// Throws captured audio away (while paused).
fn discard(consumer: &mut BlockConsumer, buf: &mut Vec<f32>) {
    while consumer.pop(buf).is_some() {}
}

/// Output device kind → echo-risk bookkeeping and automatic AEC.
fn apply_output(engine: &mut Engine, mode: AecMode, notices: &Sender<Notice>, announce: bool) {
    let out = output::default_output();
    let aec = match mode {
        AecMode::On => true,
        AecMode::Off => false,
        AecMode::Auto => out.echo_risk == "likely",
    };
    let changed = engine.aec_enabled() != aec;
    engine.set_aec(aec);
    let r = engine.session_mut().update_manifest(|m| {
        m.devices.output = out.name.clone();
        m.devices.output_kind = out.kind.into();
        m.devices.echo_risk = out.echo_risk.into();
        m.devices.aec = if aec { "on" } else { "off" }.into();
    });
    if let Err(e) = r {
        tracing::error!(error = %e, "cannot save manifest");
    }
    if changed {
        tracing::info!(aec, output = ?out.name, kind = out.kind, "echo cancellation updated");
        let _ = engine.session_mut().event(
            "aec",
            serde_json::json!({ "on": aec, "output_kind": out.kind }),
        );
    }
    if announce && out.echo_risk == "likely" {
        let _ = notices.send(Notice::EchoRisk { aec_on: aec });
    }
}

#[derive(Default)]
struct SourceState {
    failing: bool,
    /// Permission was refused; reported once. Capture may still "open" and
    /// deliver silence (macOS), which is not a device change.
    denied: bool,
    last_sound: Option<Instant>,
}

fn run(
    mut session: SessionWriter,
    opts: RecorderOptions,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    notices: Sender<Notice>,
) -> Result<Manifest> {
    let _awake = KeepAwake::new("Meeting Recorder is recording a call");

    #[cfg(target_os = "macos")]
    if recorder_capture::macos::permissions::ensure_microphone()
        == recorder_capture::macos::permissions::Access::Denied
    {
        tracing::error!("microphone permission denied");
        let _ = notices.send(Notice::PermissionDenied(Source::Mic));
    }

    let (ev_tx, ev_rx) = mpsc::channel();
    let (mic_p, mut mic_c) = block_channel(4);
    let (sys_p, mut sys_c) = block_channel(4);
    let t0 = host_now_ns();
    let mic_handle = mic::start(
        mic_p.into_shared(),
        ev_tx.clone(),
        mic::MicOptions {
            device: opts.mic_device.clone(),
        },
    );
    let system = start_system(&opts.system, sys_p.into_shared(), ev_tx.clone());
    let method = system.as_ref().map_or("none", |s| s.1);
    session.update_manifest(|m| m.system_capture = method.into())?;
    tracing::info!(dir = %session.dir().display(), system = method, "recording");

    let mut engine = Engine::new(session, t0, DEFAULT_LATENCY_MS);
    apply_output(&mut engine, opts.aec, &notices, true);
    {
        let mut st = status.lock().expect("status");
        st.system_capture = method.into();
        st.aec_on = engine.aec_enabled();
        st.output_device = engine.session().manifest().devices.output.clone();
    }

    let mut buf = Vec::with_capacity(192_000);
    let mut states = [SourceState::default(), SourceState::default()];
    let started = Instant::now();
    let mut levels = [0f32; 2];
    let mut last_ui = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_proc = (Instant::now(), procstat::sample());
    let mut warned_silent = false;
    let mut system_watch = SystemWatch::new(method, Instant::now());
    let mut last_wall = SystemTime::now();
    let mut last_host = host_now_ns();
    let mut write_error = None;
    // When the user paused, and the last host time before that.
    let mut paused_at: Option<(Instant, u64)> = None;

    while !stop.load(Ordering::SeqCst) {
        match (pause.load(Ordering::SeqCst), paused_at) {
            (true, None) => {
                // Everything captured so far stays; the uncommitted tail is
                // written on resume or stop.
                drain(&mut engine, &mut mic_c, MIC, &mut buf);
                drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
                paused_at = Some((Instant::now(), host_now_ns()));
                levels = [0.0; 2];
                let mut st = status.lock().expect("status");
                st.paused = true;
                st.levels = levels;
                tracing::info!(elapsed_ms = st.elapsed_ms, "paused");
            }
            (false, Some((since, before))) => {
                discard(&mut mic_c, &mut buf);
                discard(&mut sys_c, &mut buf);
                let paused_ms = since.elapsed().as_millis() as u64;
                tracing::info!(paused_s = paused_ms / 1000, "resumed");
                if let Err(e) =
                    engine.resume_after_pause(Gap::Pause, before, host_now_ns(), paused_ms)
                {
                    write_error = Some(e);
                    break;
                }
                paused_at = None;
                // Silence during the pause says nothing about the call.
                if let Some(t) = &mut states[SYSTEM].last_sound {
                    *t = Instant::now();
                }
                status.lock().expect("status").paused = false;
            }
            _ => {}
        }
        if paused_at.is_some() {
            // Devices may still change while paused; the sleep check is
            // skipped because the pause already skips the gap.
            while let Ok(ev) = ev_rx.try_recv() {
                handle_event(&mut engine, &mut states, &opts, &notices, &status, ev);
            }
            discard(&mut mic_c, &mut buf);
            discard(&mut sys_c, &mut buf);
            last_wall = SystemTime::now();
            last_host = host_now_ns();
            system_watch.restart(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        // A long gap in wall time means the computer slept (or the process
        // was suspended): continue in the same file without recording the gap.
        let wall = SystemTime::now();
        let gap = wall.duration_since(last_wall).unwrap_or_default();
        if gap > Duration::from_secs(3) {
            drain(&mut engine, &mut mic_c, MIC, &mut buf);
            drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
            tracing::warn!(
                seconds = gap.as_secs(),
                "system was asleep, skipping the gap"
            );
            if let Err(e) = engine.resume_after_pause(
                Gap::Sleep,
                last_host,
                host_now_ns(),
                gap.as_millis() as u64,
            ) {
                write_error = Some(e);
                break;
            }
            system_watch.restart(Instant::now());
            let _ = notices.send(Notice::Resumed {
                slept_s: gap.as_secs(),
            });
        }
        last_wall = wall;
        last_host = host_now_ns();

        while let Ok(ev) = ev_rx.try_recv() {
            handle_event(&mut engine, &mut states, &opts, &notices, &status, ev);
        }
        drain(&mut engine, &mut mic_c, MIC, &mut buf);
        let system_blocks = drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
        if let Err(e) = engine.tick(host_now_ns()) {
            write_error = Some(e);
            break;
        }
        match system_watch.update(system_blocks, Instant::now()) {
            Some(true) => {
                tracing::error!("no system audio arrives: the other side is not being recorded");
                let _ = engine
                    .session_mut()
                    .event("system_audio_missing", serde_json::Value::Null);
                let _ = notices.send(Notice::SystemMissing);
            }
            Some(false) => {
                tracing::info!("system audio arrives again");
                let _ = engine
                    .session_mut()
                    .event("system_audio_back", serde_json::Value::Null);
                let _ = notices.send(Notice::SystemBack);
            }
            None => {}
        }

        let peaks = engine.take_peaks();
        for ch in 0..2 {
            levels[ch] = peaks[ch].max(levels[ch] * 0.9);
            if peaks[ch] > 0.003 {
                states[ch].last_sound = Some(Instant::now());
            }
        }
        if last_ui.elapsed() >= Duration::from_millis(50) {
            last_ui = Instant::now();
            let mut st = status.lock().expect("status");
            st.elapsed_ms = engine.session().elapsed_ms();
            st.levels = levels;
            st.part = engine.session().current_part();
            st.aec_on = engine.aec_enabled();
            drop(st);
            let sys_quiet = states[SYSTEM]
                .last_sound
                .is_none_or(|t| t.elapsed() > Duration::from_secs(120));
            let mic_active = states[MIC]
                .last_sound
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            if system.is_some()
                && !warned_silent
                && sys_quiet
                && mic_active
                && started.elapsed() > Duration::from_secs(120)
            {
                warned_silent = true;
                tracing::warn!(
                    "system channel silent for 2 minutes while the microphone is active"
                );
                let _ = notices.send(Notice::SystemSilent);
            }
            if !sys_quiet {
                warned_silent = false;
            }
        }
        if last_stats.elapsed() >= Duration::from_secs(60) {
            last_stats = Instant::now();
            log_stats(&engine, &mic_c, &sys_c, &mut last_proc);
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    mic_handle.stop();
    if let Some((h, _)) = system {
        h.stop();
    }
    if paused_at.is_some() {
        discard(&mut mic_c, &mut buf);
        discard(&mut sys_c, &mut buf);
    } else {
        drain(&mut engine, &mut mic_c, MIC, &mut buf);
        drain(&mut engine, &mut sys_c, SYSTEM, &mut buf);
    }
    log_stats(&engine, &mic_c, &sys_c, &mut last_proc);
    if let Some(e) = &write_error {
        tracing::error!(error = %e, "write failed, stopping");
        let _ = notices.send(Notice::WriteFailed(e.to_string()));
    }
    let dir = engine.session().dir().to_path_buf();
    let manifest = engine
        .finish(host_now_ns())
        .context("cannot finish the session")?;
    let seconds: u64 = manifest.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
    tracing::info!(dir = %dir.display(), parts = manifest.parts.len(), seconds, "session completed");
    Ok(manifest)
}

fn handle_event(
    engine: &mut Engine,
    states: &mut [SourceState; 2],
    opts: &RecorderOptions,
    notices: &Sender<Notice>,
    status: &Arc<Mutex<Status>>,
    ev: CaptureEvent,
) {
    let idx = |s: Source| if s == Source::Mic { MIC } else { SYSTEM };
    let result = match ev {
        CaptureEvent::Opened {
            source,
            device,
            rate,
            channels,
        } => {
            tracing::info!(source = source.as_str(), %device, rate, channels, "device opened");
            let was_failing = std::mem::take(&mut states[idx(source)].failing);
            // The device this source last opened; a reopen of the same device
            // (stream error, sample-rate change) is not worth telling about.
            let previous = engine
                .session()
                .manifest()
                .events
                .iter()
                .rev()
                .find(|e| {
                    e.kind == "device_opened"
                        && e.data.get("source").and_then(|s| s.as_str()) == Some(source.as_str())
                })
                .and_then(|e| e.data.get("device")?.as_str().map(str::to_owned));
            let changed = previous.as_ref().is_some_and(|p| *p != device);
            if source == Source::Mic {
                let name = device.clone();
                let _ = engine
                    .session_mut()
                    .update_manifest(|m| m.devices.mic = Some(name));
                status.lock().expect("status").mic_device = Some(device.clone());
            } else {
                apply_output(engine, opts.aec, notices, changed);
                status.lock().expect("status").output_device =
                    engine.session().manifest().devices.output.clone();
            }
            if changed || was_failing {
                let _ = notices.send(Notice::DeviceChanged {
                    source,
                    device: device.clone(),
                });
            }
            engine.session_mut().event(
                "device_opened",
                serde_json::json!({ "source": source.as_str(), "device": device, "rate": rate, "channels": channels }),
            )
        }
        CaptureEvent::Lost { source, reason } => {
            tracing::warn!(source = source.as_str(), %reason, "device lost, reconnecting");
            engine.session_mut().event(
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
            let seen = if permission_denied {
                std::mem::replace(&mut st.denied, true)
            } else {
                std::mem::replace(&mut st.failing, true)
            };
            if seen {
                tracing::debug!(source = source.as_str(), %error, "device still unavailable");
                return;
            }
            tracing::error!(source = source.as_str(), %error, permission_denied, "cannot open device, retrying");
            let _ = notices.send(if permission_denied {
                Notice::PermissionDenied(source)
            } else {
                Notice::DeviceFailed {
                    source,
                    error: error.clone(),
                }
            });
            engine.session_mut().event(
                "device_failed",
                serde_json::json!({ "source": source.as_str(), "error": error, "permission_denied": permission_denied }),
            )
        }
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "cannot save manifest");
    }
}

fn log_stats(
    engine: &Engine,
    mic_c: &BlockConsumer,
    sys_c: &BlockConsumer,
    last: &mut (Instant, procstat::ProcStat),
) {
    let now = (Instant::now(), procstat::sample());
    let wall = now.0.duration_since(last.0).as_secs_f64().max(1e-3);
    let cpu_core = now.1.cpu.saturating_sub(last.1.cpu).as_secs_f64() / wall * 100.0;
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
        aec = engine.aec_enabled(),
        cpu_one_core_pct = format!("{cpu_core:.2}"),
        cpu_total_pct = format!("{:.2}", cpu_core / cores),
        rss_mb = format!("{:.1}", now.1.rss_bytes as f64 / 1e6),
        "process stats"
    );
}

impl Notice {
    /// Ukrainian text for the console and the tray.
    pub fn message(&self) -> String {
        let name = |s: &Source| {
            if *s == Source::Mic {
                "microphone"
            } else {
                "system audio"
            }
        };
        match self {
            Notice::Recovered(r) => format!(
                "Recovered a recording after a crash: {} ({} s)",
                r.dir.file_name().unwrap_or_default().to_string_lossy(),
                r.duration_ms / 1000
            ),
            Notice::PermissionDenied(Source::Mic) => {
                "No permission to use the microphone. Allow access in Privacy settings.".into()
            }
            Notice::PermissionDenied(Source::System) => {
                "No permission to record system audio. Allow access in Privacy settings.".into()
            }
            Notice::DeviceFailed { source, error } => {
                format!("Cannot open the {}: {error}. Retrying…", name(source))
            }
            Notice::DeviceChanged { source, device } => format!("{}: switched to “{device}”", name(source)),
            Notice::EchoRisk { aec_on: true } => {
                "Sound is playing through speakers, so echo cancellation is on. Use headphones for the best quality.".into()
            }
            Notice::EchoRisk { aec_on: false } => {
                "Sound is playing through speakers, so the microphone will also pick up the others. Headphones are better.".into()
            }
            Notice::SystemSilent => {
                "System audio has been silent for 2 minutes while the microphone is active. Check the output device.".into()
            }
            Notice::SystemMissing => {
                "The other participants are not being recorded: no system audio arrives. The app keeps reconnecting; your microphone is still recorded.".into()
            }
            Notice::SystemBack => "System audio is being recorded again.".into(),
            Notice::Resumed { slept_s } => {
                format!("The computer slept for {slept_s} s; recording continued in the same file.")
            }
            Notice::WriteFailed(e) => format!("Cannot write to disk: {e}. Recording stopped."),
        }
    }

    /// Problems the user must act on (as opposed to information).
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Notice::PermissionDenied(_)
                | Notice::DeviceFailed { .. }
                | Notice::SystemSilent
                | Notice::SystemMissing
                | Notice::WriteFailed(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_system_audio_is_reported_once_and_its_return_too() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut w = SystemWatch::new("coreaudio_process_tap", t0);
        // Blocks every 20 ms: fine.
        for i in 1..100 {
            assert_eq!(w.update(2, at(i * 20)), None);
        }
        // Nothing for 5 s after the last block at 1980 ms.
        assert_eq!(w.update(0, at(1980 + 4900)), None);
        assert_eq!(w.update(0, at(1980 + 5000)), Some(true));
        assert_eq!(w.update(0, at(20_000)), None, "reported once");
        assert_eq!(w.update(3, at(20_020)), Some(false));
        assert_eq!(w.update(3, at(20_040)), None);
    }

    #[test]
    fn a_pause_restarts_the_wait_and_other_backends_are_not_watched() {
        let t0 = Instant::now();
        let mut w = SystemWatch::new("coreaudio_process_tap", t0);
        w.restart(t0 + Duration::from_secs(60));
        assert_eq!(w.update(0, t0 + Duration::from_secs(63)), None);
        // WASAPI loopback delivers nothing while nothing plays.
        let mut w = SystemWatch::new("wasapi_loopback", t0);
        assert_eq!(w.update(0, t0 + Duration::from_secs(600)), None);
    }
}
