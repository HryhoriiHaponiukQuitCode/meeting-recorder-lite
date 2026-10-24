//! Look of the window: palette, fonts and the few custom controls.
//!
//! The direction is a small hardware recorder: a matte graphite (or paper
//! white) body, one red REC colour that only appears for recording, segmented
//! channel meters, and quiet cards with no visible chrome until it matters.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Response, RichText, Sense, Shadow, Stroke,
    Theme, Ui, Vec2, pos2, vec2,
};

/// Colours for one theme. Every surface is one hue; only lightness changes.
#[derive(Clone, Copy)]
pub struct Palette {
    pub canvas: Color32,
    pub card: Color32,
    pub inset: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub hairline: Color32,
    pub ink: Color32,
    pub ink_secondary: Color32,
    pub ink_muted: Color32,
    pub rec: Color32,
    pub signal: Color32,
    pub caution: Color32,
    pub brand: Color32,
    pub meter_off: Color32,
}

pub const DARK: Palette = Palette {
    canvas: Color32::from_rgb(0x15, 0x15, 0x17),
    card: Color32::from_rgb(0x1d, 0x1d, 0x20),
    inset: Color32::from_rgb(0x10, 0x10, 0x12),
    control: Color32::from_rgb(0x27, 0x27, 0x2b),
    control_hover: Color32::from_rgb(0x30, 0x30, 0x35),
    hairline: Color32::from_rgb(0x2a, 0x2a, 0x2e),
    ink: Color32::from_rgb(0xee, 0xee, 0xf0),
    ink_secondary: Color32::from_rgb(0xa3, 0xa3, 0xab),
    ink_muted: Color32::from_rgb(0x6e, 0x6e, 0x77),
    rec: Color32::from_rgb(0xe5, 0x48, 0x4d),
    signal: Color32::from_rgb(0x3d, 0xb8, 0x77),
    caution: Color32::from_rgb(0xf2, 0xa8, 0x2c),
    brand: Color32::from_rgb(0x39, 0x85, 0xf8),
    meter_off: Color32::from_rgb(0x26, 0x26, 0x2a),
};

pub const LIGHT: Palette = Palette {
    canvas: Color32::from_rgb(0xf3, 0xf3, 0xf5),
    card: Color32::from_rgb(0xff, 0xff, 0xff),
    inset: Color32::from_rgb(0xea, 0xea, 0xed),
    control: Color32::from_rgb(0xe8, 0xe8, 0xeb),
    control_hover: Color32::from_rgb(0xdf, 0xdf, 0xe3),
    hairline: Color32::from_rgb(0xe2, 0xe2, 0xe6),
    ink: Color32::from_rgb(0x18, 0x18, 0x1b),
    ink_secondary: Color32::from_rgb(0x55, 0x55, 0x5e),
    ink_muted: Color32::from_rgb(0x8c, 0x8c, 0x95),
    rec: Color32::from_rgb(0xdc, 0x3a, 0x40),
    signal: Color32::from_rgb(0x2a, 0x9d, 0x60),
    caution: Color32::from_rgb(0xc9, 0x82, 0x00),
    brand: Color32::from_rgb(0x2f, 0x78, 0xe8),
    meter_off: Color32::from_rgb(0xe4, 0xe4, 0xe8),
};

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

/// Spacing scale (4 px base).
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S6: f32 = 24.0;

/// Corner radii: controls < cards.
pub const R_CONTROL: u8 = 7;
pub const R_CARD: u8 = 12;

/// Named font families (fall back to the default UI font when the system
/// has no separate file for them).
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    for (theme, p) in [(Theme::Dark, DARK), (Theme::Light, LIGHT)] {
        ctx.set_visuals_of(theme, visuals(theme, p));
    }
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(S2, S2);
        style.spacing.button_padding = vec2(S3, 6.0);
        style.spacing.interact_size.y = 30.0;
        // A thin bar over the content instead of a gutter that clips cards.
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(11.5));
    });
}

fn visuals(theme: Theme, p: Palette) -> egui::Visuals {
    let mut v = match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    };
    v.panel_fill = p.canvas;
    v.window_fill = p.card;
    v.extreme_bg_color = p.inset;
    v.faint_bg_color = p.card;
    v.window_stroke = Stroke::new(1.0_f32, p.hairline);
    v.window_corner_radius = CornerRadius::same(R_CARD + 2);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(if theme == Theme::Dark { 120 } else { 40 }),
    };
    v.popup_shadow = v.window_shadow;
    v.selection.bg_fill = p.brand;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = p.brand;
    let radius = CornerRadius::same(R_CONTROL);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.ink);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.hairline);
    v.widgets.noninteractive.corner_radius = radius;
    for (w, fill) in [
        (&mut v.widgets.inactive, p.control),
        (&mut v.widgets.hovered, p.control_hover),
        (&mut v.widgets.active, p.control_hover),
        (&mut v.widgets.open, p.control),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = Stroke::new(1.0_f32, p.ink);
        w.corner_radius = radius;
        w.expansion = 0.0;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.hairline);
    v
}

/// Uses the platform UI font (San Francisco / Segoe UI) instead of the
/// bundled one, so the window reads as part of the system.
fn install_fonts(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    let files: &[(&str, &str)] = &[
        ("ui", "/System/Library/Fonts/SFNS.ttf"),
        ("mono", "/System/Library/Fonts/SFNSMono.ttf"),
    ];
    #[cfg(windows)]
    let files: &[(&str, &str)] = &[
        ("ui", "C:\\Windows\\Fonts\\segoeui.ttf"),
        ("semibold", "C:\\Windows\\Fonts\\seguisb.ttf"),
        ("mono", "C:\\Windows\\Fonts\\consola.ttf"),
    ];
    #[cfg(not(any(windows, target_os = "macos")))]
    let files: &[(&str, &str)] = &[];

    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = |name: &str| -> bool {
        let Some((_, path)) = files.iter().find(|(n, _)| *n == name) else {
            return false;
        };
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
                true
            }
            Err(e) => {
                tracing::debug!(path, error = %e, "system font not available");
                false
            }
        }
    };
    let ui = loaded("ui");
    let semi = loaded("semibold");
    let mono = loaded("mono");
    let proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if ui {
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".into());
    }
    if mono {
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "mono".into());
    }
    let mut semibold_chain = Vec::new();
    if semi {
        semibold_chain.push("semibold".to_owned());
    }
    if ui {
        semibold_chain.push("ui".to_owned());
    }
    semibold_chain.extend(proportional);
    fonts.families.insert(semibold(), semibold_chain);
    ctx.set_fonts(fonts);
}

/// Small uppercase label above a group, like the legend on a hardware panel.
pub fn eyebrow(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    ui.label(
        RichText::new(text.to_uppercase())
            .font(FontId::new(11.0, semibold()))
            .color(p.ink_muted)
            .extra_letter_spacing(0.6),
    );
}

/// A quiet surface one step above the canvas.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.hairline))
        .corner_radius(R_CARD)
        .inner_margin(S4)
        .show(ui, |ui| {
            // Leave room for the 1 px stroke on the right.
            ui.set_width(ui.available_width() - 1.0);
            add(ui)
        })
        .inner
}

/// The transport button: a red disc to start, a red square to stop.
pub fn record_button(ui: &mut Ui, recording: bool) -> Response {
    let p = palette(ui);
    let size = 64.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let painter = ui.painter_at(rect.expand(2.0));
    let c = rect.center();
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    // Housing ring.
    painter.circle_filled(
        c,
        size / 2.0,
        if hovered { p.control_hover } else { p.control },
    );
    painter.circle_stroke(c, size / 2.0 - 0.5, Stroke::new(1.0_f32, p.hairline));
    let scale = if pressed { 0.94 } else { 1.0 };
    let rec = if hovered {
        p.rec.gamma_multiply(1.08)
    } else {
        p.rec
    };
    if recording {
        let half = 11.0 * scale;
        painter.rect_filled(
            egui::Rect::from_center_size(c, Vec2::splat(half * 2.0)),
            CornerRadius::same(4),
            rec,
        );
    } else {
        painter.circle_filled(c, 21.0 * scale, rec);
    }
    if response.has_focus() {
        painter.circle_stroke(c, size / 2.0 + 1.5, Stroke::new(2.0_f32, p.brand));
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A segmented channel meter: 0 dBFS full, −60 dBFS empty. `None` = idle.
pub fn channel_meter(ui: &mut Ui, label: &str, hint: &str, peak: Option<f32>) {
    let p = palette(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(p.ink));
        ui.label(RichText::new(hint).color(p.ink_muted));
    });
    const SEGMENTS: usize = 28;
    let gap = 2.0;
    let height = 8.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    let seg_w = (rect.width() - gap * (SEGMENTS - 1) as f32) / SEGMENTS as f32;
    let db = peak.map(|v| if v > 0.0 { 20.0 * v.log10() } else { -120.0 });
    let lit = db.map_or(0, |db| {
        (((db + 60.0) / 60.0).clamp(0.0, 1.0) * SEGMENTS as f32).round() as usize
    });
    for i in 0..SEGMENTS {
        let x = rect.left() + i as f32 * (seg_w + gap);
        let seg = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(seg_w, height));
        let on = i < lit;
        let color = if !on {
            p.meter_off
        } else if i >= SEGMENTS - 2 {
            p.rec
        } else if i >= SEGMENTS - 6 {
            p.caution
        } else {
            p.signal
        };
        painter.rect_filled(seg, CornerRadius::same(2), color);
    }
}

/// An iOS-style switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = palette(ui);
    let size = vec2(38.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.15);
    let painter = ui.painter_at(rect);
    let track = if *on { p.signal } else { p.control_hover };
    painter.rect_filled(rect, CornerRadius::same(11), track);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    painter.circle_filled(pos2(x, rect.center().y), 8.5, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A segmented control; returns true when the selection changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let p = palette(ui);
    let height = 30.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(R_CONTROL + 1), p.inset);
    let w = (rect.width() - 4.0) / options.len() as f32;
    let mut changed = false;
    for (i, (option, label)) in options.iter().enumerate() {
        let seg = egui::Rect::from_min_size(
            pos2(rect.left() + 2.0 + i as f32 * w, rect.top() + 2.0),
            vec2(w, height - 4.0),
        );
        let response = ui.interact(seg, ui.id().with(("segment", i)), Sense::click());
        let selected = *value == *option;
        if selected {
            ui.painter()
                .rect_filled(seg, CornerRadius::same(R_CONTROL - 1), p.card);
            ui.painter().rect_stroke(
                seg,
                CornerRadius::same(R_CONTROL - 1),
                Stroke::new(1.0_f32, p.hairline),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter()
                .rect_filled(seg, CornerRadius::same(R_CONTROL - 1), p.control);
        }
        let color = if selected { p.ink } else { p.ink_secondary };
        ui.painter().text(
            seg.center(),
            egui::Align2::CENTER_CENTER,
            *label,
            FontId::new(
                13.0,
                if selected {
                    semibold()
                } else {
                    FontFamily::Proportional
                },
            ),
            color,
        );
        if response.clicked() && !selected {
            *value = *option;
            changed = true;
        }
    }
    changed
}

/// A text-only button in the brand colour, for secondary actions.
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    let r = ui.add(
        egui::Button::new(RichText::new(text).color(p.brand))
            .frame(false)
            .min_size(vec2(0.0, 24.0)),
    );
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A neutral filled button.
pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(vec2(0.0, 30.0)))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy)]
pub enum Icon {
    Close,
    Back,
    Settings,
}

/// A small round icon button. Icons are drawn, not taken from a font, so
/// they look the same with every system font.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    if response.hovered() {
        ui.painter().circle_filled(rect.center(), 14.0, p.control);
    }
    let color = if response.hovered() {
        p.ink
    } else {
        p.ink_secondary
    };
    let stroke = Stroke::new(1.6_f32, color);
    let painter = ui.painter();
    let c = rect.center();
    match icon {
        Icon::Close => {
            let d = 4.5;
            painter.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
            painter.line_segment([c + vec2(-d, d), c + vec2(d, -d)], stroke);
        }
        Icon::Back => {
            painter.line_segment([c + vec2(-6.0, 0.0), c + vec2(6.0, 0.0)], stroke);
            painter.line_segment([c + vec2(-6.0, 0.0), c + vec2(-1.5, -4.5)], stroke);
            painter.line_segment([c + vec2(-6.0, 0.0), c + vec2(-1.5, 4.5)], stroke);
        }
        Icon::Settings => {
            // Three sliders: a mixing-desk glyph rather than a gear.
            for (dy, knob) in [(-5.0, 2.5), (0.0, -3.0), (5.0, 1.0)] {
                painter.line_segment([c + vec2(-7.0, dy), c + vec2(7.0, dy)], stroke);
                painter.circle_filled(c + vec2(knob, dy), 2.4, color);
            }
        }
    }
    response
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small check mark.
pub fn check_mark(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    let c = rect.center();
    let stroke = Stroke::new(1.8_f32, color);
    ui.painter()
        .line_segment([c + vec2(-4.5, 0.5), c + vec2(-1.5, 3.5)], stroke);
    ui.painter()
        .line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], stroke);
}
