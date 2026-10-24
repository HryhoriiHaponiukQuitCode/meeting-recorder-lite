//! Meeting Recorder tray app: one menu with Start / Stop / Open folder /
//! Quit, a timer and level meters in the icon. All recording work happens
//! in `recorder_app::Recorder` on its own thread; this file is only UI.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod icon;

use std::fs::File;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use recorder_app::settings::Settings;
use recorder_app::{
    APP_NAME, AecMode, CONSENT_REMINDER, Notice, Recorder, RecorderOptions, Source, Status,
    SystemOption, logging, paths,
};
use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const UI_INTERVAL: Duration = Duration::from_millis(200);

enum UserEvent {
    Menu(MenuEvent),
}

struct Items {
    status: MenuItem,
    levels: MenuItem,
    notice: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    open: MenuItem,
    aec_auto: CheckMenuItem,
    aec_on: CheckMenuItem,
    aec_off: CheckMenuItem,
    consent: CheckMenuItem,
    folder: MenuItem,
    logs: MenuItem,
    quit: MenuItem,
}

impl Items {
    fn new() -> Self {
        Self {
            status: MenuItem::new("Не записує", false, None),
            levels: MenuItem::new(levels_text(None), false, None),
            notice: MenuItem::new("Готово до запису", false, None),
            start: MenuItem::new("Почати запис", true, None),
            stop: MenuItem::new("Зупинити запис", false, None),
            open: MenuItem::new("Відкрити папку записів", true, None),
            aec_auto: CheckMenuItem::new("Авто (лише для динаміків)", true, false, None),
            aec_on: CheckMenuItem::new("Завжди", true, false, None),
            aec_off: CheckMenuItem::new("Вимкнено", true, false, None),
            consent: CheckMenuItem::new("Нагадувати про згоду на запис", true, false, None),
            folder: MenuItem::new("Змінити папку записів…", true, None),
            logs: MenuItem::new("Відкрити журнали", true, None),
            quit: MenuItem::new("Вийти", true, None),
        }
    }

    fn menu(&self) -> Result<Menu> {
        let aec = Submenu::with_items(
            "Придушення луни",
            true,
            &[&self.aec_auto, &self.aec_on, &self.aec_off],
        )?;
        let settings = Submenu::with_items(
            "Налаштування",
            true,
            &[
                &aec,
                &self.consent,
                &self.folder,
                &PredefinedMenuItem::separator(),
                &self.logs,
                &MenuItem::new(format!("Версія {}", env!("CARGO_PKG_VERSION")), false, None),
            ],
        )?;
        let menu = Menu::new();
        menu.append_items(&[
            &self.status,
            &self.levels,
            &self.notice,
            &PredefinedMenuItem::separator(),
            &self.start,
            &self.stop,
            &self.open,
            &PredefinedMenuItem::separator(),
            &settings,
            &self.quit,
        ])?;
        Ok(menu)
    }

    fn show_settings(&self, s: &Settings) {
        self.aec_auto.set_checked(s.aec == AecMode::Auto);
        self.aec_on.set_checked(s.aec == AecMode::On);
        self.aec_off.set_checked(s.aec == AecMode::Off);
        self.consent.set_checked(!s.consent_reminder_dismissed);
    }
}

struct App {
    items: Items,
    tray: Option<TrayIcon>,
    settings: Settings,
    recorder: Option<Recorder>,
    /// Last problem reported during this recording, shown in the menu.
    problem: Option<String>,
    last_icon: Option<icon::Dot>,
    last_ui: Instant,
    /// `--smoke-test <seconds>`: record right away, then stop and quit.
    smoke_until: Option<Instant>,
}

impl App {
    fn new() -> Self {
        let settings = Settings::load();
        let items = Items::new();
        items.show_settings(&settings);
        Self {
            items,
            tray: None,
            settings,
            recorder: None,
            problem: None,
            last_icon: None,
            last_ui: Instant::now(),
            smoke_until: None,
        }
    }

    fn create_tray(&mut self) -> Result<()> {
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(self.items.menu()?))
            .with_tooltip(APP_NAME)
            .with_icon(make_icon(icon::Dot::Idle))
            .build()
            .context("cannot create the tray icon")?;
        self.tray = Some(tray);
        self.refresh();
        Ok(())
    }

    fn on_menu(&mut self, ev: MenuEvent) -> bool {
        let id = ev.id;
        let it = &self.items;
        if id == *it.start.id() {
            self.start();
        } else if id == *it.stop.id() {
            self.stop();
        } else if id == *it.open.id() {
            paths::open_in_file_manager(&self.settings.recordings_dir());
        } else if id == *it.aec_auto.id() {
            self.set_aec(AecMode::Auto);
        } else if id == *it.aec_on.id() {
            self.set_aec(AecMode::On);
        } else if id == *it.aec_off.id() {
            self.set_aec(AecMode::Off);
        } else if id == *it.consent.id() {
            self.settings.consent_reminder_dismissed = !it.consent.is_checked();
            self.save_settings();
        } else if id == *it.folder.id() {
            self.change_folder();
        } else if id == *it.logs.id() {
            paths::open_in_file_manager(&paths::log_dir());
        } else if id == *it.quit.id() {
            self.stop();
            return false;
        }
        true
    }

    fn start(&mut self) {
        if self.recorder.is_some() || !self.confirm_consent() {
            return;
        }
        self.begin();
    }

    /// The one-time legal reminder; `false` = the user cancelled.
    fn confirm_consent(&mut self) -> bool {
        if !self.settings.consent_reminder_dismissed {
            let result = MessageDialog::new()
                .set_level(MessageLevel::Info)
                .set_title(APP_NAME)
                .set_description(format!("{CONSENT_REMINDER}\n\nПочати запис?"))
                .set_buttons(MessageButtons::YesNoCancelCustom(
                    "Почати".into(),
                    "Почати й більше не нагадувати".into(),
                    "Скасувати".into(),
                ))
                .show();
            match result {
                MessageDialogResult::Custom(s) if s == "Почати" => {}
                MessageDialogResult::Custom(s) if s.starts_with("Почати й") => {
                    self.settings.consent_reminder_dismissed = true;
                    self.save_settings();
                    self.items.show_settings(&self.settings);
                }
                MessageDialogResult::Yes | MessageDialogResult::Ok => {}
                _ => return false,
            }
        }
        true
    }

    fn begin(&mut self) {
        let opts = RecorderOptions {
            root: self.settings.recordings_dir(),
            part_seconds: self.settings.part_minutes.max(1) * 60,
            bitrate_kbps: self.settings.bitrate_kbps,
            mic_device: self.settings.mic_device.clone(),
            system: SystemOption::Auto,
            aec: self.settings.aec,
        };
        match Recorder::start(opts) {
            Ok(r) => {
                tracing::info!(dir = %r.status().dir.display(), "recording started from tray");
                self.recorder = Some(r);
                self.problem = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start recording");
                error_dialog(&format!("Не вдалося почати запис: {e:#}"));
            }
        }
        self.refresh();
    }

    fn stop(&mut self) {
        let Some(r) = self.recorder.take() else {
            return;
        };
        let dir = r.status().dir;
        self.drain_notices(&r);
        match r.stop() {
            Ok(m) => {
                let seconds: u64 = m.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
                tracing::info!(dir = %dir.display(), seconds, "recording stopped from tray");
                self.items.notice.set_text(format!(
                    "Останній запис: {} ({})",
                    folder_name(&dir),
                    hms(seconds)
                ));
            }
            Err(e) => {
                tracing::error!(error = %e, "recording ended with an error");
                error_dialog(&format!("Запис завершився з помилкою: {e:#}"));
            }
        }
        self.problem = None;
        self.refresh();
    }

    fn set_aec(&mut self, mode: AecMode) {
        self.settings.aec = mode;
        self.save_settings();
        self.items.show_settings(&self.settings);
        if self.recorder.is_some() {
            self.items
                .notice
                .set_text("Придушення луни зміниться з наступного запису");
        }
    }

    fn change_folder(&mut self) {
        let picked = rfd::FileDialog::new()
            .set_title("Папка для записів")
            .set_directory(self.settings.recordings_dir())
            .pick_folder();
        if let Some(dir) = picked {
            tracing::info!(dir = %dir.display(), "recordings folder changed");
            self.settings.recordings_dir = Some(dir);
            self.save_settings();
        }
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.save() {
            tracing::error!(error = %e, "cannot save settings");
        }
    }

    fn drain_notices(&mut self, r: &Recorder) {
        while let Some(n) = r.try_notice() {
            tracing::info!(notice = ?n, "notice");
            let text = n.message();
            if n.is_problem() {
                self.problem = Some(text.clone());
            }
            match &n {
                Notice::PermissionDenied(source) => permission_dialog(*source, &text),
                Notice::WriteFailed(_) => error_dialog(&text),
                _ => {}
            }
            self.items.notice.set_text(text);
        }
    }

    /// Called every `UI_INTERVAL`: notices, finished sessions, icon and menu.
    fn tick(&mut self) {
        self.last_ui = Instant::now();
        let Some(r) = self.recorder.take() else {
            return;
        };
        self.drain_notices(&r);
        if r.status().running {
            self.recorder = Some(r);
        } else {
            // The session ended on its own (write error); collect the result.
            self.recorder = Some(r);
            self.stop();
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        let status = self.recorder.as_ref().map(Recorder::status);
        let it = &self.items;
        it.start.set_enabled(status.is_none());
        it.stop.set_enabled(status.is_some());
        let dot = match &status {
            None => icon::Dot::Idle,
            Some(_) if self.problem.is_some() => icon::Dot::Problem,
            Some(_) => icon::Dot::Recording,
        };
        match &status {
            None => {
                it.status.set_text("Не записує");
                it.levels.set_text(levels_text(None));
            }
            Some(st) => {
                it.status.set_text(format!(
                    "● Запис {}  ·  частина {}",
                    hms(st.elapsed_ms / 1000),
                    st.part
                ));
                it.levels.set_text(levels_text(Some(st)));
            }
        }
        let Some(tray) = &self.tray else {
            return;
        };
        if self.last_icon != Some(dot) {
            self.last_icon = Some(dot);
            if let Err(e) = tray.set_icon(Some(make_icon(dot))) {
                tracing::warn!(error = %e, "cannot update the tray icon");
            }
        }
        let timer = status.as_ref().map(|st| hms(st.elapsed_ms / 1000));
        #[cfg(target_os = "macos")]
        tray.set_title(timer.as_deref());
        let tooltip = match (&timer, &self.problem) {
            (None, _) => APP_NAME.to_string(),
            (Some(t), None) => format!("{APP_NAME} — запис {t}"),
            (Some(t), Some(p)) => format!("{APP_NAME} — запис {t}\n⚠ {p}"),
        };
        let _ = tray.set_tooltip(Some(tooltip));
    }
}

fn make_icon(dot: icon::Dot) -> Icon {
    let (rgba, size) = icon::render(dot);
    Icon::from_rgba(rgba, size, size).expect("valid icon size")
}

fn levels_text(st: Option<&Status>) -> String {
    let bar = |peak: f32| {
        let n = icon::level_steps(peak) as usize;
        format!("{}{}", "▮".repeat(n), "▯".repeat(icon::STEPS as usize - n))
    };
    match st {
        None => "Мікрофон / система: —".into(),
        Some(st) => format!("Мік {}   Система {}", bar(st.levels[0]), bar(st.levels[1])),
    }
}

fn hms(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn folder_name(dir: &std::path::Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn error_dialog(text: &str) {
    MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title(APP_NAME)
        .set_description(text)
        .set_buttons(MessageButtons::Ok)
        .show();
}

fn permission_dialog(source: Source, text: &str) {
    #[cfg(target_os = "macos")]
    {
        let result = MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title(APP_NAME)
            .set_description(text)
            .set_buttons(MessageButtons::OkCancelCustom(
                "Відкрити налаштування".into(),
                "Закрити".into(),
            ))
            .show();
        if matches!(&result, MessageDialogResult::Custom(s) if s.starts_with("Відкрити")) {
            let pane = match source {
                Source::Mic => "Privacy_Microphone",
                Source::System => "Privacy_ScreenCapture",
            };
            recorder_capture::macos::permissions::open_privacy_settings(pane);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = source;
        MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title(APP_NAME)
            .set_description(text)
            .set_buttons(MessageButtons::Ok)
            .show();
    }
}

/// Keeps a second copy of the app from starting.
fn single_instance() -> Option<File> {
    let dir = paths::config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let file = File::create(dir.join("instance.lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn main() -> Result<()> {
    let _log_guard = logging::init(None)?;
    let args: Vec<String> = std::env::args().collect();
    let smoke_test = args
        .iter()
        .position(|a| a == "--smoke-test")
        .and_then(|i| args.get(i + 1)?.parse::<u64>().ok());
    let Some(_instance) = single_instance() else {
        tracing::warn!("another instance is running");
        error_dialog("Meeting Recorder уже запущено — шукайте його значок у треї.");
        return Ok(());
    };

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    let mut app = App::new();
    event_loop.run(move |event, _, control_flow| {
        match event {
            // The tray must be created once the loop runs (macOS requirement).
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.create_tray() {
                    tracing::error!(error = %e, "tray init failed");
                    error_dialog(&format!("Не вдалося створити значок у треї: {e:#}"));
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                if let Some(seconds) = smoke_test {
                    // Hidden developer option for checking permissions and
                    // capture in the real bundle; skips the consent dialog.
                    tracing::info!(seconds, "smoke test");
                    app.begin();
                    app.smoke_until = Some(Instant::now() + Duration::from_secs(seconds));
                }
            }
            Event::UserEvent(UserEvent::Menu(ev)) => {
                if !app.on_menu(ev) {
                    app.tray = None;
                    tracing::info!("quit");
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            Event::LoopDestroyed => app.stop(),
            _ => {}
        }
        if app.smoke_until.is_some_and(|t| Instant::now() >= t) {
            app.stop();
            app.tray = None;
            *control_flow = ControlFlow::Exit;
            return;
        }
        if app.recorder.is_some() {
            if app.last_ui.elapsed() >= UI_INTERVAL {
                app.tick();
            }
            *control_flow = ControlFlow::WaitUntil(app.last_ui + UI_INTERVAL);
        } else if *control_flow != ControlFlow::Exit {
            *control_flow = ControlFlow::Wait;
        }
    });
}
