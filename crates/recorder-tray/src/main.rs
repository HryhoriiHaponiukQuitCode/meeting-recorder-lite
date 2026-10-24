//! Meeting Recorder window app: a transport button, timer, channel meters,
//! warnings, the last recording and a settings screen. All recording work
//! happens in `recorder_app::Recorder` on its own thread; this file is UI.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod assets;
mod ui;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use eframe::egui::{self, Align, FontId, Layout, RichText, TextureHandle, ViewportCommand, vec2};
use recorder_app::permissions::{self, Access, Permission};
use recorder_app::settings::Settings;
use recorder_app::{
    APP_NAME, AecMode, CONSENT_REMINDER, Notice, Recorder, RecorderOptions, Source, Status,
    SystemOption, logging, paths,
};

/// How often the window refreshes while recording (timer, meters).
const REFRESH: Duration = Duration::from_millis(200);

#[cfg(target_os = "macos")]
const REVEAL: &str = "Показати у Finder";
#[cfg(not(target_os = "macos"))]
const REVEAL: &str = "Показати в Провіднику";

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Settings,
}

struct Message {
    text: String,
    problem: bool,
    /// macOS: the privacy pane that fixes this problem.
    settings_pane: Option<&'static str>,
}

struct LastRecording {
    dir: PathBuf,
    started: DateTime<Utc>,
    seconds: u64,
}

/// Hidden developer options (`--smoke-test`, `--screenshot`, …).
#[derive(Default)]
struct DevOptions {
    smoke_test: Option<u64>,
    screenshot: Option<PathBuf>,
    screenshot_at: Option<f32>,
    view: Option<View>,
    theme: Option<egui::Theme>,
    window_height: Option<f32>,
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
    screenshot: Option<(PathBuf, f32)>,
    /// Privacy permissions and when they were last checked.
    permissions: Vec<(Permission, Access)>,
    permissions_checked: Instant,
    /// A system permission prompt is open.
    asking: Arc<AtomicBool>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, dev: DevOptions) -> Self {
        ui::install(&cc.egui_ctx);
        if let Some(theme) = dev.theme {
            cc.egui_ctx.set_theme(theme);
        }
        let (px, w, h) = assets::decode(assets::LOGO_PNG);
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px),
            egui::TextureOptions::LINEAR,
        );
        let mut app = Self {
            settings: Settings::load(),
            view: dev.view.unwrap_or(View::Main),
            recorder: None,
            status: None,
            message: None,
            last: None,
            logo,
            show_consent: false,
            consent_dont_ask: false,
            confirm_close: false,
            closing: false,
            title: APP_NAME.to_string(),
            started: Instant::now(),
            smoke_until: None,
            screenshot: dev
                .screenshot
                .map(|p| (p, dev.screenshot_at.unwrap_or(2.0))),
            permissions: current_permissions(),
            permissions_checked: Instant::now(),
            asking: Arc::new(AtomicBool::new(false)),
        };
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
        let p = ui::palette(ui);
        let asking = self.asking.load(Ordering::SeqCst);
        let mut request = None;
        ui::card(ui, |ui| {
            ui::eyebrow(ui, "Потрібні дозволи");
            ui.label(
                RichText::new("Щоб записувати ваш голос і голоси співрозмовників.")
                    .color(p.ink_muted)
                    .size(12.5),
            );
            ui.add_space(ui::S1);
            for (perm, access) in self.permissions.clone() {
                ui.horizontal(|ui| {
                    ui.set_min_height(30.0);
                    ui.label(RichText::new(perm.title()).color(p.ink));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match access {
                        Access::Granted => {
                            ui.label(RichText::new("Дозволено").color(p.signal));
                            ui::check_mark(ui, p.signal);
                        }
                        Access::NotAsked => {
                            let label = if asking {
                                "Чекаю відповіді…"
                            } else {
                                "Дозволити"
                            };
                            let button =
                                egui::Button::new(RichText::new(label).color(egui::Color32::WHITE))
                                    .fill(p.brand)
                                    .min_size(vec2(0.0, 28.0));
                            if ui.add_enabled(!asking, button).clicked() {
                                request = Some(perm);
                            }
                        }
                        Access::Denied => {
                            if ui::button(ui, "Увімкнути").clicked() {
                                permissions::open_settings(perm);
                            }
                        }
                    });
                });
            }
            if self.permissions.iter().any(|(_, a)| *a == Access::Denied) {
                ui.label(
                    RichText::new(
                        "Доступ було відхилено, тому macOS більше не питає. «Увімкнути» відкриє потрібний розділ налаштувань: увімкніть перемикач навпроти Meeting Recorder — програма побачить це одразу.",
                    )
                    .color(p.ink_muted)
                    .size(12.5),
                );
            }
        });
        ui.add_space(ui::S3);
        if let Some(perm) = request {
            self.ask(ui.ctx(), vec![perm]);
        }
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
                self.last = Some(LastRecording {
                    dir,
                    started: m.started_at_utc,
                    seconds,
                });
                // Information about the finished session is now stale.
                if self.message.as_ref().is_some_and(|m| !m.problem) {
                    self.message = None;
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
            if matches!(n, Notice::PermissionDenied(_)) {
                // The permissions card shows this, with the button that fixes it.
                self.permissions = current_permissions();
                continue;
            }
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

    /// Notices, a session that ended on its own, timers, close requests.
    fn poll(&mut self, ctx: &egui::Context) {
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

    fn header(&mut self, ui: &mut egui::Ui) {
        let p = ui::palette(ui);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(26.0, 26.0)));
            ui.add_space(ui::S1);
            ui.label(
                RichText::new(APP_NAME)
                    .font(FontId::new(15.0, ui::semibold()))
                    .color(p.ink),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui::icon_button(ui, ui::Icon::Settings, "Налаштування").clicked() {
                    self.view = View::Settings;
                }
            });
        });
    }

    fn transport(&mut self, ui: &mut egui::Ui) {
        let p = ui::palette(ui);
        let st = self.status.clone();
        ui.vertical_centered(|ui| {
            let (lamp, caption) = match &st {
                None => (p.ink_muted, "Готово до запису".to_string()),
                Some(s) => (p.rec, format!("Запис · частина {}", s.part)),
            };
            ui.horizontal(|ui| {
                // Centre "● CAPTION" as one unit.
                let text = RichText::new(caption.to_uppercase())
                    .font(FontId::new(11.0, ui::semibold()))
                    .color(lamp)
                    .extra_letter_spacing(0.8);
                let galley = egui::WidgetText::from(text.clone()).into_galley(
                    ui,
                    Some(egui::TextWrapMode::Extend),
                    f32::INFINITY,
                    egui::TextStyle::Body,
                );
                let width = 8.0 + ui::S2 + galley.size().x;
                ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                let (dot, _) = ui.allocate_exact_size(vec2(8.0, 14.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, lamp);
                ui.label(text);
            });
            let elapsed = st.as_ref().map_or(0, |s| s.elapsed_ms / 1000);
            ui.label(
                RichText::new(hms(elapsed))
                    .font(FontId::monospace(40.0))
                    .color(if st.is_some() { p.ink } else { p.ink_secondary }),
            );
            ui.add_space(ui::S3);
            if ui::record_button(ui, st.is_some()).clicked() {
                if st.is_some() {
                    self.stop();
                } else {
                    self.start();
                }
            }
            ui.add_space(ui::S1);
            ui.label(
                RichText::new(if st.is_some() {
                    "Зупинити запис"
                } else {
                    "Почати запис"
                })
                .color(p.ink_secondary),
            );
        });
    }

    fn channels(&self, ui: &mut egui::Ui) {
        let levels = self.status.as_ref().map(|s| s.levels);
        ui::card(ui, |ui| {
            ui::eyebrow(ui, "Канали");
            ui.add_space(ui::S1);
            ui::channel_meter(ui, "Ви", "мікрофон", levels.map(|l| l[0]));
            ui.add_space(ui::S2);
            ui::channel_meter(ui, "Співрозмовники", "системний звук", levels.map(|l| l[1]));
        });
    }

    fn message(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &self.message else {
            return;
        };
        let p = ui::palette(ui);
        let accent = if m.problem { p.caution } else { p.brand };
        let mut open_pane = None;
        let mut dismiss = false;
        let frame = egui::Frame::new()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0_f32, p.hairline))
            .corner_radius(ui::R_CARD)
            .inner_margin(egui::Margin {
                left: 18,
                right: 10,
                top: 12,
                bottom: 12,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if ui::icon_button(ui, ui::Icon::Close, "Сховати").clicked() {
                            dismiss = true;
                        }
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.label(RichText::new(&m.text).color(p.ink));
                            if let Some(pane) = m.settings_pane
                                && ui::link(ui, "Відкрити налаштування приватності").clicked()
                            {
                                open_pane = Some(pane);
                            }
                        });
                    });
                });
            });
        // Accent bar along the left edge, inside the rounded card.
        let r = frame.response.rect;
        let bar = egui::Rect::from_min_max(
            egui::pos2(r.left() + 7.0, r.top() + 12.0),
            egui::pos2(r.left() + 10.0, r.bottom() - 12.0),
        );
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), accent);
        if let Some(pane) = open_pane {
            open_privacy_settings(pane);
        }
        if dismiss {
            self.message = None;
        }
    }

    fn last_recording(&self, ui: &mut egui::Ui) {
        let p = ui::palette(ui);
        let Some(last) = &self.last else {
            ui.vertical_centered(|ui| {
                if ui::link(ui, "Відкрити папку записів").clicked() {
                    paths::open_in_file_manager(&self.settings.recordings_dir());
                }
            });
            return;
        };
        ui::card(ui, |ui| {
            ui::eyebrow(ui, "Останній запис");
            ui.add_space(ui::S1);
            ui.horizontal(|ui| {
                ui.label(RichText::new(when(last.started)).color(p.ink));
                ui.label(
                    RichText::new(format!("· {}", duration_words(last.seconds))).color(p.ink_muted),
                );
            });
            ui.horizontal(|ui| {
                if ui::link(ui, REVEAL).clicked() {
                    paths::open_in_file_manager(&last.dir);
                }
                ui.label(RichText::new("·").color(p.ink_muted));
                if ui::link(ui, "Усі записи").clicked() {
                    paths::open_in_file_manager(&self.settings.recordings_dir());
                }
            });
        });
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        ui.add_space(ui::S4);
        self.permissions_card(ui);
        ui.add_space(ui::S2);
        self.transport(ui);
        ui.add_space(ui::S4 + ui::S1);
        self.channels(ui);
        ui.add_space(ui::S3);
        self.message(ui);
        if self.message.is_some() {
            ui.add_space(ui::S3);
        }
        self.last_recording(ui);
    }

    // ── Settings screen ────────────────────────────────────────────────

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        let p = ui::palette(ui);
        ui.horizontal(|ui| {
            if ui::icon_button(ui, ui::Icon::Back, "Назад").clicked() {
                self.view = View::Main;
            }
            ui.label(
                RichText::new("Налаштування")
                    .font(FontId::new(15.0, ui::semibold()))
                    .color(p.ink),
            );
        });
        ui.add_space(ui::S6);

        ui::eyebrow(ui, "Придушення луни");
        ui.add_space(ui::S1);
        let changed = ui::segmented(
            ui,
            &mut self.settings.aec,
            &[
                (AecMode::Auto, "Авто"),
                (AecMode::On, "Завжди"),
                (AecMode::Off, "Вимкнено"),
            ],
        );
        let hint = match self.settings.aec {
            AecMode::Auto => {
                "Вмикається, коли звук іде в динаміки, і прибирає голоси співрозмовників із вашого каналу."
            }
            AecMode::On => "Завжди очищати канал мікрофона від звуку з динаміків.",
            AecMode::Off => "Мікрофон пишеться без обробки. Підходить для навушників.",
        };
        ui.label(RichText::new(hint).color(p.ink_muted).size(12.5));
        if changed {
            self.save_settings();
            if self.recorder.is_some() {
                self.message = Some(Message {
                    text: "Придушення луни зміниться з наступного запису.".into(),
                    problem: false,
                    settings_pane: None,
                });
            }
        }
        ui.add_space(ui::S6);

        ui::eyebrow(ui, "Записи");
        ui.add_space(ui::S1);
        ui::card(ui, |ui| {
            ui.label(RichText::new("Папка").color(p.ink_secondary).size(12.5));
            ui.label(RichText::new(tilde(&self.settings.recordings_dir())).color(p.ink));
            ui.add_space(ui::S2);
            ui.horizontal(|ui| {
                if ui::button(ui, "Змінити…").clicked() {
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
                if ui::button(ui, "Відкрити").clicked() {
                    paths::open_in_file_manager(&self.settings.recordings_dir());
                }
            });
            ui.add_space(ui::S3);
            ui.separator();
            ui.add_space(ui::S2);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Нагадувати про згоду").color(p.ink));
                    ui.label(
                        RichText::new("Перед кожним записом")
                            .color(p.ink_muted)
                            .size(12.5),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut remind = !self.settings.consent_reminder_dismissed;
                    if ui::toggle(ui, &mut remind).changed() {
                        self.settings.consent_reminder_dismissed = !remind;
                        self.save_settings();
                    }
                });
            });
        });
        ui.add_space(ui::S6);

        ui::eyebrow(ui, "Діагностика");
        ui.add_space(ui::S1);
        ui.horizontal(|ui| {
            if ui::link(ui, "Відкрити журнали").clicked() {
                paths::open_in_file_manager(&paths::log_dir());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("Версія {}", env!("CARGO_PKG_VERSION")))
                        .color(p.ink_muted)
                        .size(12.5),
                );
            });
        });
    }

    // ── Dialogs ────────────────────────────────────────────────────────

    fn consent_modal(&mut self, ctx: &egui::Context) {
        if !self.show_consent {
            return;
        }
        let mut start = false;
        let response = egui::Modal::new(egui::Id::new("consent"))
            .frame(modal_frame(ctx))
            .show(ctx, |ui| {
                let p = ui::palette(ui);
                ui.set_width(296.0);
                ui.label(
                    RichText::new("Перед записом")
                        .font(FontId::new(16.0, ui::semibold()))
                        .color(p.ink),
                );
                ui.add_space(ui::S1);
                ui.label(RichText::new(CONSENT_REMINDER).color(p.ink_secondary));
                ui.add_space(ui::S3);
                ui.horizontal(|ui| {
                    ui::toggle(ui, &mut self.consent_dont_ask);
                    ui.label(RichText::new("Більше не нагадувати").color(p.ink_secondary));
                });
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button(p, "Почати запис")).clicked() {
                        start = true;
                    }
                    if ui::button(ui, "Скасувати").clicked() {
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
            .frame(modal_frame(ctx))
            .show(ctx, |ui| {
                let p = ui::palette(ui);
                ui.set_width(296.0);
                ui.label(
                    RichText::new("Іде запис")
                        .font(FontId::new(16.0, ui::semibold()))
                        .color(p.ink),
                );
                ui.add_space(ui::S1);
                ui.label(
                    RichText::new(
                        "Зупинити запис і закрити програму? Усе записане буде збережено.",
                    )
                    .color(p.ink_secondary),
                );
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button(p, "Зупинити й закрити")).clicked() {
                        close = true;
                    }
                    if ui::button(ui, "Продовжити запис").clicked() {
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
            Some(st) => format!("● {} — {APP_NAME}", hms(st.elapsed_ms / 1000)),
            None => APP_NAME.to_string(),
        };
        // Every viewport command triggers a repaint, so send only changes.
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::TopBottomPanel::bottom("footer")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 24,
                        top: 4,
                        bottom: 14,
                    }),
            )
            .show(ctx, |ui| {
                let p = ui::palette(ui);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(CONSENT_REMINDER)
                            .color(p.ink_muted)
                            .size(11.0),
                    );
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin {
                        left: 20,
                        right: 12,
                        top: 16,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // An 8 px gutter on the right holds the floating scroll bar.
                    egui::Frame::new()
                        .inner_margin(egui::Margin {
                            right: 8,
                            ..Default::default()
                        })
                        .show(ui, |ui| match self.view {
                            View::Main => self.main_screen(ui),
                            View::Settings => self.settings_screen(ui),
                        });
                });
            });
        self.consent_modal(ctx);
        self.close_modal(ctx);

        if self.recorder.is_some() || self.smoke_until.is_some() || self.screenshot.is_some() {
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

fn modal_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::popup(&ctx.style())
        .inner_margin(20)
        .corner_radius(ui::R_CARD + 2)
}

fn primary_button(p: ui::Palette, text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(egui::Color32::WHITE))
        .fill(p.rec)
        .min_size(vec2(0.0, 30.0))
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

/// "Сьогодні о 20:36", "Учора о 09:05", "24.10 о 20:36".
fn when(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    let today = Local::now().date_naive();
    let day = local.date_naive();
    let prefix = if day == today {
        "Сьогодні".to_string()
    } else if today.pred_opt() == Some(day) {
        "Учора".to_string()
    } else {
        local.format("%d.%m").to_string()
    };
    format!("{prefix} о {}", local.format("%H:%M"))
}

/// "7 с", "12 хв 5 с", "1 год 2 хв".
fn duration_words(s: u64) -> String {
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, sec) => format!("{sec} с"),
        (0, min, sec) => format!("{min} хв {sec} с"),
        (h, min, _) => format!("{h} год {min} хв"),
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

fn dev_options() -> DevOptions {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    DevOptions {
        smoke_test: value("--smoke-test").and_then(|v| v.parse().ok()),
        screenshot: value("--screenshot").map(PathBuf::from),
        screenshot_at: value("--screenshot-at").and_then(|v| v.parse().ok()),
        view: value("--view").and_then(|v| (v == "settings").then_some(View::Settings)),
        window_height: value("--window-height").and_then(|v| v.parse().ok()),
        theme: value("--theme").and_then(|v| match v.as_str() {
            "light" => Some(egui::Theme::Light),
            "dark" => Some(egui::Theme::Dark),
            _ => None,
        }),
    }
}

fn main() -> Result<()> {
    let _log_guard = logging::init(None)?;
    let dev = dev_options();
    let Some(_instance) = single_instance() else {
        tracing::warn!("another instance is running");
        error_dialog("Meeting Recorder уже запущено.");
        return Ok(());
    };

    let (rgba, width, height) = assets::decode(assets::APP_ICON_PNG);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([380.0, dev.window_height.unwrap_or(700.0)])
            .with_min_inner_size([340.0, 560.0])
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
