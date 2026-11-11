//! "Prism": ripshelf's design language. Ink-dark surfaces (a light variant
//! follows the desktop's setting) and one signature accent, the prism
//! gradient, like light on the data side of a CD. Also the shared pieces:
//! fonts, gradient painting, vector icons, cards, badges, buttons, progress.
//! See docs/ui-redesign.md.

// Pieces for the pages still to come (player, library) are here already.
#![allow(dead_code)]

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Mesh, Pos2, Rect, Response, RichText, Sense, Shape, Stroke,
    StrokeKind, TextStyle, Ui, Vec2, pos2, vec2,
};

/// The prism, left to right: cyan, blue, violet, magenta, amber.
pub const PRISM: [Color32; 5] = [
    Color32::from_rgb(62, 224, 255),
    Color32::from_rgb(91, 140, 255),
    Color32::from_rgb(160, 107, 255),
    Color32::from_rgb(255, 95, 168),
    Color32::from_rgb(255, 181, 71),
];

/// Solid accent (focus, links, selection): the prism's violet-blue middle.
pub const ACCENT: Color32 = Color32::from_rgb(132, 118, 255);
pub const OK: Color32 = Color32::from_rgb(61, 220, 151);
pub const WARN: Color32 = Color32::from_rgb(255, 181, 71);
pub const ERR: Color32 = Color32::from_rgb(255, 107, 107);

/// Surface and text colours for one mode.
pub struct Palette {
    /// Sidebar and window background.
    pub base: Color32,
    /// Page background.
    pub content: Color32,
    /// Cards.
    pub card: Color32,
    pub card_hover: Color32,
    /// Hairlines and card borders.
    pub line: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub faint: Color32,
}

pub const DARK: Palette = Palette {
    base: Color32::from_rgb(11, 12, 20),
    content: Color32::from_rgb(16, 18, 28),
    card: Color32::from_rgb(23, 26, 40),
    card_hover: Color32::from_rgb(31, 35, 53),
    line: Color32::from_rgb(42, 47, 69),
    text: Color32::from_rgb(236, 238, 247),
    dim: Color32::from_rgb(154, 160, 184),
    faint: Color32::from_rgb(100, 106, 133),
};

pub const LIGHT: Palette = Palette {
    base: Color32::from_rgb(238, 239, 246),
    content: Color32::from_rgb(248, 248, 252),
    card: Color32::from_rgb(255, 255, 255),
    card_hover: Color32::from_rgb(241, 241, 250),
    line: Color32::from_rgb(222, 224, 236),
    text: Color32::from_rgb(20, 22, 37),
    dim: Color32::from_rgb(92, 97, 122),
    faint: Color32::from_rgb(146, 150, 172),
};

pub fn pal(ui: &Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// Colour at `t` (0..1) along the prism.
pub fn prism_at(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0) * (PRISM.len() - 1) as f32;
    let i = (t.floor() as usize).min(PRISM.len() - 2);
    lerp(PRISM[i], PRISM[i + 1], t - i as f32)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()), m(a.a(), b.a()))
}

fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

// ---------------------------------------------------------------- fonts

/// Font families besides the default proportional one.
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// Loads the system's UI font in three weights (Noto Sans on Fedora/Ubuntu,
/// Segoe UI on Windows); egui's built-in font stays as the fallback for
/// symbols and anything missing.
fn load_fonts(ctx: &egui::Context) {
    let candidates: [(&str, &[&str]); 3] = [
        (
            "regular",
            &[
                "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "C:\\Windows\\Fonts\\segoeui.ttf",
            ],
        ),
        (
            "medium",
            &[
                "/usr/share/fonts/google-noto/NotoSans-Medium.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
                "C:\\Windows\\Fonts\\seguisb.ttf",
            ],
        ),
        (
            "bold",
            &[
                "/usr/share/fonts/google-noto/NotoSans-Bold.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
                "C:\\Windows\\Fonts\\segoeuib.ttf",
            ],
        ),
    ];
    let mut fonts = egui::FontDefinitions::default();
    let fallback: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut loaded = std::collections::HashMap::new();
    for (name, paths) in candidates {
        if let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) {
            fonts.font_data.insert(name.into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            loaded.insert(name, true);
        }
    }
    let family = |first: &str| -> Vec<String> {
        let mut v = Vec::new();
        if loaded.contains_key(first) {
            v.push(first.to_string());
        } else if loaded.contains_key("regular") {
            v.push("regular".to_string());
        }
        v.extend(fallback.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family("regular"));
    fonts.families.insert(FontFamily::Name("medium".into()), family("medium"));
    fonts.families.insert(FontFamily::Name("bold".into()), family("bold"));
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------- style

pub fn apply(ctx: &egui::Context) {
    load_fonts(ctx);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, medium(14.0)),
            (TextStyle::Heading, bold(22.0)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = vec2(8.0, 7.0);
        s.button_padding = vec2(12.0, 6.0);
        s.interact_size.y = 28.0;
        s.indent = 22.0;
        s.window_margin = egui::Margin::same(16);
        s.scroll.bar_width = 8.0;
        s.scroll.floating = true;

        let v = &mut style.visuals;
        let p = if v.dark_mode { &DARK } else { &LIGHT };
        v.panel_fill = p.content;
        v.window_fill = p.card;
        v.extreme_bg_color = if v.dark_mode { p.base } else { p.card };
        v.text_edit_bg_color = Some(if v.dark_mode { p.base } else { p.card });
        v.faint_bg_color = p.card;
        v.code_bg_color = p.card;
        v.window_stroke = Stroke::new(1.0, p.line);
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = egui::Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(110) };
        v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = with_alpha(ACCENT, if v.dark_mode { 0.45 } else { 0.30 });
        v.selection.stroke = Stroke::new(1.0, p.text);
        v.warn_fg_color = WARN;
        v.error_fg_color = ERR;
        v.striped = false;
        v.indent_has_left_vline = false;
        v.override_text_color = None;

        let radius = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.content;
        w.noninteractive.weak_bg_fill = p.content;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        w.noninteractive.corner_radius = radius;
        for (wv, fill) in [(&mut w.inactive, p.card), (&mut w.hovered, p.card_hover), (&mut w.active, p.card_hover), (&mut w.open, p.card)] {
            wv.bg_fill = fill;
            wv.weak_bg_fill = fill;
            wv.corner_radius = radius;
            wv.expansion = 0.0;
        }
        w.inactive.bg_stroke = Stroke::new(1.0, p.line);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.hovered.bg_stroke = Stroke::new(1.0, with_alpha(ACCENT, 0.7));
        w.hovered.fg_stroke = Stroke::new(1.5, p.text);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, p.text);
    });
}

// ---------------------------------------------------------------- painting

/// A rounded rectangle filled with the prism, left to right. `alpha`
/// scales its opacity.
pub fn prism_rect(painter: &egui::Painter, rect: Rect, radius: f32, alpha: f32) {
    gradient_rect(painter, rect, radius, |t| with_alpha(prism_at(t), alpha));
}

/// A rounded rectangle whose colour runs left to right as `color(t)`.
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, radius: f32, color: impl Fn(f32) -> Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let mut pts = Vec::new();
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (c, start) in corners {
        for i in 0..=6 {
            let a = (start + 90.0 * i as f32 / 6.0).to_radians();
            pts.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    let at = |p: Pos2| color((p.x - rect.left()) / rect.width());
    let mut mesh = Mesh::default();
    let center = rect.center();
    mesh.colored_vertex(center, at(center));
    for p in &pts {
        mesh.colored_vertex(*p, at(*p));
    }
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    painter.add(Shape::mesh(mesh));
}

/// A soft prism glow fading downwards (behind page headers).
pub fn glow(painter: &egui::Painter, rect: Rect, strength: f32) {
    let cols = 24;
    let mut mesh = Mesh::default();
    for i in 0..=cols {
        let t = i as f32 / cols as f32;
        let x = rect.left() + rect.width() * t;
        // Brighter towards the left, like light catching the disc's edge.
        let a = strength * (1.0 - 0.6 * t);
        mesh.colored_vertex(pos2(x, rect.top()), with_alpha(prism_at(t), a));
        mesh.colored_vertex(pos2(x, rect.bottom()), Color32::TRANSPARENT);
    }
    for i in 0..cols as u32 {
        let (a, b, c, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        mesh.add_triangle(a, b, c);
        mesh.add_triangle(b, d, c);
    }
    painter.add(Shape::mesh(mesh));
}

/// Text with each letter coloured along the prism.
pub fn prism_text(ui: &Ui, text: &str, font: FontId) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    let n = text.chars().count().max(2) as f32 - 1.0;
    for (i, ch) in text.chars().enumerate() {
        let fmt = egui::TextFormat { font_id: font.clone(), color: prism_at(i as f32 / n), ..Default::default() };
        job.append(&ch.to_string(), 0.0, fmt);
    }
    ui.painter().layout_job(job)
}

// ---------------------------------------------------------------- icons

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    Eject,
    Next,
    Prev,
    Disc,
    Phone,
    Tag,
    Log,
    Bell,
    Check,
    Cross,
    Folder,
    Refresh,
}

/// Draws `icon` centred in `rect`, in `color`.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) / 2.0; // half size
    let w = (s * 0.16).max(1.4);
    let stroke = Stroke::new(w, color);
    let tri = |pts: Vec<Pos2>| Shape::convex_polygon(pts, color, Stroke::NONE);
    match icon {
        Icon::Play => {
            painter.add(tri(vec![c + vec2(-0.45 * s, -0.62 * s), c + vec2(0.62 * s, 0.0), c + vec2(-0.45 * s, 0.62 * s)]));
        }
        Icon::Pause => {
            for dx in [-0.32, 0.32] {
                let r = Rect::from_center_size(c + vec2(dx * s, 0.0), vec2(0.28 * s, 1.2 * s));
                painter.rect_filled(r, 1.5, color);
            }
        }
        Icon::Stop => {
            painter.rect_filled(Rect::from_center_size(c, Vec2::splat(1.05 * s)), 2.0, color);
        }
        Icon::Eject => {
            painter.add(tri(vec![c + vec2(0.0, -0.62 * s), c + vec2(0.62 * s, 0.12 * s), c + vec2(-0.62 * s, 0.12 * s)]));
            painter.rect_filled(Rect::from_min_max(c + vec2(-0.62 * s, 0.34 * s), c + vec2(0.62 * s, 0.56 * s)), 1.0, color);
        }
        Icon::Next | Icon::Prev => {
            let d = if icon == Icon::Next { 1.0 } else { -1.0 };
            for off in [-0.42, 0.18] {
                let x0 = off * d;
                painter.add(tri(vec![
                    c + vec2((x0 - 0.0) * s, -0.5 * s),
                    c + vec2((x0 + 0.5 * d) * s, 0.0),
                    c + vec2(x0 * s, 0.5 * s),
                ]));
            }
            let bar = 0.72 * d;
            painter.rect_filled(Rect::from_center_size(c + vec2(bar * s, 0.0), vec2(0.14 * s, 1.0 * s)), 1.0, color);
        }
        Icon::Disc => {
            painter.circle_stroke(c, 0.82 * s, stroke);
            painter.circle_stroke(c, 0.26 * s, stroke);
            painter.line_segment([c + vec2(0.34 * s, -0.55 * s), c + vec2(0.55 * s, -0.34 * s)], Stroke::new(w * 0.8, color));
        }
        Icon::Phone => {
            let r = Rect::from_center_size(c, vec2(0.95 * s, 1.6 * s));
            painter.rect_stroke(r, 3.0, stroke, StrokeKind::Middle);
            painter.line_segment([pos2(c.x - 0.15 * s, r.bottom() - 0.22 * s), pos2(c.x + 0.15 * s, r.bottom() - 0.22 * s)], stroke);
        }
        Icon::Tag => {
            let pts = vec![
                c + vec2(-0.8 * s, -0.8 * s),
                c + vec2(0.05 * s, -0.8 * s),
                c + vec2(0.85 * s, 0.0),
                c + vec2(0.05 * s, 0.8 * s),
                c + vec2(-0.8 * s, 0.8 * s),
            ];
            let mut closed = pts.clone();
            closed.push(pts[0]);
            painter.add(Shape::line(closed, stroke));
            painter.circle_filled(c + vec2(-0.4 * s, 0.0), 0.13 * s, color);
        }
        Icon::Log => {
            for (i, len) in [1.0f32, 0.7, 0.85].iter().enumerate() {
                let y = c.y + (i as f32 - 1.0) * 0.5 * s;
                painter.line_segment([pos2(c.x - 0.75 * s, y), pos2(c.x + (len * 1.5 - 0.75) * s, y)], stroke);
            }
        }
        Icon::Bell => {
            let top = c + vec2(0.0, -0.55 * s);
            painter.circle_stroke(top + vec2(0.0, 0.42 * s), 0.48 * s, stroke);
            painter.rect_filled(Rect::from_min_max(c + vec2(-0.48 * s, -0.13 * s), c + vec2(0.48 * s, 0.45 * s)), 0.0, color);
            painter.line_segment([c + vec2(-0.72 * s, 0.48 * s), c + vec2(0.72 * s, 0.48 * s)], stroke);
            painter.circle_filled(c + vec2(0.0, 0.72 * s), 0.14 * s, color);
        }
        Icon::Check => {
            painter.add(Shape::line(vec![c + vec2(-0.6 * s, 0.02 * s), c + vec2(-0.18 * s, 0.45 * s), c + vec2(0.62 * s, -0.48 * s)], Stroke::new(w * 1.2, color)));
        }
        Icon::Cross => {
            let k = 0.5 * s;
            painter.line_segment([c + vec2(-k, -k), c + vec2(k, k)], Stroke::new(w * 1.2, color));
            painter.line_segment([c + vec2(-k, k), c + vec2(k, -k)], Stroke::new(w * 1.2, color));
        }
        Icon::Folder => {
            let pts = vec![
                c + vec2(-0.8 * s, -0.6 * s),
                c + vec2(-0.2 * s, -0.6 * s),
                c + vec2(0.0, -0.4 * s),
                c + vec2(0.8 * s, -0.4 * s),
                c + vec2(0.8 * s, 0.65 * s),
                c + vec2(-0.8 * s, 0.65 * s),
                c + vec2(-0.8 * s, -0.6 * s),
            ];
            painter.add(Shape::line(pts, stroke));
        }
        Icon::Refresh => {
            let r = 0.62 * s;
            let pts: Vec<Pos2> = (0..=20).map(|i| {
                let a = (-60.0f32 + 280.0 * i as f32 / 20.0).to_radians();
                c + vec2(a.cos(), a.sin()) * r
            }).collect();
            let end = *pts.last().unwrap();
            painter.add(Shape::line(pts, stroke));
            painter.add(tri(vec![end + vec2(-0.32 * s, -0.05 * s), end + vec2(0.18 * s, -0.3 * s), end + vec2(0.12 * s, 0.25 * s)]));
        }
    }
}

// ---------------------------------------------------------------- widgets

/// Small capitals heading for a group (sidebar sections, page parts).
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(medium(11.0)).color(pal(ui).faint).extra_letter_spacing(1.2));
}

/// A page title with an optional subtitle, over a soft prism glow.
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str, right: impl FnOnce(&mut Ui)) {
    // The glow covers the whole panel (its clip rect), not just the margins.
    let area = ui.clip_rect();
    let glow_rect = Rect::from_min_size(area.min, vec2(area.width(), 150.0));
    glow(ui.painter(), glow_rect, if ui.visuals().dark_mode { 0.16 } else { 0.10 });
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(bold(28.0)).color(pal(ui).text));
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).color(pal(ui).dim));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
    ui.add_space(10.0);
}

/// A card: raised surface with a hairline border.
pub fn card(ui: &Ui) -> egui::Frame {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(14))
}

/// A small coloured label ("FLAC", "CD", "DRM").
pub fn badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = medium(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = galley.size() + vec2(12.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, 5.0, with_alpha(color, 0.16));
    ui.painter().rect_stroke(rect, 5.0, Stroke::new(1.0, with_alpha(color, 0.45)), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

/// A round icon-only button.
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let p = pal(ui);
    let enabled = ui.is_enabled();
    if resp.hovered() && enabled {
        ui.painter().circle_filled(rect.center(), size / 2.0, p.card_hover);
    }
    let color = if !enabled { p.faint } else if resp.hovered() { p.text } else { p.dim };
    paint_icon(ui.painter(), rect.shrink(size * 0.25), icon, color);
    if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Prism-filled: the page's main action.
    Primary,
    /// Card-coloured.
    Secondary,
    /// Outlined in the error colour.
    Danger,
}

/// A pill button with an optional leading icon.
pub fn button(ui: &mut Ui, icon: Option<Icon>, text: &str, kind: Kind) -> Response {
    let font = medium(14.0);
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let icon_w = if icon.is_some() { 20.0 } else { 0.0 };
    let size = vec2(galley.size().x + icon_w + 28.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let enabled = ui.is_enabled();
    let hovered = resp.hovered() && enabled;
    let painter = ui.painter();
    let fg = match kind {
        Kind::Primary => Color32::WHITE,
        Kind::Danger => ERR,
        Kind::Secondary => p.text,
    };
    let fg = if enabled { fg } else { p.faint };
    match kind {
        Kind::Primary if enabled => {
            prism_rect(painter, rect, 16.0, if hovered { 1.0 } else { 0.88 });
            // Darken a little so white text stays readable on the light end.
            painter.rect_filled(rect, 16.0, Color32::from_black_alpha(if hovered { 30 } else { 50 }));
        }
        Kind::Danger => {
            painter.rect_filled(rect, 16.0, if hovered { with_alpha(ERR, 0.14) } else { p.card });
            painter.rect_stroke(rect, 16.0, Stroke::new(1.0, with_alpha(ERR, if enabled { 0.6 } else { 0.25 })), StrokeKind::Inside);
        }
        _ => {
            painter.rect_filled(rect, 16.0, if hovered { p.card_hover } else { p.card });
            painter.rect_stroke(rect, 16.0, Stroke::new(1.0, if hovered { with_alpha(ACCENT, 0.7) } else { p.line }), StrokeKind::Inside);
        }
    }
    let mut x = rect.left() + 14.0;
    if let Some(i) = icon {
        let ir = Rect::from_center_size(pos2(x + 8.0, rect.center().y), Vec2::splat(15.0));
        paint_icon(painter, ir, i, fg);
        x += icon_w;
    }
    let galley = ui.painter().layout_no_wrap(text.to_string(), medium(14.0), fg);
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(pos2(x, y), galley, fg);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A slim progress bar filled with the prism, optional text to its right.
pub fn progress(ui: &mut Ui, fraction: f32, width: f32, text: Option<&str>) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
        let p = pal(ui);
        ui.painter().rect_filled(rect, 3.0, p.line);
        let f = fraction.clamp(0.0, 1.0);
        if f > 0.0 {
            let fill = Rect::from_min_size(rect.min, vec2((rect.width() * f).max(6.0), rect.height()));
            // The gradient spans the whole bar, so the colour says how far along.
            gradient_rect(ui.painter(), fill, 3.0, |t| prism_at(t * f));
        }
        if let Some(t) = text {
            ui.label(RichText::new(t).color(p.dim).small());
        }
    });
}

/// One row of the sidebar: icon, label and an optional status on the right.
/// The selected row gets a card background and a prism marker.
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool, status: Option<(&str, Color32)>) -> Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click());
    let p = pal(ui);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 9.0, p.card_hover);
        let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 8.0), vec2(3.0, rect.height() - 16.0));
        // Vertical prism marker.
        let mut mesh = Mesh::default();
        for (i, y) in [bar.top(), bar.bottom()].iter().enumerate() {
            let c = prism_at(i as f32 * 0.6);
            mesh.colored_vertex(pos2(bar.left(), *y), c);
            mesh.colored_vertex(pos2(bar.right(), *y), c);
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 3, 2);
        painter.add(Shape::mesh(mesh));
    } else if resp.hovered() {
        painter.rect_filled(rect, 9.0, p.card);
    }
    let icon_color = if selected { prism_at(0.3) } else { p.dim };
    paint_icon(painter, Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(17.0)), icon, icon_color);
    painter.text(
        pos2(rect.left() + 42.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        if selected { medium(14.5) } else { FontId::proportional(14.5) },
        if selected { p.text } else { p.dim },
    );
    if let Some((text, color)) = status {
        let right = rect.right() - 10.0;
        if text.is_empty() {
            painter.circle_filled(pos2(right - 4.0, rect.center().y), 4.0, color);
        } else {
            painter.text(pos2(right, rect.center().y), Align2::RIGHT_CENTER, text, medium(12.0), color);
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A switch with a label, for on/off settings.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.0), p.text);
    let size = vec2(36.0 + 8.0 + galley.size().x, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let track = Rect::from_min_size(pos2(rect.left(), rect.center().y - 10.0), vec2(36.0, 20.0));
    let painter = ui.painter();
    painter.rect_filled(track, 10.0, p.line);
    if t > 0.0 {
        gradient_rect(painter, track, 10.0, |x| with_alpha(prism_at(x * 0.6), t));
    }
    let knob = pos2(track.left() + 10.0 + 16.0 * t, track.center().y);
    painter.circle_filled(knob, 7.5, Color32::WHITE);
    painter.galley(pos2(track.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
    resp
}
