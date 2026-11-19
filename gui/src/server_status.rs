//! The "Server status" window (Settings → The LocalBeat server): the server
//! computer as fastfetch sees it, each disk's size and use, the space the
//! library takes, and how far the Opus copies have got. Any login may ask
//! (LocalBeat server's `getServerStatus`); fetched on a background thread.

use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Color32, RichText, vec2};
use localbeat_core::config::ServerPrefs;
use localbeat_core::subsonic::{ServerDisk, ServerStatus, Subsonic};

use crate::theme;

#[derive(Default)]
pub struct StatusWindow {
    open: bool,
    fetching: Option<Receiver<Result<ServerStatus, String>>>,
    last: Option<Result<ServerStatus, String>>,
}

impl StatusWindow {
    /// Opens the window and asks the server (again).
    pub fn open(&mut self, ctx: &egui::Context, server: &ServerPrefs) {
        self.open = true;
        self.fetch(ctx, server);
    }

    fn fetch(&mut self, ctx: &egui::Context, server: &ServerPrefs) {
        if self.fetching.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (server, ctx) = (server.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = match Subsonic::new(&server) {
                Some(c) => c.server_status().map_err(|e| format!("{e:#}")),
                None => Err("This computer has no music server login yet.".into()),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.fetching = Some(rx);
    }

    pub fn show(&mut self, ctx: &egui::Context, server: &ServerPrefs) {
        if let Some(rx) = &self.fetching
            && let Ok(r) = rx.try_recv()
        {
            self.last = Some(r);
            self.fetching = None;
        }
        if !self.open {
            return;
        }
        let mut open = self.open;
        let mut refresh = false;
        egui::Window::new("Server status")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .default_height(620.0)
            .show(ctx, |ui| {
                let p = theme::pal(ui);
                ui.horizontal(|ui| {
                    let line = match &self.last {
                        Some(Ok(s)) => format!("LocalBeat server {} · running {}", s.version, duration(s.uptime_secs)),
                        _ => server.url.clone(),
                    };
                    ui.label(RichText::new(line).color(p.dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.fetching.is_some() {
                            ui.spinner();
                        } else if theme::icon_button(ui, theme::Icon::Refresh, 22.0, "Ask again").clicked() {
                            refresh = true;
                        }
                    });
                });
                ui.add_space(6.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match &self.last {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking the server (adding up the library takes a moment)…");
                        });
                    }
                    Some(Err(e)) => {
                        ui.colored_label(theme::ERR, e);
                    }
                    Some(Ok(s)) => body(ui, s),
                });
            });
        self.open = open;
        if refresh {
            self.fetch(ctx, server);
        }
    }
}

fn body(ui: &mut egui::Ui, s: &ServerStatus) {
    let p = theme::pal(ui);

    theme::section_label(ui, "Disks");
    ui.add_space(4.0);
    if s.disks.is_empty() {
        ui.label(RichText::new("The server didn't say (not measured on this kind of server yet).").color(p.dim));
    }
    for d in &s.disks {
        disk(ui, d);
        ui.add_space(8.0);
    }

    ui.add_space(6.0);
    theme::section_label(ui, "Library");
    ui.add_space(4.0);
    ui.label(RichText::new(format!("{} songs in {} albums", thousands(s.songs), thousands(s.albums))).color(p.text));
    egui::Grid::new("server-folders").num_columns(3).spacing([18.0, 4.0]).show(ui, |ui| {
        for f in &s.folders {
            let name = match f.kind.as_str() {
                "removed" => format!("{} (taken out, can be put back)", f.name),
                _ => f.name.clone(),
            };
            ui.label(RichText::new(name).color(p.text)).on_hover_text(&f.path);
            ui.label(RichText::new(size(f.bytes)).monospace().color(p.text));
            ui.label(RichText::new(format!("{} files", thousands(f.files))).color(p.dim));
            ui.end_row();
        }
        let music: u64 = s.folders.iter().filter(|f| f.kind == "library").map(|f| f.bytes).sum();
        ui.label(RichText::new("Music in all").font(theme::medium(14.0)).color(p.text));
        ui.label(RichText::new(size(music)).monospace().strong().color(p.text));
        ui.label("");
        ui.end_row();
    });
    ui.add_space(4.0);
    if s.opus.needed > 0 {
        let line = if s.opus.made >= s.opus.needed {
            format!("Opus copies: all {} FLAC songs converted", thousands(s.opus.needed))
        } else {
            format!("Opus copies: {} of {} FLAC songs converted (the rest in the background)", thousands(s.opus.made), thousands(s.opus.needed))
        };
        ui.label(RichText::new(line).color(p.dim));
    }
    let scan = match (&s.last_scan, s.scanning) {
        (_, true) => "Scanning the library now".to_string(),
        (Some(t), false) => format!("Last scan {}", when(t)),
        (None, false) => "Not scanned yet".to_string(),
    };
    ui.label(RichText::new(scan).color(p.dim));

    ui.add_space(10.0);
    theme::section_label(ui, "Computer (fastfetch)");
    ui.add_space(4.0);
    match (&s.fastfetch, &s.fastfetch_error) {
        (Some(text), _) => {
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(RichText::new(text).monospace().color(p.text)).selectable(true).wrap_mode(egui::TextWrapMode::Extend));
            });
        }
        (None, Some(e)) => {
            ui.colored_label(theme::WARN, e);
            if e.contains("isn't installed") {
                ui.small("On the server: sudo dnf install fastfetch (host/setup.sh installs it).");
            }
        }
        (None, None) => {
            ui.label(RichText::new("The server didn't send it.").color(p.dim));
        }
    }
}

/// One disk: where, what's on it, a bar, and the numbers.
fn disk(ui: &mut egui::Ui, d: &ServerDisk) {
    let p = theme::pal(ui);
    let frac = if d.total > 0 { d.used as f32 / d.total as f32 } else { 0.0 };
    ui.horizontal(|ui| {
        ui.label(RichText::new(&d.mount).font(theme::medium(14.0)).color(p.text));
        ui.label(RichText::new(d.holds.join(", ")).color(p.dim));
    });
    let w = ui.available_width().min(560.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, p.line);
    let mut fill = rect;
    fill.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(if d.used > 0 { 4.0 } else { 0.0 }));
    let colour: Color32 = if frac > 0.9 { theme::ERR } else if frac > 0.75 { theme::WARN } else { theme::ACCENT };
    ui.painter().rect_filled(fill, 4.0, colour);
    ui.label(
        RichText::new(format!("{} used of {} ({:.0}%) · {} free", size(d.used), size(d.total), frac * 100.0, size(d.available)))
            .small()
            .color(p.dim),
    );
}

fn size(b: u64) -> String {
    let b = b as f64;
    if b >= 1e12 {
        format!("{:.2} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{b} bytes")
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m} min"),
        (0, _) => format!("{h} h {m} min"),
        _ => format!("{d} d {h} h"),
    }
}

/// "2026-11-19T10:00:05Z" → "2026-11-19 10:00 UTC".
fn when(iso: &str) -> String {
    match (iso.get(..10), iso.get(11..16)) {
        (Some(date), Some(time)) => format!("{date} {time} UTC"),
        _ => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(size(96_000_000_000), "96.0 GB");
        assert_eq!(size(1_000), "1 KB");
        assert_eq!(size(1_200_000_000_000), "1.20 TB");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(999), "999");
        assert_eq!(duration(3 * 86400 + 4 * 3600 + 60), "3 d 4 h");
        assert_eq!(duration(3700), "1 h 1 min");
        assert_eq!(when("2026-11-19T10:00:05Z"), "2026-11-19 10:00 UTC");
    }
}
