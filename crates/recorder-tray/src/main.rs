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
        let asking = self.asking.load(Ordering::SeqCst);
        let granted = self
            .permissions
            .iter()
            .filter(|(_, a)| *a == Access::Granted)
            .count();
        let total = self.permissions.len();
        let mut request = None;
        ui::card(ui, |ui| {
            ui::card_header(ui, "Потрібні дозволи", |ui| {
                ui::pill(ui, &format!("{granted} з {total}"), ui::Tone::Amber, false);
            });
            ui.label(ui::muted(
                "Щоб записувати ваш голос і голоси співрозмовників.",
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
                            ui::pill(ui, "Дозволено", ui::Tone::Green, false);
                        }
                        Access::NotAsked if asking => {
                            ui::pill(ui, "Чекаю відповіді…", ui::Tone::Grey, false);
                        }
                        Access::NotAsked => {
                            if ui::primary(ui, "Дозволити", None, None, false).clicked() {
                                request = Some(perm);
                            }
                        }
                        Access::Denied => {
                            if ui::outline(ui, "Увімкнути", None, false).clicked() {
                                permissions::open_settings(perm);
                            }
                        }
                    });
                });
            }
            if self.permissions.iter().any(|(_, a)| *a == Access::Denied) {
                ui.add_space(ui::S1);
                ui.label(ui::muted(
                    "Доступ було відхилено, тому macOS більше не питає. «Увімкнути» відкриє потрібний розділ налаштувань: увімкніть перемикач навпроти Meeting Recorder — програма побачить це одразу.",
                ));
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
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(32.0, 32.0)));
            ui.add_space(ui::S1);
            ui.label(
                RichText::new(APP_NAME)
                    .font(FontId::new(18.0, ui::bold()))
                    .color(ui::INK),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui::icon_button(ui, ui::Icon::Settings, "Налаштування").clicked() {
                    self.view = View::Settings;
                }
            });
        });
    }

    /// The main card: status, the timer and the one action.
    fn transport(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        ui::card(ui, |ui| {
            ui::card_header(ui, "Запис дзвінка", |ui| match &st {
                None => {
                    ui::pill(ui, "Готово", ui::Tone::Blue, false);
                }
                Some(s) => {
                    ui::pill(ui, "Іде запис", ui::Tone::Red, true);
                    if s.part > 1 {
                        ui::pill(ui, &format!("Частина {}", s.part), ui::Tone::Grey, false);
                    }
                }
            });
            ui.add_space(ui::S2);
            ui.label(ui::caption("Тривалість"));
            let elapsed = st.as_ref().map_or(0, |s| s.elapsed_ms / 1000);
            ui.label(
                RichText::new(hms(elapsed))
                    .font(FontId::new(42.0, ui::bold()))
                    .color(if st.is_some() { ui::INK } else { ui::INK_MUTED }),
            );
            ui.add_space(ui::S3);
            let clicked = if st.is_some() {
                ui::outline(ui, "Зупинити запис", Some((ui::Icon::Stop, ui::RED)), true)
            } else {
                ui::primary(ui, "Почати запис", Some(ui::Icon::Record), None, true)
            }
            .clicked();
            if clicked {
                if st.is_some() {
                    self.stop();
                } else {
                    self.start();
                }
            }
        });
    }

    fn channels(&self, ui: &mut egui::Ui) {
        let levels = self.status.as_ref().map(|s| s.levels);
        ui::card(ui, |ui| {
            ui::card_header(ui, "Канали", |_| {});
            ui.add_space(ui::S1);
            let rows = [
                (ui::Icon::Mic, "Ви", "мікрофон", levels.map(|l| l[0])),
                (
                    ui::Icon::Speaker,
                    "Співрозмовники",
                    "системний звук",
                    levels.map(|l| l[1]),
                ),
            ];
            for (i, (icon, name, source, peak)) in rows.into_iter().enumerate() {
                if i > 0 {
                    ui.add_space(ui::S2);
                }
                ui.horizontal(|ui| {
                    ui::icon_tile(ui, icon, ui::Tone::Blue);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = ui::S1 + 2.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).font(FontId::new(14.0, ui::medium())));
                            ui.label(ui::muted(source));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let level = match peak.map(ui::to_db) {
                                    // A real minus sign, not a hyphen.
                                    Some(db) if db > -60.0 => {
                                        format!("{db:.0} дБ").replace('-', "\u{2212}")
                                    }
                                    Some(_) => "тиша".to_string(),
                                    None => "—".to_string(),
                                };
                                ui.label(ui::muted(level));
                            });
                        });
                        ui::meter(ui, peak);
                    });
                });
            }
        });
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
        let mut open_pane = None;
        let mut dismiss = false;
        ui::card(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui::icon_tile(ui, icon, tone);
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if ui::close_button(ui, "Сховати").clicked() {
                        dismiss = true;
                    }
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.add_space(ui::S1);
                        ui.label(RichText::new(&m.text).color(ui::INK));
                        if let Some(pane) = m.settings_pane
                            && ui::link(ui, "Налаштування приватності").clicked()
                        {
                            open_pane = Some(pane);
                        }
                    });
                });
            });
        });
        if let Some(pane) = open_pane {
            open_privacy_settings(pane);
        }
        if dismiss {
            self.message = None;
        }
    }

    fn last_recording(&self, ui: &mut egui::Ui) {
        ui::card(ui, |ui| {
            let Some(last) = &self.last else {
                ui::card_header(ui, "Записи", |_| {});
                ui.label(ui::muted("Тут з'явиться останній запис."));
                ui.add_space(ui::S1);
                if ui::link(ui, "Відкрити папку записів").clicked() {
                    paths::open_in_file_manager(&self.settings.recordings_dir());
                }
                return;
            };
            ui::card_header(ui, "Останній запис", |ui| {
                ui::pill(ui, "Збережено", ui::Tone::Green, false);
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(when(last.started)).color(ui::INK));
                ui.label(
                    RichText::new(format!("· {}", duration_words(last.seconds)))
                        .color(ui::INK_MUTED),
                );
            });
            ui.add_space(ui::S2);
            ui.horizontal(|ui| {
                if ui::outline(ui, REVEAL, Some((ui::Icon::Folder, ui::BLUE)), false).clicked() {
                    paths::open_in_file_manager(&last.dir);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::link(ui, "Усі записи").clicked() {
                        paths::open_in_file_manager(&self.settings.recordings_dir());
                    }
                });
            });
        });
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        ui.add_space(ui::S4);
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

    // ── Settings screen ────────────────────────────────────────────────

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui::icon_button(ui, ui::Icon::Back, "Назад").clicked() {
                self.view = View::Main;
            }
            ui.add_space(ui::S1);
            ui.label(
                RichText::new("Налаштування")
                    .font(FontId::new(18.0, ui::bold()))
                    .color(ui::INK),
            );
        });
        ui.add_space(ui::S4);

        ui::card(ui, |ui| self.echo_settings(ui));
        ui.add_space(ui::S3);
        ui::card(ui, |ui| self.recordings_settings(ui));
        ui.add_space(ui::S3);
        ui::card(ui, |ui| {
            ui::card_header(ui, "Діагностика", |ui| {
                ui.label(ui::muted(format!("Версія {}", env!("CARGO_PKG_VERSION"))));
            });
            ui.label(ui::muted(
                "Журнали допоможуть розібратися, якщо із записом щось не так.",
            ));
            ui.add_space(ui::S1);
            if ui::link(ui, "Відкрити журнали").clicked() {
                paths::open_in_file_manager(&paths::log_dir());
            }
        });
    }

    fn echo_settings(&mut self, ui: &mut egui::Ui) {
        ui::card_header(ui, "Придушення луни", |_| {});
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
        ui.label(ui::muted(hint));
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
    }

    fn recordings_settings(&mut self, ui: &mut egui::Ui) {
        ui::card_header(ui, "Записи", |_| {});
        ui.add_space(ui::S1);
        ui.label(ui::caption("Папка"));
        ui.label(RichText::new(tilde(&self.settings.recordings_dir())).color(ui::INK));
        ui.add_space(ui::S2);
        ui.horizontal(|ui| {
            if ui::outline(ui, "Змінити…", None, false).clicked() {
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
            if ui::outline(ui, "Відкрити", Some((ui::Icon::Folder, ui::BLUE)), false).clicked()
            {
                paths::open_in_file_manager(&self.settings.recordings_dir());
            }
        });
        ui.add_space(ui::S3);
        ui::divider(ui);
        ui.add_space(ui::S3);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("Нагадувати про згоду").font(FontId::new(14.0, ui::medium())),
                );
                ui.label(ui::muted("Перед кожним записом"));
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
                modal_title(ui, "Перед записом");
                ui.label(RichText::new(CONSENT_REMINDER).color(ui::INK_SECONDARY));
                ui.add_space(ui::S3);
                ui.horizontal(|ui| {
                    ui::toggle(ui, &mut self.consent_dont_ask);
                    ui.label(RichText::new("Більше не нагадувати").color(ui::INK_SECONDARY));
                });
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary(ui, "Почати запис", Some(ui::Icon::Record), None, false)
                        .clicked()
                    {
                        start = true;
                    }
                    if ui::outline(ui, "Скасувати", None, false).clicked() {
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
                modal_title(ui, "Іде запис");
                ui.label(
                    RichText::new(
                        "Зупинити запис і закрити програму? Усе записане буде збережено.",
                    )
                    .color(ui::INK_SECONDARY),
                );
                ui.add_space(ui::S4);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::primary(ui, "Зупинити й закрити", None, None, false).clicked()
                    {
                        close = true;
                    }
                    if ui::outline(ui, "Продовжити", None, false).clicked() {
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

        ui::backdrop(ctx);
        egui::TopBottomPanel::bottom("footer")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(ui::CANVAS)
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 24,
                        top: 4,
                        bottom: 14,
                    }),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(CONSENT_REMINDER)
                            .color(ui::INK_MUTED)
                            .size(11.0),
                    );
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 14,
                right: 6,
                top: 14,
                bottom: 4,
            }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Room for the card shadows; the wider right side holds
                    // the floating scroll bar.
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

fn modal_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(FontId::new(17.0, ui::bold()))
            .color(ui::INK),
    );
    ui.add_space(ui::S1);
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
