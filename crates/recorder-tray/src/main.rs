//! Meeting Recorder window app. On the left: a transport button, timer,
//! channel meters, warnings, the last recording, the list of recordings and
//! the settings. On the right: the transcript of the selected recording
//! (`reader`). All recording work happens in `recorder_app::Recorder` on
//! its own thread; this file is UI.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod assets;
mod reader;
mod ui;

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use eframe::egui::{self, Align, FontId, Layout, RichText, TextureHandle, ViewportCommand, vec2};
use recorder_app::permissions::{self, Access, Permission};
use recorder_app::sessions::{self, SessionInfo};
use recorder_app::settings::Settings;
use recorder_app::transcription::{
    self, JobOutcome, ModelDownload, Phase, Quality, TranscriptState, TranscriptionJob,
};
use recorder_app::{
    APP_NAME, AecMode, CONSENT_REMINDER, Monitor, Notice, Recorder, RecorderOptions, Status,
    SystemOption, logging, paths,
};
use recorder_transcribe::model::ModelStatus;

/// How long the channel check listens.
const CHECK_SECONDS: u64 = 10;
/// A channel "works" when its peak reached −45 dBFS during the check:
/// above room noise, below normal speech or playback.
const CHECK_WORKS: f32 = 0.0056;

/// How often the window refreshes while recording: often enough for smooth
/// level bars, and once a second when minimized (the title timer only).
const REFRESH: Duration = Duration::from_millis(33);
const REFRESH_MINIMIZED: Duration = Duration::from_secs(1);
/// Progress of a transcription or download needs no more than this.
const REFRESH_BACKGROUND: Duration = Duration::from_millis(250);
/// How often transcript and model states are re-read from disk.
const DISK_CHECK: Duration = Duration::from_secs(2);
/// The navigation rail, the section column (recording, list, settings);
/// the transcript takes the rest.
const RAIL_WIDTH: f32 = 84.0;
const LEFT_WIDTH: f32 = 372.0;
/// The list reads this many recordings at first and this many more each
/// time it is scrolled to the end (only these manifests are read).
const LOAD_STEP: usize = 50;
/// The widest the Record and Settings sections get.
const SECTION_MAX: f32 = 620.0;

#[cfg(target_os = "macos")]
const REVEAL: &str = "Show in Finder";
#[cfg(not(target_os = "macos"))]
const REVEAL: &str = "Show in Explorer";

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Settings,
    Recordings,
}

struct Message {
    text: String,
    problem: bool,
}

/// Result of the channel check for one channel.
#[derive(Clone)]
enum Check {
    Works,
    Silent,
    Failed(String),
}

/// A recording being closed on a background thread: capture devices take
/// a moment to shut down, and the window must not freeze meanwhile.
struct Saving {
    dir: PathBuf,
    elapsed_ms: u64,
    thread: std::thread::JoinHandle<Result<recorder_app::Manifest>>,
}

struct LastRecording {
    dir: PathBuf,
    started: DateTime<Utc>,
    seconds: u64,
}

/// A recording's name being edited in the transcript pane.
struct Renaming {
    dir: PathBuf,
    text: String,
    /// Put the cursor in the field on the next frame.
    focus: bool,
}

enum TranscriptAction {
    Start(PathBuf),
    Stop,
    /// Take a waiting recording off the queue.
    Unqueue(PathBuf),
    Open(PathBuf),
    /// Show the transcript of this recording on the right.
    View(PathBuf),
    Reveal(PathBuf),
    SetUp,
    /// Open the list of recordings.
    List,
    /// Read more recordings into the list (scrolled to its end).
    More,
}

/// Hidden developer options (`--smoke-test`, `--screenshot`, …).
#[derive(Default)]
struct DevOptions {
    smoke_test: Option<u64>,
    /// `--smoke-pause 2:5`: pause the smoke test at 2 s, resume at 5 s.
    smoke_pause: Option<(f32, f32)>,
    /// `--check`: start the channel check at launch.
    check: bool,
    screenshot: Option<PathBuf>,
    screenshot_at: Option<f32>,
    view: Option<View>,
    window_height: Option<f32>,
    window_width: Option<f32>,
    /// `--scroll 600`: start scrolled down (screenshots of lower cards).
    scroll: Option<f32>,
    /// `--transcribe`: transcribe the last recording at launch.
    transcribe: bool,
    /// `--show <session folder>` and `--find <text>`: what the transcript
    /// pane shows (screenshots).
    show: Option<PathBuf>,
    find: Option<String>,
    /// `--rename <text>`: the shown recording's name field, open with this.
    rename: Option<String>,
}

struct App {
    settings: Settings,
    view: View,
    recorder: Option<Recorder>,
    status: Option<Status>,
    message: Option<Message>,
    last: Option<LastRecording>,
    logo: TextureHandle,
    show_consent: bool,
    consent_dont_ask: bool,
    confirm_close: bool,
    closing: bool,
    title: String,
    started: Instant,
    smoke_until: Option<Instant>,
    smoke_pause: Option<(f32, f32)>,
    screenshot: Option<(PathBuf, f32)>,
    /// Scroll offset to apply once (`--scroll`).
    scroll: Option<f32>,
    /// The running channel check and when it started.
    check: Option<(Monitor, Instant)>,
    check_result: Option<[Check; 2]>,
    saving: Option<Saving>,
    /// Privacy permissions and when they were last checked.
    permissions: Vec<(Permission, Access)>,
    permissions_checked: Instant,
    /// A system permission prompt is open.
    asking: Arc<AtomicBool>,
    /// The transcriber process at work, and sessions waiting for it.
    job: Option<TranscriptionJob>,
    queue: VecDeque<PathBuf>,
    download: Option<ModelDownload>,
    /// Re-read from disk every [`DISK_CHECK`].
    model_status: ModelStatus,
    model_size: u64,
    last_transcript: TranscriptState,
    /// All recordings, for the list (refreshed while it is shown).
    sessions: Vec<SessionInfo>,
    /// How many recordings the list has read, and how many there are.
    loaded: usize,
    sessions_total: usize,
    disk_checked: Instant,
    /// The recording whose transcript is on the right, and its state.
    reader: reader::Reader,
    shown: Option<SessionInfo>,
    renaming: Option<Renaming>,
    /// The last recording's name, if it was given one.
    last_title: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, dev: DevOptions) -> Self {
        ui::install(&cc.egui_ctx);
        let (px, w, h) = assets::decode(assets::LOGO_PNG);
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px),
            egui::TextureOptions::LINEAR,
        );
        let settings = Settings::load();
        let last = sessions::latest(&settings.recordings_dir()).map(|s| LastRecording {
            dir: s.dir,
            started: s.started,
            seconds: s.seconds,
        });
        let mut app = Self {
            view: dev.view.unwrap_or(View::Main),
            recorder: None,
            status: None,
            message: None,
            last,
            logo,
            show_consent: false,
            consent_dont_ask: false,
            confirm_close: false,
            closing: false,
            title: APP_NAME.to_string(),
            started: Instant::now(),
            smoke_until: None,
            smoke_pause: dev.smoke_pause,
            check: None,
            check_result: None,
            saving: None,
            screenshot: dev
                .screenshot
                .map(|p| (p, dev.screenshot_at.unwrap_or(2.0))),
            scroll: dev.scroll,
            permissions: current_permissions(),
            permissions_checked: Instant::now(),
            asking: Arc::new(AtomicBool::new(false)),
            job: None,
            queue: VecDeque::new(),
            download: None,
            model_status: ModelStatus::Missing,
            model_size: 0,
            last_transcript: TranscriptState::None,
            sessions: Vec::new(),
            loaded: LOAD_STEP,
            sessions_total: 0,
            disk_checked: Instant::now(),
            reader: reader::Reader::default(),
            shown: None,
            renaming: None,
            last_title: None,
            settings,
        };
        // Nothing is opened at start; the user picks a recording.
        app.reader.select(dev.show.clone());
        if let Some(q) = dev.find {
            app.reader.query = q;
        }
        if let (Some(dir), Some(text)) = (dev.show.clone(), dev.rename) {
            app.renaming = Some(Renaming {
                dir,
                text,
                focus: true,
            });
        }
        app.check_disk();
        // First launch: ask for everything right away, so the system prompts
        // appear now and not in the middle of a call. Screenshots run
        // unattended and never prompt.
        if app.screenshot.is_none() {
            let missing: Vec<Permission> = app
                .permissions
                .iter()
                .filter(|(_, a)| *a == Access::NotAsked)
                .map(|(p, _)| *p)
                .collect();
            app.ask(&cc.egui_ctx, missing);
        }
        if dev.check {
            app.start_check();
        }
        if dev.transcribe
            && let Some(dir) = app.last.as_ref().map(|l| l.dir.clone())
        {
            app.enqueue(dir);
        }
        if let Some(seconds) = dev.smoke_test {
            // Checks permissions and capture in the real bundle; skips the
            // consent reminder.
            tracing::info!(seconds, "smoke test");
            app.begin();
            app.smoke_until = Some(Instant::now() + Duration::from_secs(seconds));
        }
        app
    }

    /// Shows the system prompts one after another on a background thread.
    fn ask(&self, ctx: &egui::Context, which: Vec<Permission>) {
        if which.is_empty() || self.asking.swap(true, Ordering::SeqCst) {
            return;
        }
        let asking = self.asking.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for p in which {
                permissions::request(p);
            }
            asking.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    fn permissions_card(&mut self, ui: &mut egui::Ui) {
        if self.permissions.iter().all(|(_, a)| *a == Access::Granted) {
            return;
        }
        let asking = self.asking.load(Ordering::SeqCst);
        let granted = self
            .permissions
            .iter()
            .filter(|(_, a)| *a == Access::Granted)
            .count();
        let total = self.permissions.len();
        let mut request = None;
        ui::card(ui, |ui| {
            ui::card_header(ui, "Permissions needed", |ui| {
                ui::pill(ui, &format!("{granted} of {total}"), ui::Tone::Amber, false);
            });
            ui.label(ui::muted(
                "To record your voice and the other participants.",
            ));
            ui.add_space(ui::S2);
            for (perm, access) in self.permissions.clone() {
                ui.horizontal(|ui| {
                    let icon = match perm {
                        Permission::Microphone => ui::Icon::Mic,
                        Permission::SystemAudio => ui::Icon::Speaker,
                    };
                    let tone = if access == Access::Granted {
                        ui::Tone::Green
                    } else {
                        ui::Tone::Blue
                    };
                    ui::icon_tile(ui, icon, tone);
                    ui.label(RichText::new(perm.title()).font(FontId::new(14.0, ui::medium())));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match access {
                        Access::Granted => {
                            ui::pill(ui, "Allowed", ui::Tone::Green, false);
                        }
                        Access::NotAsked if asking => {
                            ui::pill(ui, "Waiting…", ui::Tone::Grey, false);
                        }
                        Access::NotAsked => {
                            if ui::primary(ui, "Allow", None, None, false).clicked() {
                                request = Some(perm);
                            }
                        }
                        Access::Denied => {
                            if ui::outline(ui, "Turn on", None, false).clicked() {
                                permissions::open_settings(perm);
                            }
                        }
                    });
                });
            }
            if self.permissions.iter().any(|(_, a)| *a == Access::Denied) {
                ui.add_space(ui::S1);
                ui.label(ui::muted(
                    "Access was denied, so macOS will not ask again. “Turn on” opens the right page in System Settings: switch on Meeting Recorder there and the app picks it up at once.",
                ));
            }
        });
        ui.add_space(ui::S3);
        if let Some(perm) = request {
            self.ask(ui.ctx(), vec![perm]);
        }
    }

    fn start(&mut self) {
        if self.recorder.is_some() || self.saving.is_some() {
            return;
        }
        if self.settings.consent_reminder_dismissed {
            self.begin();
        } else {
            self.consent_dont_ask = false;
            self.show_consent = true;
        }
    }

    fn start_check(&mut self) {
        if self.recorder.is_none() {
            self.check_result = None;
            self.check = Some((
                Monitor::start(self.settings.mic_device.clone()),
                Instant::now(),
            ));
        }
    }

    /// Ends the channel check and keeps its verdict for each channel.
    fn finish_check(&mut self) {
        let Some((monitor, _)) = self.check.take() else {
            return;
        };
        let st = monitor.status();
        close_in_background(monitor);
        self.check_result = Some(std::array::from_fn(|ch| match &st.failed[ch] {
            Some(e) if st.max[ch] < CHECK_WORKS => Check::Failed(e.clone()),
            _ if st.max[ch] >= CHECK_WORKS => Check::Works,
            _ => Check::Silent,
        }));
    }

    fn begin(&mut self) {
        // The check holds the devices; a recording opens them itself.
        if let Some((monitor, _)) = self.check.take() {
            close_in_background(monitor);
        }
        // The call gets the computer to itself: a transcription in progress
        // stops and resumes from its checkpoint after the recording.
        if let Some(job) = self.job.take() {
            self.queue.push_front(job.session().to_path_buf());
            job.cancel();
        }
        self.check_result = None;
        let opts = RecorderOptions {
            root: self.settings.recordings_dir(),
            // One file per call.
            part_seconds: 0,
            bitrate_kbps: self.settings.bitrate_kbps,
            mic_device: self.settings.mic_device.clone(),
            system: SystemOption::Auto,
            aec: self.settings.aec,
        };
        match Recorder::start(opts) {
            Ok(r) => {
                tracing::info!(dir = %r.status().dir.display(), "recording started from the window");
                self.status = Some(r.status());
                self.recorder = Some(r);
                self.message = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start recording");
                self.message = Some(Message {
                    text: format!("Could not start recording: {e:#}"),
                    problem: true,
                });
            }
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if let (Some(r), Some(st)) = (&self.recorder, &mut self.status) {
            r.set_paused(paused);
            // Shown at once; the recorder confirms on its next status update.
            st.paused = paused;
        }
    }

    /// Stops capture at once and closes the session in the background; the
    /// result arrives in [`Self::collect_saved`].
    fn stop(&mut self) {
        let Some(r) = self.recorder.take() else {
            return;
        };
        self.drain_notices(&r);
        let st = r.status();
        self.status = None;
        let thread = std::thread::spawn(move || r.stop());
        self.saving = Some(Saving {
            dir: st.dir,
            elapsed_ms: st.elapsed_ms,
            thread,
        });
    }

    /// Picks up a session closed in the background; `wait` blocks until it
    /// is (on quit).
    fn collect_saved(&mut self, wait: bool) {
        if !wait && !self.saving.as_ref().is_some_and(|s| s.thread.is_finished()) {
            return;
        }
        let Some(saving) = self.saving.take() else {
            return;
        };
        let dir = saving.dir;
        let result = saving
            .thread
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the recording thread panicked")));
        match result {
            Ok(m) => {
                let seconds: u64 = m.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
                tracing::info!(dir = %dir.display(), seconds, "recording stopped from the window");
                if self.settings.transcribe_automatically
                    && self.model_status == ModelStatus::Ready
                    && seconds > 0
                {
                    self.enqueue(dir.clone());
                }
                self.last = Some(LastRecording {
                    dir,
                    started: m.started_at_utc,
                    seconds,
                });
                self.check_disk();
                // Information about the finished session is now stale.
                if self.message.as_ref().is_some_and(|m| !m.problem) {
                    self.message = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "recording ended with an error");
                self.message = Some(Message {
                    text: format!("Recording ended with an error: {e:#}"),
                    problem: true,
                });
            }
        }
    }

    fn drain_notices(&mut self, r: &Recorder) {
        while let Some(n) = r.try_notice() {
            tracing::info!(notice = ?n, "notice");
            match n {
                // The permissions card shows this, with the button that fixes it.
                Notice::PermissionDenied(_) => self.permissions = current_permissions(),
                // Only failures that lose audio reach the window; hints and
                // status changes (echo risk, silence, device switch, sleep,
                // recovery) stay in the log.
                Notice::DeviceFailed { .. } | Notice::WriteFailed(_) | Notice::SystemMissing => {
                    self.message = Some(Message {
                        text: n.message(),
                        problem: true,
                    });
                }
                Notice::SystemBack
                    if self
                        .message
                        .as_ref()
                        .is_some_and(|m| m.text == Notice::SystemMissing.message()) =>
                {
                    self.message = None;
                }
                _ => {}
            }
        }
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.save() {
            tracing::error!(error = %e, "cannot save settings");
        }
    }

    // ── Transcription ──────────────────────────────────────────────────

    /// Model and transcript states as they are on disk.
    fn check_disk(&mut self) {
        let dir = self.settings.models_dir();
        self.model_status =
            transcription::model_status(&dir, self.settings.transcription_quality());
        self.model_size = transcription::model_disk_size(&dir);
        self.last_transcript = self.last.as_ref().map_or(TranscriptState::None, |l| {
            transcription::transcript_state(&l.dir)
        });
        self.last_title = self.last.as_ref().and_then(|l| sessions::title(&l.dir));
        // The list reads every manifest, so only while it is on screen.
        if self.view == View::Recordings {
            let (list, total) = sessions::page(&self.settings.recordings_dir(), 0, self.loaded);
            self.sessions = list;
            self.sessions_total = total;
            // Opening the list shows a transcript: the one chosen before, or
            // the newest.
            if self.reader.dir().is_none()
                && let Some(first) = self.sessions.first()
            {
                self.reader.select(Some(first.dir.clone()));
            }
        }
        self.reader.refresh();
        self.shown = self.reader.dir().and_then(sessions::info);
        self.disk_checked = Instant::now();
    }

    fn enqueue(&mut self, dir: PathBuf) {
        let busy = self.job.as_ref().is_some_and(|j| j.session() == dir);
        if !busy && !self.queue.contains(&dir) {
            self.queue.push_back(dir);
        }
    }

    /// Starts the next queued transcription when nothing is being recorded.
    fn pump_transcription(&mut self) {
        if let Some(job) = &mut self.job
            && let Some(outcome) = job.poll()
        {
            self.job = None;
            if let JobOutcome::Failed(e) = outcome {
                self.message = Some(Message {
                    text: format!("Transcription failed: {e}"),
                    problem: true,
                });
            }
            self.check_disk();
        }
        let idle = self.recorder.is_none() && self.saving.is_none() && self.job.is_none();
        if !idle || self.model_status != ModelStatus::Ready {
            return;
        }
        let Some(dir) = self.queue.pop_front() else {
            return;
        };
        match TranscriptionJob::start(
            &dir,
            &self.settings.models_dir(),
            self.settings.transcription_threads(),
            self.settings.transcription_quality(),
            self.settings.tell_voices_apart,
        ) {
            Ok(job) => self.job = Some(job),
            Err(e) => {
                tracing::error!(error = %e, "cannot start the transcriber");
                self.message = Some(Message {
                    text: format!("Could not start transcription: {e}"),
                    problem: true,
                });
            }
        }
        self.check_disk();
    }

    fn start_download(&mut self) {
        if self.download.is_none() {
            self.download = Some(ModelDownload::start(
                self.settings.models_dir(),
                self.settings.transcription_quality(),
            ));
        }
    }

    fn poll_download(&mut self) {
        let Some(finished) = self.download.as_ref().and_then(|d| d.state().finished) else {
            return;
        };
        self.download = None;
        if let Err(Some(e)) = finished {
            self.message = Some(Message {
                text: format!("Could not download the speech model: {e}"),
                problem: true,
            });
        }
        self.check_disk();
    }

    /// Notices, a session that ended on its own, timers, close requests.
    fn poll(&mut self, ctx: &egui::Context) {
        self.collect_saved(false);
        self.poll_download();
        self.pump_transcription();
        if self.disk_checked.elapsed() >= DISK_CHECK {
            self.check_disk();
        }
        let missing = self.permissions.iter().any(|(_, a)| *a != Access::Granted);
        if missing && self.permissions_checked.elapsed() >= Duration::from_secs(1) {
            // Picks up a switch flipped in System Settings without a restart.
            self.permissions = current_permissions();
            self.permissions_checked = Instant::now();
        }
        if self.permissions.iter().any(|(_, a)| *a != Access::Granted) {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(r) = self.recorder.take() {
            self.drain_notices(&r);
            let st = r.status();
            let running = st.running;
            self.status = Some(st);
            self.recorder = Some(r);
            if !running {
                // Stopped by itself (write error): collect the result.
                self.stop();
            }
        }
        if let Some((pause_at, resume_at)) = self.smoke_pause {
            let t = self.started.elapsed().as_secs_f32();
            let paused = self.status.as_ref().is_some_and(|s| s.paused);
            if t >= resume_at {
                self.smoke_pause = None;
                self.set_paused(false);
            } else if t >= pause_at && !paused {
                self.set_paused(true);
            }
        }
        if self
            .check
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() >= Duration::from_secs(CHECK_SECONDS))
        {
            self.finish_check();
        }
        if self.smoke_until.is_some_and(|t| Instant::now() >= t) {
            self.smoke_until = None;
            self.stop();
            if self.screenshot.is_none() {
                self.close(ctx);
            }
        }
        if let Some((_, at)) = &self.screenshot
            && self.started.elapsed().as_secs_f32() >= *at
        {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot
            && let Some((path, _)) = self.screenshot.take()
        {
            save_png(&path, &image);
            if self.smoke_until.is_none() {
                self.close(ctx);
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.recorder.is_some() && !self.closing
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.stop();
        self.closing = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    // ── Main screen ────────────────────────────────────────────────────

    /// The navigation rail: the logo, Record and Recordings at the top,
    /// Settings at the bottom (as Material's NavigationRail).
    fn rail(&mut self, ui: &mut egui::Ui) {
        let recording = self.recorder.is_some();
        let mut go = None;
        ui.vertical_centered(|ui| {
            ui.add_space(ui::S1);
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(34.0, 34.0)))
                .on_hover_text(APP_NAME);
            ui.add_space(ui::S4 + ui::S2);
            for (view, icon, label) in [
                (View::Main, ui::Icon::Mic, "Record"),
                (View::Recordings, ui::Icon::List, "Recordings"),
            ] {
                let badge = view == View::Main && recording;
                if ui::rail_item(ui, icon, label, self.view == view, badge).clicked() {
                    go = Some(view);
                }
                ui.add_space(ui::S1);
            }
            ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                ui.add_space(ui::S2);
                if ui::rail_item(
                    ui,
                    ui::Icon::Settings,
                    "Settings",
                    self.view == View::Settings,
                    false,
                )
                .clicked()
                {
                    go = Some(View::Settings);
                }
            });
        });
        if let Some(v) = go {
            self.go(v);
        }
    }

    fn go(&mut self, view: View) {
        self.view = view;
        if view == View::Recordings {
            self.check_disk();
        }
    }

    /// The main card: status, the timer and the one action.
    fn transport(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        // While the last session closes, keep its duration on screen.
        let saved_ms = self.saving.as_ref().map(|s| s.elapsed_ms);
        ui::card(ui, |ui| {
            ui::card_header(ui, "Call recording", |ui| match &st {
                None if saved_ms.is_some() => {
                    ui::pill(ui, "Saving…", ui::Tone::Grey, false);
                }
                None => {
                    ui::pill(ui, "Ready", ui::Tone::Blue, false);
                }
                Some(s) => {
                    if s.paused {
                        ui::pill(ui, "Paused", ui::Tone::Amber, false);
                    } else {
                        ui::pill(ui, "Recording", ui::Tone::Red, true);
                    }
                }
            });
            ui.add_space(ui::S2);
            ui.label(ui::caption("Duration"));
            let elapsed = st.as_ref().map(|s| s.elapsed_ms).or(saved_ms).unwrap_or(0) / 1000;
            ui.label(
                RichText::new(hms(elapsed))
                    .font(FontId::new(42.0, ui::bold()))
                    .color(match &st {
                        Some(s) if !s.paused => ui::INK,
                        Some(_) => ui::INK_SECONDARY,
                        None if saved_ms.is_some() => ui::INK_SECONDARY,
                        None => ui::INK_MUTED,
                    }),
            );
            ui.add_space(ui::S3);
            let Some(s) = &st else {
                if ui::primary(ui, "Start recording", Some(ui::Icon::Mic), None, true).clicked() {
                    self.start();
                }
                return;
            };
            let paused = s.paused;
            let (mut toggle, mut stop) = (false, false);
            ui.columns(2, |cols| {
                toggle = if paused {
                    ui::primary(&mut cols[0], "Resume", Some(ui::Icon::Play), None, true)
                } else {
                    ui::outline(
                        &mut cols[0],
                        "Pause",
                        Some((ui::Icon::Pause, ui::INK)),
                        true,
                    )
                }
                .clicked();
                stop = ui::outline(&mut cols[1], "Stop", Some((ui::Icon::Stop, ui::RED)), true)
                    .clicked();
            });
            if toggle {
                self.set_paused(!paused);
            }
            if stop {
                self.stop();
            }
        });
    }

    fn channels(&mut self, ui: &mut egui::Ui) {
        // Live levels come from the recording or, before one, from the check.
        let levels = match (&self.status, &self.check) {
            (Some(st), _) => Some(st.levels),
            (None, Some((m, _))) => Some(m.status().levels),
            _ => None,
        };
        let left = self
            .check
            .as_ref()
            .map(|(_, t)| CHECK_SECONDS.saturating_sub(t.elapsed().as_secs()));
        let recording = self.recorder.is_some();
        let (mut start, mut stop) = (false, false);
        ui::card(ui, |ui| {
            ui::card_header(ui, "Channels", |ui| {
                if recording {
                    return;
                }
                if left.is_some() {
                    stop = ui::outline_small(ui, "Stop test", Some((ui::Icon::Stop, ui::RED)))
                        .clicked();
                } else {
                    start = ui::outline_small(ui, "Test", Some((ui::Icon::Wave, ui::BLUE)))
                        .on_hover_text("Check that both channels hear sound")
                        .clicked();
                }
            });
            ui.add_space(ui::S1);
            let rows = [
                (ui::Icon::Mic, "You", "microphone", levels.map(|l| l[0])),
                (
                    ui::Icon::Speaker,
                    "Others",
                    "system audio",
                    levels.map(|l| l[1]),
                ),
            ];
            for (i, (icon, name, source, peak)) in rows.into_iter().enumerate() {
                if i > 0 {
                    ui.add_space(ui::S2);
                }
                let verdict = self.check_result.as_ref().map(|r| r[i].clone());
                ui.horizontal(|ui| {
                    ui::icon_tile(ui, icon, ui::Tone::Blue);
                    let level = ui::smooth_level(ui, name, peak);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = ui::S1 + 2.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).font(FontId::new(14.0, ui::medium())));
                            ui.label(ui::muted(source));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                match (peak, verdict) {
                                    (None, Some(Check::Works)) => {
                                        ui::pill(ui, "Works", ui::Tone::Green, false);
                                    }
                                    (None, Some(Check::Silent)) => {
                                        ui::pill(ui, "No sound", ui::Tone::Amber, false);
                                    }
                                    (None, Some(Check::Failed(e))) => {
                                        ui::pill(ui, "Unavailable", ui::Tone::Red, false)
                                            .on_hover_text(e);
                                    }
                                    (None, None) => {
                                        ui.label(ui::muted("—"));
                                    }
                                    (Some(_), _) if level < 0.005 => {
                                        ui.label(ui::muted("silent"));
                                    }
                                    (Some(_), _) => {
                                        // A real minus sign, not a hyphen.
                                        let db = format!("{:.0} dB", level * 60.0 - 60.0)
                                            .replace('-', "\u{2212}");
                                        ui.label(ui::muted(db));
                                    }
                                }
                            });
                        });
                        ui::meter(ui, level);
                    });
                });
            }
            if let Some(left) = left {
                ui.add_space(ui::S2);
                ui.label(ui::muted(format!(
                    "Say something and play any sound, such as a video, so both channels \
                     hear it. {left} s left."
                )));
            } else if let Some(result) = &self.check_result {
                let hint = match result {
                    [Check::Works, Check::Works] => None,
                    [Check::Works, Check::Silent] => Some(
                        "No sound from others. Play something during the test and check \
                         the output device.",
                    ),
                    [Check::Silent, Check::Works] => {
                        Some("No sound from your microphone. Check that it is not muted.")
                    }
                    [Check::Silent, Check::Silent] => Some(
                        "No sound on either channel. Speak and play something during the test.",
                    ),
                    _ => Some("A channel could not be opened. Check the permissions above."),
                };
                if let Some(hint) = hint {
                    ui.add_space(ui::S2);
                    ui.label(ui::muted(hint));
                }
            }
        });
        if start {
            self.start_check();
        }
        if stop {
            self.finish_check();
        }
    }

    fn message(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &self.message else {
            return;
        };
        let (icon, tone) = if m.problem {
            (ui::Icon::Alert, ui::Tone::Amber)
        } else {
            (ui::Icon::Info, ui::Tone::Blue)
        };
        let mut dismiss = false;
        ui::card(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui::icon_tile(ui, icon, tone);
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if ui::close_button(ui, "Dismiss").clicked() {
                        dismiss = true;
                    }
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.add_space(ui::S1);
                        ui.label(RichText::new(&m.text).color(ui::INK));
                    });
                });
            });
        });
        if dismiss {
            self.message = None;
        }
    }

    fn last_recording(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        ui::card(ui, |ui| {
            let Some(last) = &self.last else {
                ui::card_header(ui, "Recordings", |_| {});
                ui.label(ui::muted("Your last recording will appear here."));
                ui.add_space(ui::S1);
                if ui::link(ui, "All recordings").clicked() {
                    action = Some(TranscriptAction::List);
                }
                return;
            };
            ui::card_header(ui, "Last recording", |ui| {
                ui::pill(ui, "Saved", ui::Tone::Green, false);
            });
            match &self.last_title {
                // A name can be long: on its own line, cut to the card.
                Some(t) => {
                    ui.add(
                        egui::Label::new(RichText::new(t).color(ui::INK))
                            .truncate()
                            .selectable(false),
                    );
                    ui.label(ui::muted(format!(
                        "{} · {}",
                        when(last.started),
                        duration_words(last.seconds)
                    )));
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(when(last.started)).color(ui::INK));
                        ui.label(
                            RichText::new(format!("· {}", duration_words(last.seconds)))
                                .color(ui::INK_MUTED),
                        );
                    });
                }
            }
            ui.add_space(ui::S2);
            ui.horizontal(|ui| {
                if ui::outline(ui, REVEAL, Some((ui::Icon::Folder, ui::BLUE)), false).clicked() {
                    paths::open_in_file_manager(&last.dir);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::link(ui, "All recordings").clicked() {
                        action = Some(TranscriptAction::List);
                    }
                });
            });
            ui.add_space(ui::S3);
            ui::divider(ui);
            ui.add_space(ui::S3);
            if let Some(a) = self.transcript_row(ui, &last.dir) {
                action = Some(a);
            }
        });
        if let Some(a) = action {
            self.apply(a);
        }
    }

    fn apply(&mut self, action: TranscriptAction) {
        match action {
            TranscriptAction::Start(dir) => {
                self.message = None;
                self.enqueue(dir);
                self.pump_transcription();
            }
            TranscriptAction::Stop => {
                if let Some(job) = self.job.take() {
                    job.cancel();
                }
                self.check_disk();
            }
            TranscriptAction::Unqueue(dir) => self.queue.retain(|d| *d != dir),
            TranscriptAction::Open(path) => paths::open_file(&path),
            TranscriptAction::View(dir) => {
                if self.renaming.as_ref().is_some_and(|r| r.dir != dir) {
                    self.renaming = None;
                }
                self.reader.select(Some(dir));
                self.go(View::Recordings);
            }
            TranscriptAction::Reveal(dir) => paths::open_in_file_manager(&dir),
            TranscriptAction::SetUp => self.view = View::Settings,
            TranscriptAction::More => {
                self.loaded += LOAD_STEP;
                self.check_disk();
            }
            TranscriptAction::List => self.go(View::Recordings),
        }
    }

    /// Transcript buttons for one recording, laid out right to left:
    /// Transcribe / progress and Stop / Queued / Open / Try again / Set up.
    fn transcript_controls(
        &self,
        ui: &mut egui::Ui,
        dir: &Path,
        state: &TranscriptState,
    ) -> Option<TranscriptAction> {
        let mut action = None;
        let job = self.job.as_ref().filter(|j| j.session() == dir);
        if let Some(job) = job {
            if ui::outline_small(ui, "Stop", Some((ui::Icon::Stop, ui::RED))).clicked() {
                action = Some(TranscriptAction::Stop);
            }
            let st = job.state();
            let pct = if st.started && st.total_s > 0.0 {
                format!("{:.0}%", st.done_s / st.total_s * 100.0)
            } else {
                "Starting…".into()
            };
            use recorder_transcribe::heat::Heat;
            let (text, hint) = match st.heat {
                Heat::Normal => (pct, None),
                Heat::Warm => (
                    format!("{pct} · resting"),
                    Some("The Mac is warm: transcription takes breaks to cool it down."),
                ),
                Heat::Hot => (
                    format!("{pct} · cooling down"),
                    Some("The Mac is hot: transcription waits until it cools down."),
                ),
            };
            let pill = ui::pill(ui, &text, ui::Tone::Blue, true);
            if let Some(h) = hint {
                pill.on_hover_text(h);
            }
        } else if self.queue.iter().any(|d| d == dir) {
            if ui::outline_small(ui, "Cancel", None).clicked() {
                action = Some(TranscriptAction::Unqueue(dir.to_path_buf()));
            }
            ui::pill(ui, "Queued", ui::Tone::Grey, false);
        } else if let TranscriptState::Done(_) = state {
            if ui::outline_small(ui, "View", Some((ui::Icon::Arrow, ui::BLUE))).clicked() {
                action = Some(TranscriptAction::View(dir.to_path_buf()));
            }
        } else if self.model_status != ModelStatus::Ready {
            if ui::outline_small(ui, "Set up", None).clicked() {
                action = Some(TranscriptAction::SetUp);
            }
        } else if let TranscriptState::Failed(e) = state {
            if ui::outline_small(ui, "Try again", None)
                .on_hover_text(e)
                .clicked()
            {
                action = Some(TranscriptAction::Start(dir.to_path_buf()));
            }
        } else {
            // `Running` without a job here: another process or an
            // interrupted run; starting resumes it.
            if ui::outline_small(ui, "Transcribe", Some((ui::Icon::Wave, ui::BLUE))).clicked() {
                action = Some(TranscriptAction::Start(dir.to_path_buf()));
            }
        }
        action
    }

    /// Progress of the running transcription of `dir`, if any.
    fn job_progress(&self, dir: &Path) -> Option<f32> {
        let job = self.job.as_ref().filter(|j| j.session() == dir)?;
        let st = job.state();
        Some((st.done_s / st.total_s.max(1e-9)) as f32)
    }

    /// Transcript of the last recording: start, progress, open.
    fn transcript_row(&self, ui: &mut egui::Ui, dir: &Path) -> Option<TranscriptAction> {
        let mut action = None;
        let queued = self.queue.iter().any(|d| d == dir);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Transcript").font(FontId::new(14.0, ui::medium())));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                action = self.transcript_controls(ui, dir, &self.last_transcript);
            });
        });
        let hint = if let Some(p) = self.job_progress(dir) {
            ui.add_space(ui::S1);
            ui::progress(ui, p);
            "Runs in the background at low priority and pauses while you record.".to_string()
        } else if queued && self.recorder.is_some() {
            "Starts when the recording ends.".into()
        } else if queued {
            "Starts shortly.".into()
        } else if self.model_status == ModelStatus::Outdated {
            "Update the speech model in Settings to transcribe.".into()
        } else if self.model_status != ModelStatus::Ready {
            "Download the speech model once to transcribe calls on this computer.".into()
        } else {
            match &self.last_transcript {
                TranscriptState::Done(_) => {
                    "Who said what, with times. Also saved as text, subtitles and JSON.".into()
                }
                TranscriptState::Failed(e) => format!("Failed: {e}"),
                _ => "Turns speech into text on this computer; audio never leaves it.".into(),
            }
        };
        ui.add_space(ui::S1);
        ui.label(ui::muted(hint));
        action
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Record", |_| {});
        self.permissions_card(ui);
        self.transport(ui);
        ui.add_space(ui::S3);
        self.message(ui);
        if self.message.is_some() {
            ui.add_space(ui::S3);
        }
        self.channels(ui);
        ui.add_space(ui::S3);
        self.last_recording(ui);
    }

    // ── Recordings screen ──────────────────────────────────────────────

    fn recordings_screen(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        section_title(ui, "Recordings", |ui| {
            if ui::icon_button(ui, ui::Icon::Folder, "Open the recordings folder").clicked() {
                action = Some(TranscriptAction::Reveal(self.settings.recordings_dir()));
            }
        });
        ui::card(ui, |ui| {
            let count = self.sessions.len();
            let total = self.sessions_total;
            ui::card_header(ui, "All recordings", |ui| {
                if total > 0 {
                    ui.label(ui::muted(total.to_string()));
                }
            });
            if count == 0 {
                ui.label(ui::muted(
                    "No recordings yet. They appear here when you press Stop.",
                ));
                return;
            }
            if self.model_status != ModelStatus::Ready {
                ui.label(ui::muted(if self.model_status == ModelStatus::Outdated {
                    "To transcribe, update the speech model in Settings."
                } else {
                    "To transcribe, download the speech model once in Settings."
                }));
                ui.add_space(ui::S2);
            }
            // One list; more is read when its end shows.
            let mut last_rect = None;
            ui.add_space(ui::S1);
            for s in &self.sessions {
                let (a, rect) = self.session_row(ui, s);
                if a.is_some() {
                    action = a;
                }
                last_rect = Some(rect);
            }
            if count < total {
                ui.add_space(ui::S3);
                ui.label(ui::muted("Loading older recordings…"));
                if last_rect.is_some_and(|r| ui.is_rect_visible(r)) && action.is_none() {
                    action = Some(TranscriptAction::More);
                }
            }
        });
        if let Some(a) = action {
            self.apply(a);
        }
    }

    /// One recording: when, how long, transcript state and its button.
    fn session_row(
        &self,
        ui: &mut egui::Ui,
        s: &SessionInfo,
    ) -> (Option<TranscriptAction>, egui::Rect) {
        let mut action = None;
        let busy = self.job.as_ref().is_some_and(|j| j.session() == s.dir);
        let queued = self.queue.contains(&s.dir);
        let state = match (&s.transcript, busy, queued) {
            _ if s.recording => "recording now",
            _ if s.seconds == 0 => "no audio",
            _ if s.audio_missing => "audio file missing",
            (_, true, _) => "transcribing",
            (_, _, true) => "waiting",
            (TranscriptState::Done(_), ..) => "transcript ready",
            // Short: the row also holds a button and the folder icon.
            (TranscriptState::Failed(_), ..) => "failed",
            (TranscriptState::Running, ..) => "interrupted",
            (TranscriptState::None, ..) => "no transcript",
        };
        // The whole row opens the transcript; the buttons on it are laid
        // over it, so they still take their own clicks.
        let selected = self.reader.dir() == Some(s.dir.as_path());
        let progress = self.job_progress(&s.dir);
        let height = if progress.is_some() { 64.0 } else { 52.0 };
        let (rect, row) =
            ui.allocate_exact_size(vec2(ui.available_width(), height), egui::Sense::click());
        let row = row.on_hover_cursor(egui::CursorIcon::PointingHand);
        let fill = if selected {
            ui::BLUE.gamma_multiply(0.08)
        } else if row.hovered() {
            ui::HOVER
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect.expand2(vec2(ui::S2, 0.0)), 10, fill);
        if row.clicked() {
            action = Some(TranscriptAction::View(s.dir.clone()));
        }
        let mut inner = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::top_down(Align::Min)),
        );
        // Room for what shows on the right; long names are cut before it.
        let near = inner.rect_contains_pointer(rect) || selected;
        let done = matches!(s.transcript, TranscriptState::Done(_));
        let controls = s.recording || (s.transcribable() && (!done || busy || queued));
        let reserve = if near && !busy && !queued { 44.0 } else { 0.0 }
            + if controls { 120.0 } else { 0.0 };
        inner.horizontal(|ui| {
            ui.set_min_height(52.0);
            ui.vertical(|ui| {
                ui.set_max_width((rect.width() - reserve - ui::S2).max(100.0));
                ui.add_space(ui::S2);
                ui.spacing_mut().item_spacing.y = 2.0;
                let (name, sub) = match &s.title {
                    Some(t) => (
                        t.clone(),
                        format!(
                            "{} · {} · {state}",
                            when(s.started),
                            duration_words(s.seconds)
                        ),
                    ),
                    None => (
                        when(s.started),
                        format!("{} · {state}", duration_words(s.seconds)),
                    ),
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(name)
                            .font(FontId::new(14.0, ui::medium()))
                            .color(if selected { ui::BLUE } else { ui::INK }),
                    )
                    .truncate()
                    .selectable(false),
                );
                ui.add(egui::Label::new(ui::muted(sub)).truncate().selectable(false));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // The folder button only on the row under the pointer or the
                // shown one, so the list stays quiet.
                if near
                    && !busy
                    && !queued
                    && ui::icon_button(ui, ui::Icon::Folder, REVEAL).clicked()
                {
                    action = Some(TranscriptAction::Reveal(s.dir.clone()));
                }
                if s.recording {
                    ui::pill(ui, "Recording", ui::Tone::Red, true);
                } else if s.transcribable() && (!done || busy || queued) {
                    action = self
                        .transcript_controls(ui, &s.dir, &s.transcript)
                        .or(action.take());
                }
            });
        });
        if let Some(p) = progress {
            inner.add_space(ui::S1);
            ui::progress(&mut inner, p);
        }
        (action, rect)
    }

    /// The section (Record, Recordings or Settings) in a scroll area,
    /// centred when there is room to spare.
    fn section(&mut self, ui: &mut egui::Ui) {
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("section", self.view as u8))
            .auto_shrink([false, false]);
        if let Some(y) = self.scroll.take() {
            area = area.vertical_scroll_offset(y);
        }
        area.show(ui, |ui| {
            // A column no wider than SECTION_MAX, in the middle.
            let full = ui.available_rect_before_wrap();
            let width = full.width().min(SECTION_MAX + 16.0);
            let column = egui::Rect::from_min_size(
                egui::pos2(full.center().x - width / 2.0, full.top()),
                vec2(width, full.height()),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
                // Room for the card shadows, and on the right for the
                // floating scroll bar.
                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 10,
                        top: 4,
                        bottom: 12,
                    })
                    .show(ui, |ui| match self.view {
                        View::Main => self.main_screen(ui),
                        View::Settings => self.settings_screen(ui),
                        View::Recordings => self.recordings_screen(ui),
                    });
            });
        });
    }

    // ── Transcript pane ────────────────────────────────────────────────

    /// The right side: the transcript of the selected recording, or what
    /// stands in for it (transcribing, not yet, failed, nothing selected).
    fn start_renaming(&mut self, info: &SessionInfo) {
        self.renaming = Some(Renaming {
            dir: info.dir.clone(),
            text: info.title.clone().unwrap_or_default(),
            focus: true,
        });
    }

    /// The name field in place of the title: Enter or Save keeps it, Esc or
    /// Cancel drops the change; an empty name shows the date again.
    fn rename_row(&mut self, ui: &mut egui::Ui, info: &SessionInfo) {
        let Some(r) = self.renaming.as_mut() else {
            return;
        };
        let (mut save, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                save = ui::primary_small(ui, "Save").clicked();
                cancel = ui::outline_small(ui, "Cancel", None).clicked();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut r.text)
                            .hint_text(when(info.started))
                            .char_limit(sessions::TITLE_MAX_CHARS)
                            .font(FontId::new(17.0, ui::medium()))
                            .desired_width(ui.available_width() - ui::S2)
                            .margin(vec2(10.0, 7.0)),
                    );
                    if r.focus {
                        field.request_focus();
                        r.focus = false;
                    }
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        save = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                });
            });
        });
        ui.add_space(2.0);
        ui.label(ui::muted(format!(
            "{} · {} · leave empty to show the date",
            when(info.started),
            duration_words(info.seconds)
        )));
        if cancel {
            self.renaming = None;
        } else if save && let Some(r) = self.renaming.take() {
            self.save_title(&r.dir, &r.text);
        }
    }

    /// Names a recording and shows the name everywhere at once.
    fn save_title(&mut self, dir: &Path, name: &str) {
        if let Err(e) = sessions::set_title(dir, name) {
            self.message = Some(Message {
                text: format!("Cannot rename the recording: {e}"),
                problem: true,
            });
            return;
        }
        let title = sessions::title(dir);
        for s in self
            .sessions
            .iter_mut()
            .chain(self.shown.as_mut())
            .filter(|s| s.dir == dir)
        {
            s.title = title.clone();
        }
        if self.last.as_ref().is_some_and(|l| l.dir == dir) {
            self.last_title = title;
        }
    }

    fn transcript_pane(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        ui::card_frame().show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let Some(info) = self.shown.clone() else {
                if self.last.is_some() {
                    empty_state(
                        ui,
                        "Choose a recording",
                        "Pick a recording on the left to read its transcript here.",
                    );
                } else {
                    empty_state(
                        ui,
                        "No recording yet",
                        "Record a call; its transcript appears here, who said what and when.",
                    );
                }
                return;
            };
            // Title row: its name (or when), how long, and what can be done
            // with it. The buttons first, so a long name is cut before them.
            if self.renaming.as_ref().is_some_and(|r| r.dir == info.dir) {
                self.rename_row(ui, &info);
            } else {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui::icon_button(ui, ui::Icon::Folder, REVEAL).clicked() {
                            action = Some(TranscriptAction::Reveal(info.dir.clone()));
                        }
                        if ui::icon_button(ui, ui::Icon::Pencil, "Rename").clicked() {
                            self.start_renaming(&info);
                        }
                        let busy = self.job.as_ref().is_some_and(|j| j.session() == info.dir)
                            || self.queue.contains(&info.dir);
                        match (&info.transcript, busy) {
                            (TranscriptState::Done(txt), false) => {
                                if ui::outline_small(ui, "Open file", None)
                                    .on_hover_text("transcript.txt in your text editor")
                                    .clicked()
                                {
                                    action = Some(TranscriptAction::Open(txt.clone()));
                                }
                                if ui::outline_small(ui, "Copy all", None).clicked()
                                    && let Some(Ok(t)) = &self.reader.transcript
                                {
                                    ui.ctx().copy_text(reader::plain_text(t));
                                    self.message = Some(Message {
                                        text: "The transcript is copied.".into(),
                                        problem: false,
                                    });
                                }
                            }
                            _ if info.transcribable() => {
                                if let Some(a) =
                                    self.transcript_controls(ui, &info.dir, &info.transcript)
                                {
                                    action = Some(a);
                                }
                            }
                            _ => {}
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let (name, sub) = match &info.title {
                                    Some(t) => (
                                        t.clone(),
                                        format!(
                                            "{} · {}",
                                            when(info.started),
                                            duration_words(info.seconds)
                                        ),
                                    ),
                                    None => (when(info.started), duration_words(info.seconds)),
                                };
                                let title = ui
                                    .add(
                                        egui::Label::new(
                                            RichText::new(name)
                                                .font(FontId::new(19.0, ui::bold()))
                                                .color(ui::INK),
                                        )
                                        .truncate()
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::Text);
                                if title.double_clicked() {
                                    self.start_renaming(&info);
                                }
                                ui.label(ui::muted(sub));
                            });
                        });
                    });
                });
            }
            ui.add_space(ui::S3);
            let job = self.job_progress(&info.dir);
            let queued = self.queue.contains(&info.dir);
            match (&self.reader.transcript, job) {
                (_, Some(p)) => {
                    ui::progress(ui, p);
                    ui.add_space(ui::S2);
                    ui.label(ui::muted(
                        "Transcribing in the background at low priority. The text appears here \
                         when it is ready.",
                    ));
                }
                (Some(Ok(t)), None) if matches!(info.transcript, TranscriptState::Done(_)) => {
                    reader::people(ui, t);
                    ui.add_space(ui::S2);
                    reader::search(ui, &mut self.reader.query, t);
                    ui.add_space(ui::S2);
                    ui::divider(ui);
                    reader::body(ui, t, &self.reader.query);
                }
                (Some(Err(e)), None) => {
                    empty_state(ui, "The transcript cannot be read", e);
                }
                _ if info.recording => {
                    empty_state(ui, "Recording now", "The transcript follows when it ends.");
                }
                _ if info.seconds == 0 => {
                    empty_state(ui, "No audio", "Nothing was recorded in this session.");
                }
                _ if info.audio_missing => empty_state(
                    ui,
                    "The audio is missing",
                    "The audio files of this recording were deleted or moved.",
                ),
                _ if queued => empty_state(
                    ui,
                    "Waiting to be transcribed",
                    "It starts when the current transcription or recording ends.",
                ),
                _ => match &info.transcript {
                    TranscriptState::Failed(e) => empty_state(
                        ui,
                        "Transcription failed",
                        &format!("{e}. Try again above."),
                    ),
                    _ if self.model_status == ModelStatus::Outdated => empty_state(
                        ui,
                        "No transcript yet",
                        "Update the speech models in Settings, then press Transcribe.",
                    ),
                    _ if self.model_status != ModelStatus::Ready => empty_state(
                        ui,
                        "No transcript yet",
                        "Download the speech models once in Settings, then press Transcribe.",
                    ),
                    _ => empty_state(
                        ui,
                        "No transcript yet",
                        "Press Transcribe above. It runs on this computer; audio never leaves it.",
                    ),
                },
            }
        });
        if let Some(a) = action {
            self.apply(a);
        }
    }

    // ── Settings screen ────────────────────────────────────────────────

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Settings", |_| {});

        ui::card(ui, |ui| self.echo_settings(ui));
        ui.add_space(ui::S3);
        ui::card(ui, |ui| self.recordings_settings(ui));
        ui.add_space(ui::S3);
        ui::card(ui, |ui| self.transcription_settings(ui));
        ui.add_space(ui::S3);
        ui::card(ui, |ui| {
            ui::card_header(ui, "Diagnostics", |ui| {
                ui.label(ui::muted(format!("Version {}", env!("CARGO_PKG_VERSION"))));
            });
            ui.label(ui::muted(
                "Logs help find out what went wrong with a recording.",
            ));
            ui.add_space(ui::S1);
            if ui::link(ui, "Open logs").clicked() {
                paths::open_in_file_manager(&paths::log_dir());
            }
        });
    }

    fn echo_settings(&mut self, ui: &mut egui::Ui) {
        ui::card_header(ui, "Echo cancellation", |_| {});
        ui.add_space(ui::S1);
        let changed = ui::segmented(
            ui,
            &mut self.settings.aec,
            &[
                (AecMode::Auto, "Auto"),
                (AecMode::On, "Always"),
                (AecMode::Off, "Off"),
            ],
        );
        let hint = match self.settings.aec {
            AecMode::Auto => {
                "Turns on when sound plays through speakers and removes the other participants from your channel."
            }
            AecMode::On => "Always remove speaker sound from the microphone channel.",
            AecMode::Off => "The microphone is recorded as is. Best with headphones.",
        };
        ui.label(ui::muted(hint));
        if changed {
            self.save_settings();
            if self.recorder.is_some() {
                self.message = Some(Message {
                    text: "Echo cancellation changes from the next recording.".into(),
                    problem: false,
                });
            }
        }
    }

    fn recordings_settings(&mut self, ui: &mut egui::Ui) {
        ui::card_header(ui, "Recordings", |_| {});
        ui.add_space(ui::S1);
        ui.label(ui::caption("Folder"));
        ui.label(RichText::new(tilde(&self.settings.recordings_dir())).color(ui::INK));
        ui.add_space(ui::S2);
        ui.horizontal(|ui| {
            if ui::outline(ui, "Change…", None, false).clicked() {
                let picked = rfd::FileDialog::new()
                    .set_title("Recordings folder")
                    .set_directory(self.settings.recordings_dir())
                    .pick_folder();
                if let Some(dir) = picked {
                    tracing::info!(dir = %dir.display(), "recordings folder changed");
                    self.settings.recordings_dir = Some(dir);
                    self.save_settings();
                }
            }
            if ui::outline(ui, "Open", Some((ui::Icon::Folder, ui::BLUE)), false).clicked() {
                paths::open_in_file_manager(&self.settings.recordings_dir());
            }
        });
        ui.add_space(ui::S3);
        ui::divider(ui);
        ui.add_space(ui::S3);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new("Consent reminder").font(FontId::new(14.0, ui::medium())));
                ui.label(ui::muted("Before every recording"));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut remind = !self.settings.consent_reminder_dismissed;
                if ui::toggle(ui, &mut remind).changed() {
                    self.settings.consent_reminder_dismissed = !remind;
                    self.save_settings();
                }
            });
        });
    }

    fn transcription_settings(&mut self, ui: &mut egui::Ui) {
        let download = self.download.as_ref().map(|d| d.state());
        ui::card_header(ui, "Transcription", |ui| {
            match (&download, self.model_status) {
                (Some(_), _) => {
                    ui::pill(ui, "Downloading", ui::Tone::Blue, true);
                }
                (None, ModelStatus::Ready) => {
                    ui::pill(ui, "Ready", ui::Tone::Green, false);
                }
                (None, ModelStatus::Incomplete) => {
                    ui::pill(ui, "Incomplete", ui::Tone::Amber, false);
                }
                (None, ModelStatus::Outdated) => {
                    ui::pill(ui, "Update", ui::Tone::Amber, false);
                }
                (None, ModelStatus::Missing) => {
                    ui::pill(ui, "Not set up", ui::Tone::Grey, false);
                }
            }
        });
        ui.label(ui::muted(
            "Speech is turned into text on this computer; audio never leaves it. \
             Ukrainian, English and 23 other European languages.",
        ));
        ui.add_space(ui::S2);

        // Quality: only where there is a choice (macOS).
        let quality = self.settings.transcription_quality();
        if Quality::available().len() > 1 {
            ui.label(ui::caption("Quality"));
            ui.add_space(ui::S1);
            let mut q = quality;
            if ui::segmented(
                ui,
                &mut q,
                &[(Quality::Best, "Best"), (Quality::Fast, "Fast")],
            ) {
                self.settings.transcription_quality = q;
                self.save_settings();
                self.check_disk();
            }
            ui.label(ui::muted(match q {
                Quality::Best => {
                    "Whisper writes the text and Parakeet fills in what it skips: the most accurate. \
                     An hour of call takes about 10–20 minutes."
                }
                Quality::Fast => {
                    "Parakeet alone: an hour of call takes about 3 minutes, with more mistakes \
                     in words and terms."
                }
            }));
            ui.add_space(ui::S3);
        }

        // The model: download, progress, delete.
        let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
        let (mut start, mut cancel, mut delete) = (false, false, false);
        if let Some(state) = &download {
            let (done, total, phase) = state.progress.map_or(
                (0, transcription::download_size(quality), Phase::Downloading),
                |p| (p.done, p.total, p.phase),
            );
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(match phase {
                        Phase::Downloading => {
                            format!("Downloading… {:.0} of {:.0} MB", mb(done), mb(total))
                        }
                        Phase::Verifying => "Checking the files…".into(),
                    })
                    .color(ui::INK),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    cancel = ui::outline_small(ui, "Cancel", None).clicked();
                });
            });
            ui.add_space(ui::S1);
            ui::progress(ui, done as f32 / total.max(1) as f32);
            ui.add_space(ui::S1);
            ui.label(ui::muted(
                "You can keep using the app. An interrupted download resumes.",
            ));
        } else if self.model_status == ModelStatus::Ready {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Speech models · {:.0} MB", mb(self.model_size)))
                        .color(ui::INK),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    delete = ui::outline_small(ui, "Delete", None)
                        .on_hover_text("Frees the disk space; transcripts stay")
                        .clicked();
                });
            });
        } else {
            let missing = transcription::missing_bytes(&self.settings.models_dir(), quality);
            let gb = missing as f64 / 1_073_741_824.0;
            let outdated = self.model_status == ModelStatus::Outdated;
            let label = if outdated {
                format!("Update speech models ({gb:.1} GB)")
            } else {
                format!("Download speech models ({gb:.1} GB)")
            };
            start = ui::primary(ui, &label, None, None, true).clicked();
            ui.add_space(ui::S1);
            if outdated {
                ui.label(ui::muted(
                    "A more accurate speech model: about half the mistakes, and no lost \
                     sentences in English. It replaces the old one.",
                ));
            }
            ui.label(ui::muted(match quality {
                Quality::Best => {
                    "Once. About 3.5 GB on disk and up to 4 GB of memory while transcribing."
                }
                Quality::Fast => {
                    "Once. About 2.6 GB on disk and up to 3 GB of memory while transcribing."
                }
            }));
        }
        if start {
            self.start_download();
        }
        if cancel && let Some(d) = &self.download {
            d.cancel();
        }
        if delete {
            if let Some(job) = self.job.take() {
                job.cancel();
            }
            self.queue.clear();
            if let Err(e) = transcription::delete_model(&self.settings.models_dir()) {
                self.message = Some(Message {
                    text: format!("Could not delete the model: {e}"),
                    problem: true,
                });
            }
            self.check_disk();
        }

        ui.add_space(ui::S3);
        ui::divider(ui);
        ui.add_space(ui::S3);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("Transcribe automatically").font(FontId::new(14.0, ui::medium())),
                );
                ui.label(ui::muted("After each recording"));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui::toggle(ui, &mut self.settings.transcribe_automatically).changed() {
                    self.save_settings();
                }
            });
        });
        ui.add_space(ui::S3);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new("Tell voices apart").font(FontId::new(14.0, ui::medium())));
                ui.label(ui::muted(
                    "A name or Participant 1, 2… for each person on the call",
                ));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui::toggle(ui, &mut self.settings.tell_voices_apart).changed() {
                    self.save_settings();
                }
            });
        });
        ui.add_space(ui::S3);
        ui.label(ui::caption("Speed"));
        ui.add_space(ui::S1);
        let mut threads = if self.settings.transcription_threads >= 4 {
            4
        } else {
            2
        };
        if ui::segmented(ui, &mut threads, &[(2, "Gentle"), (4, "Fast")]) {
            self.settings.transcription_threads = threads;
            self.save_settings();
        }
        ui.label(ui::muted(match (quality, threads >= 4) {
            (Quality::Fast, true) => {
                "Uses more of the processor; about a quarter faster. An hour takes about 1.5 minutes."
            }
            (Quality::Fast, false) => {
                "Keeps the computer cool and quiet. An hour of call takes about 2 minutes."
            }
            (Quality::Best, true) => "Uses more of the processor; somewhat faster.",
            (Quality::Best, false) => {
                "Keeps the processor cooler; Whisper runs on the graphics processor either way."
            }
        }));

        ui.add_space(ui::S3);
        ui.label(ui::caption("Model folder"));
        ui.label(RichText::new(tilde(&self.settings.models_dir())).color(ui::INK));
        ui.add_space(ui::S1);
        ui.horizontal(|ui| {
            if ui::link(ui, "Use another folder").clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("Folder with the speech model")
                    .set_directory(self.settings.models_dir())
                    .pick_folder()
            {
                tracing::info!(dir = %dir.display(), "models folder changed");
                self.settings.models_dir = Some(dir);
                self.save_settings();
                self.check_disk();
            }
            if self.settings.models_dir.is_some() && ui::link(ui, "Default").clicked() {
                self.settings.models_dir = None;
                self.save_settings();
                self.check_disk();
            }
        });
        ui.add_space(ui::S2);
        ui.label(ui::muted(transcription::ATTRIBUTION));
    }

    // ── Dialogs ────────────────────────────────────────────────────────

    fn consent_modal(&mut self, ctx: &egui::Context) {
        if !self.show_consent {
            return;
        }
        let mut start = false;
        let response = egui::Modal::new(egui::Id::new("consent"))
            .frame(ui::modal_frame())
            .show(ctx, |ui| {
                ui.set_width(296.0);
                modal_title(ui, "Before you record");
                ui.label(RichText::new(CONSENT_REMINDER).color(ui::INK_SECONDARY));
                ui.add_space(ui::S3);
                ui.horizontal(|ui| {
                    ui::toggle(ui, &mut self.consent_dont_ask);
                    ui.label(RichText::new("Don't remind me again").color(ui::INK_SECONDARY));
                });
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary(ui, "Start recording", Some(ui::Icon::Mic), None, false)
                        .clicked()
                    {
                        start = true;
                    }
                    if ui::outline(ui, "Cancel", None, false).clicked() {
                        self.show_consent = false;
                    }
                });
            });
        if response.should_close() {
            self.show_consent = false;
        }
        if start {
            self.show_consent = false;
            if self.consent_dont_ask {
                self.settings.consent_reminder_dismissed = true;
                self.save_settings();
            }
            self.begin();
        }
    }

    fn close_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("close"))
            .frame(ui::modal_frame())
            .show(ctx, |ui| {
                ui.set_width(296.0);
                modal_title(ui, "Recording in progress");
                ui.label(
                    RichText::new("Stop recording and quit? Everything recorded so far is saved.")
                        .color(ui::INK_SECONDARY),
                );
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary(ui, "Stop and quit", None, None, false).clicked() {
                        close = true;
                    }
                    if ui::outline(ui, "Resume", None, false).clicked() {
                        self.confirm_close = false;
                    }
                });
            });
        if response.should_close() {
            self.confirm_close = false;
        }
        if close {
            self.confirm_close = false;
            self.close(ctx);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        let title = match &self.status {
            Some(st) if st.paused => format!("Paused {} — {APP_NAME}", hms(st.elapsed_ms / 1000)),
            Some(st) => format!("● {} — {APP_NAME}", hms(st.elapsed_ms / 1000)),
            None => APP_NAME.to_string(),
        };
        // Every viewport command triggers a repaint, so send only changes.
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        ui::backdrop(ctx);
        egui::SidePanel::left("rail")
            .exact_width(RAIL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 6,
                right: 0,
                top: 16,
                bottom: 14,
            }))
            .show(ctx, |ui| self.rail(ui));
        if self.view == View::Recordings {
            // The list on the left, the transcript of the chosen recording.
            egui::SidePanel::left("controls")
                .exact_width(LEFT_WIDTH)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin {
                    left: 2,
                    right: 0,
                    top: 14,
                    bottom: 8,
                }))
                .show(ctx, |ui| self.section(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin {
                    left: 6,
                    right: 20,
                    top: 18,
                    bottom: 20,
                }))
                .show(ctx, |ui| self.transcript_pane(ui));
        } else {
            // Record and Settings: one column, centred, no wider than
            // SECTION_MAX.
            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin {
                    left: 2,
                    right: 6,
                    top: 14,
                    bottom: 8,
                }))
                .show(ctx, |ui| self.section(ui));
        }
        self.consent_modal(ctx);
        self.close_modal(ctx);

        if self.recorder.is_some()
            || self.saving.is_some()
            || self.check.is_some()
            || self.smoke_until.is_some()
            || self.screenshot.is_some()
        {
            let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
            ctx.request_repaint_after(if minimized {
                REFRESH_MINIMIZED
            } else {
                REFRESH
            });
        } else if self.job.is_some() || self.download.is_some() || !self.queue.is_empty() {
            ctx.request_repaint_after(REFRESH_BACKGROUND);
        }
    }

    /// Cmd+Q and logout skip the close confirmation; the session is still
    /// closed cleanly before the process exits.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
        self.collect_saved(true);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
        self.collect_saved(true);
    }
}

/// The title of the section column, with something on the right.
fn section_title(ui: &mut egui::Ui, title: &str, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        ui.label(
            RichText::new(title)
                .font(FontId::new(20.0, ui::bold()))
                .color(ui::INK),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(ui::S4);
}

/// A quiet centred note where the transcript would be.
fn empty_state(ui: &mut egui::Ui, title: &str, text: &str) {
    ui.add_space((ui.available_height() * 0.3).max(ui::S4));
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(title)
                .font(FontId::new(15.5, ui::medium()))
                .color(ui::INK_SECONDARY),
        );
        ui.add_space(ui::S1);
        ui.label(ui::muted(text));
    });
}

fn modal_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(FontId::new(17.0, ui::bold()))
            .color(ui::INK),
    );
    ui.add_space(ui::S1);
}

/// Devices take a moment to close; the window does not wait for it.
fn close_in_background(monitor: Monitor) {
    std::thread::spawn(move || drop(monitor));
}

fn current_permissions() -> Vec<(Permission, Access)> {
    permissions::needed()
        .iter()
        .map(|&p| (p, permissions::status(p)))
        .collect()
}

/// The path with the home folder shown as `~`.
fn tilde(path: &std::path::Path) -> String {
    match dirs_home().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn hms(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// "Today at 20:36", "Yesterday at 09:05", "24 Oct at 20:36".
fn when(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    let today = Local::now().date_naive();
    let day = local.date_naive();
    let prefix = if day == today {
        "Today".to_string()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_string()
    } else {
        local.format("%-d %b").to_string()
    };
    format!("{prefix} at {}", local.format("%H:%M"))
}

/// "7 s", "12 min 5 s", "1 h 2 min".
fn duration_words(s: u64) -> String {
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, sec) => format!("{sec} s"),
        (0, min, sec) => format!("{min} min {sec} s"),
        (h, min, _) => format!("{h} h {min} min"),
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) {
    let [w, h] = image.size;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for c in &image.pixels {
        rgba.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let result = File::create(path)
        .map_err(anyhow::Error::from)
        .and_then(|f| {
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()?.write_image_data(&rgba)?;
            Ok(())
        });
    match result {
        Ok(()) => tracing::info!(path = %path.display(), "screenshot saved"),
        Err(e) => tracing::error!(error = %e, "cannot save screenshot"),
    }
}

fn error_dialog(text: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(APP_NAME)
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Keeps a second copy of the app from starting.
fn single_instance() -> Option<File> {
    let dir = paths::config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let file = File::create(dir.join("instance.lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn dev_options() -> DevOptions {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    DevOptions {
        smoke_test: value("--smoke-test").and_then(|v| v.parse().ok()),
        check: args.iter().any(|a| a == "--check"),
        smoke_pause: value("--smoke-pause").and_then(|v| {
            let (a, b) = v.split_once(':')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        }),
        screenshot: value("--screenshot").map(PathBuf::from),
        screenshot_at: value("--screenshot-at").and_then(|v| v.parse().ok()),
        view: value("--view").and_then(|v| match v.as_str() {
            "settings" => Some(View::Settings),
            "recordings" => Some(View::Recordings),
            _ => None,
        }),
        window_height: value("--window-height").and_then(|v| v.parse().ok()),
        window_width: value("--window-width").and_then(|v| v.parse().ok()),
        scroll: value("--scroll").and_then(|v| v.parse().ok()),
        transcribe: args.iter().any(|a| a == "--transcribe"),
        show: value("--show").map(PathBuf::from),
        find: value("--find"),
        rename: value("--rename"),
    }
}

fn main() -> Result<()> {
    let _log_guard = logging::init(None)?;
    let dev = dev_options();
    let Some(_instance) = single_instance() else {
        tracing::warn!("another instance is running");
        error_dialog("Meeting Recorder is already running.");
        return Ok(());
    };

    let (rgba, width, height) = assets::decode(assets::APP_ICON_PNG);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([
                dev.window_width.unwrap_or(1120.0),
                dev.window_height.unwrap_or(760.0),
            ])
            .with_min_inner_size([860.0, 600.0])
            .with_icon(Arc::new(egui::IconData {
                rgba,
                width,
                height,
            })),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, dev)))),
    )
    .map_err(|e| anyhow::anyhow!("cannot open the window: {e}"))?;
    tracing::info!("quit");
    Ok(())
}
