//! Meeting Recorder window app: Start / Stop, timer, level meters for both
//! channels, warnings and settings. All recording work happens in
//! `recorder_app::Recorder` on its own thread; this file is only UI.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod assets;

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, RichText, Sense, TextureHandle, ViewportCommand,
    vec2,
};
use recorder_app::settings::Settings;
use recorder_app::{
    APP_NAME, AecMode, CONSENT_REMINDER, Notice, Recorder, RecorderOptions, Source, Status,
    SystemOption, logging, paths,
};

/// How often the window refreshes while recording (timer, meters).
const REFRESH: Duration = Duration::from_millis(200);

const RED: Color32 = Color32::from_rgb(222, 49, 56);
const GREEN: Color32 = Color32::from_rgb(52, 199, 89);
const AMBER: Color32 = Color32::from_rgb(240, 150, 10);

struct Message {
    text: String,
    problem: bool,
    /// macOS: the privacy pane that fixes this problem.
    settings_pane: Option<&'static str>,
}

struct App {
    settings: Settings,
    recorder: Option<Recorder>,
    status: Option<Status>,
    message: Option<Message>,
    logo: TextureHandle,
    show_consent: bool,
    consent_dont_ask: bool,
    confirm_close: bool,
    closing: bool,
    title: String,
    /// `--smoke-test <seconds>`: record right away, then stop and quit.
    smoke_until: Option<Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, smoke_test: Option<u64>) -> Self {
        let (px, w, h) = assets::decode(assets::LOGO_PNG);
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px),
            egui::TextureOptions::LINEAR,
        );
        let mut app = Self {
            settings: Settings::load(),
            recorder: None,
            status: None,
            message: None,
            logo,
            show_consent: false,
            consent_dont_ask: false,
            confirm_close: false,
            closing: false,
            title: APP_NAME.to_string(),
            smoke_until: None,
        };
        if let Some(seconds) = smoke_test {
            // Hidden developer option for checking permissions and capture in
            // the real bundle; skips the consent reminder.
            tracing::info!(seconds, "smoke test");
            app.begin();
            app.smoke_until = Some(Instant::now() + Duration::from_secs(seconds));
        }
        app
    }

    fn start(&mut self) {
        if self.recorder.is_some() {
            return;
        }
        if self.settings.consent_reminder_dismissed {
            self.begin();
        } else {
            self.consent_dont_ask = false;
            self.show_consent = true;
        }
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
                tracing::info!(dir = %r.status().dir.display(), "recording started from the window");
                self.status = Some(r.status());
                self.recorder = Some(r);
                self.message = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start recording");
                self.message = Some(Message {
                    text: format!("Не вдалося почати запис: {e:#}"),
                    problem: true,
                    settings_pane: None,
                });
            }
        }
    }

    fn stop(&mut self) {
        let Some(r) = self.recorder.take() else {
            return;
        };
        self.drain_notices(&r);
        let dir = r.status().dir;
        self.status = None;
        match r.stop() {
            Ok(m) => {
                let seconds: u64 = m.parts.iter().map(|p| p.duration_ms).sum::<u64>() / 1000;
                tracing::info!(dir = %dir.display(), seconds, "recording stopped from the window");
                let keep_problem = self.message.as_ref().is_some_and(|m| m.problem);
                if !keep_problem {
                    self.message = Some(Message {
                        text: format!("Збережено: {} ({})", folder_name(&dir), hms(seconds)),
                        problem: false,
                        settings_pane: None,
                    });
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "recording ended with an error");
                self.message = Some(Message {
                    text: format!("Запис завершився з помилкою: {e:#}"),
                    problem: true,
                    settings_pane: None,
                });
            }
        }
    }

    fn drain_notices(&mut self, r: &Recorder) {
        while let Some(n) = r.try_notice() {
            tracing::info!(notice = ?n, "notice");
            // Information never hides an unresolved problem.
            if !n.is_problem() && self.message.as_ref().is_some_and(|m| m.problem) {
                continue;
            }
            let settings_pane = match &n {
                Notice::PermissionDenied(Source::Mic) => Some("Privacy_Microphone"),
                Notice::PermissionDenied(Source::System) => Some("Privacy_ScreenCapture"),
                _ => None,
            };
            self.message = Some(Message {
                text: n.message(),
                problem: n.is_problem(),
                settings_pane: settings_pane.filter(|_| cfg!(target_os = "macos")),
            });
        }
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.save() {
            tracing::error!(error = %e, "cannot save settings");
        }
    }

    /// Notices, a session that ended on its own, the smoke-test timer.
    fn poll(&mut self, ctx: &egui::Context) {
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
        if self.smoke_until.is_some_and(|t| Instant::now() >= t) {
            self.smoke_until = None;
            self.stop();
            self.closing = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.recorder.is_some() && !self.closing
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(36.0, 36.0)));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(APP_NAME).size(20.0).strong());
                ui.label(RichText::new("Запис онлайн-дзвінків").weak());
            });
        });
    }

    fn recording_panel(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        ui.vertical_centered(|ui| {
            let elapsed = st.as_ref().map_or(0, |s| s.elapsed_ms / 1000);
            ui.label(RichText::new(hms(elapsed)).size(44.0).monospace());
            let caption = match &st {
                None => RichText::new("Не записує").weak(),
                Some(s) => RichText::new(format!("Іде запис · частина {}", s.part)).color(RED),
            };
            ui.label(caption);
            ui.add_space(10.0);
            let (text, fill) = if st.is_some() {
                ("Зупинити запис", Color32::from_gray(90))
            } else {
                ("Почати запис", RED)
            };
            let button = egui::Button::new(RichText::new(text).size(17.0).color(Color32::WHITE))
                .fill(fill)
                .corner_radius(10.0)
                .min_size(vec2(ui.available_width(), 46.0));
            if ui.add(button).clicked() {
                if st.is_some() {
                    self.stop();
                } else {
                    self.start();
                }
            }
        });
        ui.add_space(14.0);
        let levels = st.as_ref().map(|s| s.levels);
        meter(ui, "Мікрофон (ви)", levels.map(|l| l[0]));
        ui.add_space(6.0);
        meter(ui, "Системний звук (співрозмовники)", levels.map(|l| l[1]));
    }

    fn message_panel(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &self.message else {
            return;
        };
        let stroke = if m.problem {
            AMBER
        } else {
            ui.visuals().weak_text_color()
        };
        let text = if m.problem {
            AMBER
        } else {
            ui.visuals().text_color()
        };
        let mut open_pane = None;
        let mut dismiss = false;
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0_f32, stroke))
            .corner_radius(8.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&m.text).color(text));
                ui.horizontal(|ui| {
                    if let Some(pane) = m.settings_pane
                        && ui.button("Відкрити налаштування").clicked()
                    {
                        open_pane = Some(pane);
                    }
                    if ui.small_button("Сховати").clicked() {
                        dismiss = true;
                    }
                });
            });
        if let Some(pane) = open_pane {
            open_privacy_settings(pane);
        }
        if dismiss {
            self.message = None;
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Налаштування")
            .id_salt("settings")
            .show(ui, |ui| {
                ui.label("Придушення луни в каналі мікрофона:");
                let before = self.settings.aec;
                ui.radio_value(
                    &mut self.settings.aec,
                    AecMode::Auto,
                    "Авто — лише коли звук іде в динаміки",
                );
                ui.radio_value(&mut self.settings.aec, AecMode::On, "Завжди");
                ui.radio_value(&mut self.settings.aec, AecMode::Off, "Вимкнено");
                if self.settings.aec != before {
                    self.save_settings();
                    if self.recorder.is_some() {
                        self.message = Some(Message {
                            text: "Придушення луни зміниться з наступного запису.".into(),
                            problem: false,
                            settings_pane: None,
                        });
                    }
                }
                ui.add_space(6.0);
                let mut remind = !self.settings.consent_reminder_dismissed;
                if ui
                    .checkbox(&mut remind, "Нагадувати про згоду перед записом")
                    .changed()
                {
                    self.settings.consent_reminder_dismissed = !remind;
                    self.save_settings();
                }
                ui.add_space(6.0);
                ui.label("Папка записів:");
                ui.label(
                    RichText::new(self.settings.recordings_dir().display().to_string())
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Змінити…").clicked() {
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
                    if ui.button("Відкрити журнали").clicked() {
                        paths::open_in_file_manager(&paths::log_dir());
                    }
                });
            });
    }

    fn consent_modal(&mut self, ctx: &egui::Context) {
        if !self.show_consent {
            return;
        }
        let mut start = false;
        let response = egui::Modal::new(egui::Id::new("consent")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(RichText::new("Перед записом").size(17.0).strong());
            ui.add_space(6.0);
            ui.label(CONSENT_REMINDER);
            ui.add_space(8.0);
            ui.checkbox(&mut self.consent_dont_ask, "Більше не нагадувати");
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(red_button("Почати запис")).clicked() {
                    start = true;
                }
                if ui.button("Скасувати").clicked() {
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
        let response = egui::Modal::new(egui::Id::new("close")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(RichText::new("Іде запис").size(17.0).strong());
            ui.add_space(6.0);
            ui.label("Зупинити запис і закрити програму? Усе записане буде збережено.");
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(red_button("Зупинити й закрити")).clicked() {
                    close = true;
                }
                if ui.button("Продовжити запис").clicked() {
                    self.confirm_close = false;
                }
            });
        });
        if response.should_close() {
            self.confirm_close = false;
        }
        if close {
            self.confirm_close = false;
            self.stop();
            self.closing = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        let title = match &self.status {
            Some(st) => format!("● {} — {APP_NAME}", hms(st.elapsed_ms / 1000)),
            None => APP_NAME.to_string(),
        };
        // Every viewport command triggers a repaint, so send only changes.
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new(CONSENT_REMINDER).small().weak());
                ui.label(
                    RichText::new(format!("Версія {}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .weak(),
                );
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.header(ui);
                    ui.add_space(14.0);
                    self.recording_panel(ui);
                    ui.add_space(12.0);
                    self.message_panel(ui);
                    ui.add_space(8.0);
                    let open = egui::Button::new("Відкрити папку записів")
                        .min_size(vec2(ui.available_width(), 32.0));
                    if ui.add(open).clicked() {
                        paths::open_in_file_manager(&self.settings.recordings_dir());
                    }
                    ui.add_space(8.0);
                    self.settings_panel(ui);
                });
            });
        self.consent_modal(ctx);
        self.close_modal(ctx);

        if self.recorder.is_some() || self.smoke_until.is_some() {
            ctx.request_repaint_after(REFRESH);
        }
    }

    /// Cmd+Q and logout skip the close confirmation; the session is still
    /// closed cleanly before the process exits.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

fn red_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(RED)
}

/// A labelled level bar: 0 dBFS full, −60 dBFS empty.
fn meter(ui: &mut egui::Ui, label: &str, peak: Option<f32>) {
    ui.label(RichText::new(label).small());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(5), ui.visuals().extreme_bg_color);
    let Some(peak) = peak else {
        return;
    };
    let db = if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        -120.0
    };
    let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    if fill > 0.0 {
        let mut bar = rect;
        bar.set_width(rect.width() * fill);
        let color = if db > -3.0 {
            RED
        } else if db > -12.0 {
            AMBER
        } else {
            GREEN
        };
        painter.rect_filled(bar, CornerRadius::same(5), color);
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

fn open_privacy_settings(pane: &str) {
    #[cfg(target_os = "macos")]
    recorder_capture::macos::permissions::open_privacy_settings(pane);
    #[cfg(not(target_os = "macos"))]
    let _ = pane;
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

fn main() -> Result<()> {
    let _log_guard = logging::init(None)?;
    let args: Vec<String> = std::env::args().collect();
    let smoke_test = args
        .iter()
        .position(|a| a == "--smoke-test")
        .and_then(|i| args.get(i + 1)?.parse::<u64>().ok());
    let Some(_instance) = single_instance() else {
        tracing::warn!("another instance is running");
        error_dialog("Meeting Recorder уже запущено.");
        return Ok(());
    };

    let (rgba, width, height) = assets::decode(assets::APP_ICON_PNG);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([380.0, 560.0])
            .with_min_inner_size([340.0, 460.0])
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
        Box::new(move |cc| Ok(Box::new(App::new(cc, smoke_test)))),
    )
    .map_err(|e| anyhow::anyhow!("cannot open the window: {e}"))?;
    tracing::info!("quit");
    Ok(())
}
