//! The app's look: larger, more readable text, roomier spacing, rounded
//! widgets and one accent colour, in both dark and light mode (egui follows
//! the desktop's setting).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

/// The accent: selected tabs, links, progress, focus.
pub const ACCENT: Color32 = Color32::from_rgb(64, 132, 230);

struct Palette {
    panel: Color32,
    window: Color32,
    /// Text fields, list backgrounds.
    sunken: Color32,
    /// Cards, stripes.
    raised: Color32,
    widget: Color32,
    widget_hover: Color32,
    border: Color32,
    text: Color32,
}

const DARK: Palette = Palette {
    panel: Color32::from_rgb(30, 31, 36),
    window: Color32::from_rgb(38, 40, 46),
    sunken: Color32::from_rgb(22, 23, 27),
    raised: Color32::from_rgb(40, 42, 49),
    widget: Color32::from_rgb(50, 53, 61),
    widget_hover: Color32::from_rgb(62, 66, 77),
    border: Color32::from_rgb(58, 61, 70),
    text: Color32::from_rgb(226, 228, 233),
};

const LIGHT: Palette = Palette {
    panel: Color32::from_rgb(245, 246, 248),
    window: Color32::from_rgb(255, 255, 255),
    sunken: Color32::from_rgb(255, 255, 255),
    raised: Color32::from_rgb(236, 238, 242),
    widget: Color32::from_rgb(226, 229, 235),
    widget_hover: Color32::from_rgb(212, 217, 226),
    border: Color32::from_rgb(208, 212, 220),
    text: Color32::from_rgb(30, 32, 38),
};

pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 7.0);
        s.button_padding = Vec2::new(12.0, 5.0);
        s.interact_size.y = 26.0;
        s.indent = 22.0;
        s.window_margin = egui::Margin::same(14);

        let v = &mut style.visuals;
        let p = if v.dark_mode { &DARK } else { &LIGHT };
        v.panel_fill = p.panel;
        v.window_fill = p.window;
        v.extreme_bg_color = p.sunken;
        v.text_edit_bg_color = Some(p.sunken);
        v.faint_bg_color = p.raised;
        v.code_bg_color = p.raised;
        v.window_stroke = Stroke::new(1.0, p.border);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(if v.dark_mode { 0.55 } else { 0.35 });
        v.selection.stroke = Stroke::new(1.0, if v.dark_mode { Color32::WHITE } else { p.text });
        v.striped = true;
        v.indent_has_left_vline = false;

        let radius = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.panel;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        w.noninteractive.corner_radius = radius;
        for (wv, fill) in [(&mut w.inactive, p.widget), (&mut w.hovered, p.widget_hover), (&mut w.active, p.widget_hover), (&mut w.open, p.widget)] {
            wv.bg_fill = fill;
            wv.weak_bg_fill = fill;
            wv.corner_radius = radius;
        }
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.8));
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    });
}

/// A tab label in the header: larger than body text.
pub fn tab(text: &str) -> egui::RichText {
    egui::RichText::new(text).size(17.0)
}
