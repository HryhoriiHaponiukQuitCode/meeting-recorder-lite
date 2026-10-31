//! Look of the window, matched to the QuitCode web app: a pale blue-grey
//! canvas washed with blue at the top, white cards with a soft shadow, bold
//! navy figures, one blue primary action and pastel status pills.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Response,
    RichText, Sense, Shadow, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

pub const CANVAS: Color32 = Color32::from_rgb(0xf4, 0xf6, 0xfa);
/// The blue wash at the top of the canvas.
const CANVAS_TINT: Color32 = Color32::from_rgb(0xe6, 0xef, 0xfd);
pub const CARD: Color32 = Color32::WHITE;
pub const HAIRLINE: Color32 = Color32::from_rgb(0xe8, 0xec, 0xf2);
const BORDER: Color32 = Color32::from_rgb(0xdc, 0xe2, 0xea);
const BORDER_HOVER: Color32 = Color32::from_rgb(0xc4, 0xcd, 0xd9);
pub const HOVER: Color32 = Color32::from_rgb(0xf7, 0xf9, 0xfc);
const TRACK: Color32 = Color32::from_rgb(0xeb, 0xef, 0xf4);
pub const INK: Color32 = Color32::from_rgb(0x14, 0x1c, 0x2b);
pub const INK_SECONDARY: Color32 = Color32::from_rgb(0x4b, 0x56, 0x68);
pub const INK_MUTED: Color32 = Color32::from_rgb(0x8a, 0x94, 0xa6);
pub const BLUE: Color32 = Color32::from_rgb(0x17, 0x74, 0xf0);
const BLUE_HOVER: Color32 = Color32::from_rgb(0x0e, 0x63, 0xd6);
pub const RED: Color32 = Color32::from_rgb(0xe0, 0x3b, 0x3b);
const AMBER: Color32 = Color32::from_rgb(0xf3, 0xa7, 0x12);

/// Spacing scale (4 px base).
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;

/// Corner radii: pills < controls < cards.
const R_PILL: u8 = 8;
const R_CONTROL: u8 = 10;
pub const R_CARD: u8 = 16;

/// Pastel tones of the status pills and icon tiles: (background, text).
#[derive(Clone, Copy)]
pub enum Tone {
    Blue,
    Amber,
    Green,
    Red,
    Grey,
}

impl Tone {
    fn colors(self) -> (Color32, Color32) {
        match self {
            Tone::Blue => (
                Color32::from_rgb(0xe6, 0xf0, 0xff),
                Color32::from_rgb(0x17, 0x68, 0xd8),
            ),
            Tone::Amber => (
                Color32::from_rgb(0xff, 0xf2, 0xd0),
                Color32::from_rgb(0xa1, 0x62, 0x00),
            ),
            Tone::Green => (
                Color32::from_rgb(0xdc, 0xf5, 0xe6),
                Color32::from_rgb(0x13, 0x80, 0x46),
            ),
            Tone::Red => (
                Color32::from_rgb(0xfd, 0xe5, 0xe5),
                Color32::from_rgb(0xc4, 0x28, 0x28),
            ),
            Tone::Grey => (
                Color32::from_rgb(0xee, 0xf1, 0xf5),
                Color32::from_rgb(0x4b, 0x56, 0x68),
            ),
        }
    }
}

/// Medium weight: titles, names, buttons.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Bold weight: the big figures.
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    // One light look, the same as the web app, whatever the system theme.
    ctx.set_theme(egui::Theme::Light);
    ctx.set_visuals_of(egui::Theme::Light, visuals());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(S2, S2);
        style.spacing.button_padding = vec2(S4, 8.0);
        style.spacing.interact_size.y = 34.0;
        // A thin bar over the content instead of a gutter that clips cards.
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style.interaction.selectable_labels = false;
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::new(14.0, medium()));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(12.0));
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    v.panel_fill = CANVAS;
    v.window_fill = CARD;
    v.extreme_bg_color = TRACK;
    v.faint_bg_color = CANVAS;
    v.window_stroke = Stroke::new(1.0_f32, HAIRLINE);
    v.window_corner_radius = CornerRadius::same(R_CARD);
    v.window_shadow = Shadow {
        offset: [0, 16],
        blur: 40,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(16, 36, 72, 46),
    };
    v.popup_shadow = v.window_shadow;
    v.selection.bg_fill = BLUE.gamma_multiply(0.25);
    v.selection.stroke = Stroke::new(1.0_f32, BLUE);
    v.hyperlink_color = BLUE;
    let radius = CornerRadius::same(R_CONTROL);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, INK);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, HAIRLINE);
    v.widgets.noninteractive.corner_radius = radius;
    for (w, fill, border) in [
        (&mut v.widgets.inactive, CARD, BORDER),
        (&mut v.widgets.hovered, HOVER, BORDER_HOVER),
        (&mut v.widgets.active, TRACK, BORDER_HOVER),
        (&mut v.widgets.open, CARD, BORDER),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0_f32, border);
        w.fg_stroke = Stroke::new(1.0_f32, INK);
        w.corner_radius = radius;
        w.expansion = 0.0;
    }
    v
}

/// A built-in system family with real weights: Helvetica Neue on macOS
/// (San Francisco ships only as a variable font, which renders at one
/// weight), Segoe UI on Windows. Each file is read once and shared by its
/// faces.
fn install_fonts(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    let faces: &[(&str, &str, u32)] = &[
        ("regular", "/System/Library/Fonts/HelveticaNeue.ttc", 0),
        ("medium", "/System/Library/Fonts/HelveticaNeue.ttc", 10),
        ("bold", "/System/Library/Fonts/HelveticaNeue.ttc", 1),
    ];
    #[cfg(windows)]
    let faces: &[(&str, &str, u32)] = &[
        ("regular", "C:\\Windows\\Fonts\\segoeui.ttf", 0),
        ("medium", "C:\\Windows\\Fonts\\seguisb.ttf", 0),
        ("bold", "C:\\Windows\\Fonts\\segoeuib.ttf", 0),
    ];
    #[cfg(not(any(windows, target_os = "macos")))]
    let faces: &[(&str, &str, u32)] = &[];

    let mut fonts = egui::FontDefinitions::default();
    let mut files: Vec<(&str, &'static [u8])> = Vec::new();
    let mut loaded = Vec::new();
    for &(name, path, index) in faces {
        let bytes = match files.iter().find(|(p, _)| *p == path) {
            Some((_, b)) => *b,
            None => match std::fs::read(path) {
                // Fonts live as long as the app.
                Ok(b) => {
                    let b: &'static [u8] = Box::leak(b.into_boxed_slice());
                    files.push((path, b));
                    b
                }
                Err(e) => {
                    tracing::debug!(path, error = %e, "system font not available");
                    continue;
                }
            },
        };
        let mut data = egui::FontData::from_static(bytes);
        data.index = index;
        fonts.font_data.insert(name.to_owned(), data.into());
        loaded.push(name);
    }
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let chain = |first: &str| -> Vec<String> {
        [first, "regular"]
            .into_iter()
            .filter(|n| loaded.contains(n))
            .map(str::to_owned)
            .chain(fallback.iter().cloned())
            .collect()
    };
    fonts
        .families
        .insert(FontFamily::Proportional, chain("regular"));
    fonts.families.insert(medium(), chain("medium"));
    fonts.families.insert(bold(), chain("bold"));
    ctx.set_fonts(fonts);
}

/// The canvas: pale grey with a blue wash fading out from the top.
pub fn backdrop(ctx: &egui::Context) {
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(rect, 0, CANVAS);
    let fade = Rect::from_min_size(rect.min, vec2(rect.width(), 260.0_f32.min(rect.height())));
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(fade.left_top(), CANVAS_TINT);
    mesh.colored_vertex(fade.right_top(), CANVAS_TINT);
    mesh.colored_vertex(fade.right_bottom(), CANVAS);
    mesh.colored_vertex(fade.left_bottom(), CANVAS);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, HAIRLINE))
        .corner_radius(R_CARD)
        .shadow(Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(16, 36, 72, 16),
        })
        .inner_margin(Margin::same(18))
}

/// A white card with a hairline and a soft shadow.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame()
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A card title on the left and something (usually a pill) on the right.
pub fn card_header(ui: &mut Ui, title: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .font(FontId::new(15.5, medium()))
                .color(INK),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
}

/// Secondary text: hints, units, captions.
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(INK_MUTED).size(12.5)
}

/// The grey label above a value ("Duration").
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).color(INK_SECONDARY).size(13.0)
}

/// A pastel status pill, optionally with a dot before the text.
pub fn pill(ui: &mut Ui, text: &str, tone: Tone, dot: bool) -> Response {
    let (bg, fg) = tone.colors();
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::new(12.5, medium()), fg);
    let dot_w = if dot { 12.0 } else { 0.0 };
    let size = vec2(galley.size().x + dot_w + 20.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, R_PILL, bg);
    let x = rect.left() + 10.0;
    if dot {
        painter.circle_filled(pos2(x + 3.5, rect.center().y), 3.5, fg);
    }
    painter.galley(
        pos2(x + dot_w, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    response
}

#[derive(Clone, Copy)]
pub enum Icon {
    Play,
    Pause,
    Wave,
    Stop,
    Arrow,
    Close,
    Settings,
    Folder,
    /// A list of recordings: lines with a dot before each.
    List,
    Mic,
    Speaker,
    Alert,
    Info,
}

/// Icons are drawn, not taken from a font, so they look the same with every
/// system font.
fn draw_icon(painter: &egui::Painter, icon: Icon, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.7_f32, color);
    let line = |a: Vec2, b: Vec2| painter.line_segment([c + a, c + b], stroke);
    match icon {
        Icon::Play => {
            painter.add(Shape::convex_polygon(
                vec![
                    c + vec2(-3.5, -6.0),
                    c + vec2(6.0, 0.0),
                    c + vec2(-3.5, 6.0),
                ],
                color,
                Stroke::new(1.0_f32, color),
            ));
        }
        Icon::Wave => {
            for (dx, h) in [(-6.0, 4.0), (-2.0, 10.0), (2.0, 14.0), (6.0, 7.0)] {
                line(vec2(dx, -h / 2.0), vec2(dx, h / 2.0));
            }
        }
        Icon::Pause => {
            for dx in [-3.0, 3.0] {
                painter.rect_filled(
                    Rect::from_center_size(c + vec2(dx, 0.0), vec2(3.5, 11.0)),
                    1,
                    color,
                );
            }
        }
        Icon::Stop => {
            painter.rect_filled(Rect::from_center_size(c, Vec2::splat(10.0)), 2, color);
        }
        Icon::Arrow => {
            line(vec2(-5.5, 0.0), vec2(5.5, 0.0));
            line(vec2(5.5, 0.0), vec2(1.5, -4.0));
            line(vec2(5.5, 0.0), vec2(1.5, 4.0));
        }
        Icon::Close => {
            line(vec2(-4.5, -4.5), vec2(4.5, 4.5));
            line(vec2(-4.5, 4.5), vec2(4.5, -4.5));
        }
        Icon::Settings => {
            // Three sliders: a mixing-desk glyph rather than a gear.
            for (dy, knob) in [(-5.0, 2.5), (0.0, -3.0), (5.0, 1.0)] {
                line(vec2(-7.0, dy), vec2(7.0, dy));
                painter.circle_filled(c + vec2(knob, dy), 2.4, color);
            }
        }
        Icon::List => {
            for dy in [-5.0, 0.0, 5.0] {
                painter.circle_filled(c + vec2(-6.0, dy), 1.6, color);
                line(vec2(-2.5, dy), vec2(7.0, dy));
            }
        }
        Icon::Folder => {
            let body = Rect::from_center_size(c + vec2(0.0, 1.0), vec2(15.0, 11.0));
            painter.rect_stroke(body, 2, stroke, StrokeKind::Middle);
            let tab = body.left_top();
            painter.add(Shape::line(
                vec![
                    tab + vec2(0.0, 1.0),
                    tab + vec2(1.5, -2.0),
                    tab + vec2(5.5, -2.0),
                    tab + vec2(7.0, 0.0),
                ],
                stroke,
            ));
        }
        Icon::Mic => {
            painter.rect_stroke(
                Rect::from_min_max(c + vec2(-3.0, -7.5), c + vec2(3.0, 2.0)),
                3,
                stroke,
                StrokeKind::Middle,
            );
            arc(painter, c + vec2(0.0, -1.5), 6.0, 0.0, 180.0, stroke);
            line(vec2(0.0, 4.5), vec2(0.0, 7.5));
        }
        Icon::Speaker => {
            painter.add(Shape::closed_line(
                vec![
                    c + vec2(-7.5, -2.5),
                    c + vec2(-4.5, -2.5),
                    c + vec2(-0.5, -6.0),
                    c + vec2(-0.5, 6.0),
                    c + vec2(-4.5, 2.5),
                    c + vec2(-7.5, 2.5),
                ],
                stroke,
            ));
            arc(painter, c + vec2(0.5, 0.0), 3.5, -50.0, 50.0, stroke);
            arc(painter, c + vec2(0.5, 0.0), 7.0, -50.0, 50.0, stroke);
        }
        Icon::Alert => {
            line(vec2(0.0, -5.5), vec2(0.0, 1.5));
            painter.circle_filled(c + vec2(0.0, 5.0), 1.2, color);
        }
        Icon::Info => {
            painter.circle_filled(c + vec2(0.0, -5.0), 1.2, color);
            line(vec2(0.0, -1.5), vec2(0.0, 5.5));
        }
    }
}

/// An arc from `from` to `to` degrees (0 = right, 90 = down).
fn arc(painter: &egui::Painter, c: Pos2, r: f32, from: f32, to: f32, stroke: Stroke) {
    let points = (0..=16)
        .map(|i| {
            let a = (from + (to - from) * i as f32 / 16.0).to_radians();
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    painter.add(Shape::line(points, stroke));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    /// The full width of the card.
    Full,
    Regular,
    /// For card headers.
    Small,
}

impl Size {
    fn of(full_width: bool) -> Self {
        if full_width {
            Size::Full
        } else {
            Size::Regular
        }
    }
}

struct ButtonLook {
    fill: Color32,
    hover: Color32,
    border: Option<(Color32, Color32)>,
    text: Color32,
    icon: Color32,
}

/// A button with an optional icon before and after the text.
fn icon_button_with_text(
    ui: &mut Ui,
    text: &str,
    leading: Option<Icon>,
    trailing: Option<Icon>,
    look: ButtonLook,
    size: Size,
) -> Response {
    const ICON: f32 = 14.0;
    let gap = if size == Size::Small { 6.0 } else { 8.0 };
    let font = FontId::new(if size == Size::Small { 13.0 } else { 14.0 }, medium());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, look.text);
    let icons = [leading, trailing].iter().flatten().count() as f32;
    let content = galley.size().x + icons * (ICON + gap);
    let size = match size {
        Size::Full => vec2(ui.available_width(), 44.0),
        Size::Regular => vec2(content + 32.0, 36.0),
        Size::Small => vec2(content + 24.0, 30.0),
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        R_CONTROL,
        if hovered { look.hover } else { look.fill },
    );
    if let Some((border, border_hover)) = look.border {
        let color = if hovered { border_hover } else { border };
        painter.rect_stroke(
            rect,
            R_CONTROL,
            Stroke::new(1.0_f32, color),
            StrokeKind::Inside,
        );
    }
    let nudge = if pressed { 1.0 } else { 0.0 };
    let cy = rect.center().y + nudge;
    let mut x = rect.center().x - content / 2.0;
    if let Some(icon) = leading {
        draw_icon(painter, icon, pos2(x + ICON / 2.0, cy), look.icon);
        x += ICON + gap;
    }
    let text_w = galley.size().x;
    painter.galley(pos2(x, cy - galley.size().y / 2.0), galley, look.text);
    x += text_w + gap;
    if let Some(icon) = trailing {
        draw_icon(painter, icon, pos2(x + ICON / 2.0, cy), look.icon);
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            R_CONTROL + 2,
            Stroke::new(2.0_f32, BLUE.gamma_multiply(0.45)),
            StrokeKind::Outside,
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The blue primary action.
pub fn primary(
    ui: &mut Ui,
    text: &str,
    leading: Option<Icon>,
    trailing: Option<Icon>,
    full_width: bool,
) -> Response {
    let look = ButtonLook {
        fill: BLUE,
        hover: BLUE_HOVER,
        border: None,
        text: Color32::WHITE,
        icon: Color32::WHITE,
    };
    icon_button_with_text(ui, text, leading, trailing, look, Size::of(full_width))
}

/// The white outlined action, with an optional coloured icon.
pub fn outline(
    ui: &mut Ui,
    text: &str,
    icon: Option<(Icon, Color32)>,
    full_width: bool,
) -> Response {
    let look = ButtonLook {
        fill: CARD,
        hover: HOVER,
        border: Some((BORDER, BORDER_HOVER)),
        text: INK,
        icon: icon.map_or(INK, |(_, c)| c),
    };
    icon_button_with_text(
        ui,
        text,
        icon.map(|(i, _)| i),
        None,
        look,
        Size::of(full_width),
    )
}

/// A compact outlined action for card headers.
pub fn outline_small(ui: &mut Ui, text: &str, icon: Option<(Icon, Color32)>) -> Response {
    let look = ButtonLook {
        fill: CARD,
        hover: HOVER,
        border: Some((BORDER, BORDER_HOVER)),
        text: INK,
        icon: icon.map_or(INK, |(_, c)| c),
    };
    icon_button_with_text(ui, text, icon.map(|(i, _)| i), None, look, Size::Small)
}

/// A square outlined icon button (settings, back).
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
    let hovered = response.hovered();
    let painter = ui.painter();
    painter.rect_filled(rect, R_CONTROL, if hovered { HOVER } else { CARD });
    painter.rect_stroke(
        rect,
        R_CONTROL,
        Stroke::new(1.0_f32, if hovered { BORDER_HOVER } else { BORDER }),
        StrokeKind::Inside,
    );
    draw_icon(
        painter,
        icon,
        rect.center(),
        if hovered { INK } else { INK_SECONDARY },
    );
    response
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A navigation rail entry (as Material's NavigationRail): the icon on a
/// pill that fills with a blue tint when selected, the label below.
pub fn rail_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool, badge: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 60.0), Sense::click());
    let hovered = response.hovered();
    let pill = Rect::from_center_size(pos2(rect.center().x, rect.top() + 18.0), vec2(52.0, 32.0));
    let painter = ui.painter();
    if selected {
        painter.rect_filled(pill, 16, BLUE.gamma_multiply(0.13));
    } else if hovered {
        painter.rect_filled(pill, 16, TRACK);
    }
    let ink = if selected {
        BLUE
    } else if hovered {
        INK
    } else {
        INK_SECONDARY
    };
    draw_icon(painter, icon, pill.center(), ink);
    if badge {
        // A red dot: something is going on there (recording).
        let at = pill.right_top() + vec2(-12.0, 7.0);
        painter.circle_filled(at, 4.5, CARD);
        painter.circle_filled(at, 3.5, RED);
    }
    let font = if selected {
        FontId::new(12.0, medium())
    } else {
        FontId::proportional(12.0)
    };
    painter.text(
        pos2(rect.center().x, pill.bottom() + 12.0),
        Align2::CENTER_CENTER,
        label,
        font,
        if selected { INK } else { INK_SECONDARY },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small round close button for dismissible cards.
pub fn close_button(ui: &mut Ui, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter().circle_filled(rect.center(), 13.0, TRACK);
    }
    draw_icon(
        ui.painter(),
        Icon::Close,
        rect.center(),
        if hovered { INK } else { INK_MUTED },
    );
    response
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A blue text link with a trailing arrow ("All recordings →").
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::new(14.0, medium()), BLUE);
    let size = vec2(galley.size().x + 22.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if response.hovered() { BLUE_HOVER } else { BLUE };
    let text_w = galley.size().x;
    let painter = ui.painter();
    painter.galley(
        pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    let shift = if response.hovered() { 2.0 } else { 0.0 };
    draw_icon(
        painter,
        Icon::Arrow,
        pos2(rect.left() + text_w + 13.0 + shift, rect.center().y),
        color,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A round pastel tile with an icon, like the avatars in the web app's lists.
pub fn icon_tile(ui: &mut Ui, icon: Icon, tone: Tone) {
    let (bg, fg) = tone.colors();
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 18.0, bg);
    draw_icon(ui.painter(), icon, rect.center(), fg);
}

/// The level to show, as a fraction of the −60…0 dBFS scale. Levels
/// arrive every 50 ms; this eases towards them every frame, rising quickly
/// and falling slowly like a VU meter, so the bar and the figure move
/// smoothly. `None` = idle.
pub fn smooth_level(ui: &Ui, id_salt: &str, peak: Option<f32>) -> f32 {
    let target = ((peak.map_or(-120.0, to_db) + 60.0) / 60.0).clamp(0.0, 1.0);
    let id = egui::Id::new(("level", id_salt));
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let shown = ui.data_mut(|d| {
        let v = d.get_temp_mut_or(id, target);
        *v = ease_level(*v, target, dt);
        *v
    });
    // Finish falling to zero after the recording stops.
    if peak.is_none() && shown > 0.001 {
        ui.ctx().request_repaint();
    }
    shown
}

/// One step of the level easing: rises with a 40 ms time constant, falls
/// with 250 ms, independent of the frame rate.
fn ease_level(current: f32, target: f32, dt: f32) -> f32 {
    let tau = if target > current { 0.04 } else { 0.25 };
    current + (target - current) * (1.0 - (-dt / tau).exp())
}

/// A thin level bar; `level` comes from [`smooth_level`].
pub fn meter(ui: &mut Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3, TRACK);
    if level < 0.005 {
        return;
    }
    let color = if level > 59.0 / 60.0 {
        RED
    } else if level > 0.9 {
        AMBER
    } else {
        BLUE
    };
    let mut fill = rect;
    fill.set_width((rect.width() * level).max(6.0));
    painter.rect_filled(fill, 3, color);
}

/// A thin blue progress bar, `fraction` in 0..=1.
pub fn progress(ui: &mut Ui, fraction: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3, TRACK);
    let mut fill = rect;
    fill.set_width((rect.width() * fraction.clamp(0.0, 1.0)).max(6.0));
    painter.rect_filled(fill, 3, BLUE);
}

pub fn to_db(peak: f32) -> f32 {
    if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        -120.0
    }
}

/// A segmented control; returns true when the selection changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let height = 38.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter().rect_filled(rect, R_CONTROL + 1, TRACK);
    let w = (rect.width() - 6.0) / options.len() as f32;
    let mut changed = false;
    for (i, (option, label)) in options.iter().enumerate() {
        let seg = Rect::from_min_size(
            pos2(rect.left() + 3.0 + i as f32 * w, rect.top() + 3.0),
            vec2(w, height - 6.0),
        );
        let response = ui.interact(seg, ui.id().with(("segment", i)), Sense::click());
        let selected = *value == *option;
        if selected {
            let painter = ui.painter();
            painter.add(
                Shadow {
                    offset: [0, 1],
                    blur: 4,
                    spread: 0,
                    color: Color32::from_rgba_unmultiplied(16, 36, 72, 24),
                }
                .as_shape(seg, R_CONTROL - 2),
            );
            painter.rect_filled(seg, R_CONTROL - 2, CARD);
        }
        let font = if selected {
            FontId::new(13.5, medium())
        } else {
            FontId::proportional(13.5)
        };
        let color = if selected || response.hovered() {
            INK
        } else {
            INK_SECONDARY
        };
        ui.painter()
            .text(seg.center(), Align2::CENTER_CENTER, *label, font, color);
        if response.clicked() && !selected {
            *value = *option;
            changed = true;
        }
        response.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    changed
}

/// An iOS-style switch in the brand blue.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(40.0, 24.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.15);
    let off = Color32::from_rgb(0xd3, 0xd9, 0xe2);
    let painter = ui.painter();
    painter.rect_filled(rect, 12, egui::lerp(egui::Rgba::from(off)..=BLUE.into(), t));
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    painter.circle_filled(pos2(x, rect.center().y), 9.5, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A hairline between groups inside a card.
pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0_f32, HAIRLINE),
    );
}

/// The card frame for modal dialogs.
pub fn modal_frame() -> egui::Frame {
    card_frame().inner_margin(Margin::same(22)).shadow(Shadow {
        offset: [0, 16],
        blur: 40,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(16, 36, 72, 46),
    })
}

#[cfg(test)]
mod tests {
    use super::ease_level;

    fn run(mut v: f32, target: f32, seconds: f32, fps: f32) -> f32 {
        for _ in 0..(seconds * fps) as usize {
            v = ease_level(v, target, 1.0 / fps);
        }
        v
    }

    #[test]
    fn level_rises_fast_falls_slowly_at_any_frame_rate() {
        // Rising: most of the way within 0.1 s.
        assert!(run(0.0, 1.0, 0.1, 30.0) > 0.9);
        // Falling: still visible after 0.1 s, gone after 1.5 s.
        let falling = run(1.0, 0.0, 0.1, 30.0);
        assert!(falling > 0.6, "fell to {falling}");
        assert!(run(1.0, 0.0, 1.5, 30.0) < 0.01);
        // The same motion at 30 and 60 frames per second.
        assert!((run(1.0, 0.0, 0.5, 30.0) - run(1.0, 0.0, 0.5, 60.0)).abs() < 0.01);
    }
}
