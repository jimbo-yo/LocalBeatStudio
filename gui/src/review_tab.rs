//! Review tab: the Miscellaneous albums `localbeat enrich` wasn't sure about.
//! Each shows the proposed MusicBrainz release and why it wasn't written;
//! accept it, give another release, or leave the album as it is.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, RichText};
use localbeat_core::config::Settings;
use localbeat_core::enrich_review::{self, ReviewItem};

pub struct ReviewTab {
    items: Option<Vec<ReviewItem>>,
    search: String,
    /// Another release ID/URL typed per album.
    other: BTreeMap<String, String>,
    /// Albums being written (their buttons are off meanwhile).
    working: Vec<String>,
    tx: Sender<(String, Result<String, String>)>,
    rx: Receiver<(String, Result<String, String>)>,
}

impl ReviewTab {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        ReviewTab { items: None, search: String::new(), other: BTreeMap::new(), working: Vec::new(), tx, rx }
    }

    /// Finished accepts, as log lines.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok((rel, r)) = self.rx.try_recv() {
            self.working.retain(|w| w != &rel);
            match r {
                Ok(report) => {
                    if let Some(items) = &mut self.items {
                        items.retain(|i| i.rel != rel);
                    }
                    lines.push(format!("Review: {report}"));
                }
                Err(e) => lines.push(format!("Review: {rel}: {e}")),
            }
        }
        if !self.working.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }
        lines
    }

    fn accept(&mut self, s: &Settings, rel: &str, release: &str, ctx: &egui::Context) {
        self.working.push(rel.to_string());
        let (s, rel, release, tx, ctx) = (s.clone(), rel.to_string(), release.to_string(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = enrich_review::accept(&s, &rel, &release).map_err(|e| format!("{e:#}"));
            let _ = tx.send((rel, r));
            ctx.request_repaint();
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings) -> Vec<String> {
        let mut lines = Vec::new();
        if self.items.is_none() {
            let mut items = enrich_review::list(s);
            items.sort_by_key(|a| a.rel.to_lowercase());
            self.items = Some(items);
        }
        let total = self.items.as_ref().map_or(0, Vec::len);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{total} albums to review")).font(crate::theme::bold(18.0)));
            if crate::theme::button(ui, Some(crate::theme::Icon::Refresh), "Refresh", crate::theme::Kind::Secondary).clicked() {
                self.items = None;
            }
            ui.separator();
            ui.label("Search:");
            ui.text_edit_singleline(&mut self.search);
        });
        ui.weak(
            "Found on MusicBrainz but not written, because something didn't line up (shown under each). \
             Accept writes the tags and cover into the files (undo: localbeat enrich --undo NAME); \
             Leave as is stops future runs from asking.",
        );
        ui.separator();

        let needle = self.search.to_lowercase();
        let items: Vec<ReviewItem> = self
            .items
            .iter()
            .flatten()
            .filter(|i| needle.is_empty() || i.rel.to_lowercase().contains(&needle) || i.summary.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for item in items {
                let busy = self.working.contains(&item.rel);
                crate::theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&item.rel).font(crate::theme::medium(15.0)));
                        ui.weak(if item.by_sound { "· identified by sound" } else { "· found by name" });
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Proposed:");
                        ui.label(&item.summary);
                        if !item.release_id.is_empty() {
                            ui.hyperlink_to("MusicBrainz ↗", format!("https://musicbrainz.org/release/{}", item.release_id));
                        }
                    });
                    if !item.why.is_empty() {
                        ui.label(RichText::new(format!("Why not sure: {}", item.why)).color(crate::theme::WARN));
                    }
                    ui.horizontal(|ui| {
                        if busy {
                            ui.spinner();
                            ui.label("Writing…");
                            return;
                        }
                        if ui.add_enabled_ui(!item.release_id.is_empty(), |ui| crate::theme::button(ui, Some(crate::theme::Icon::Check), "Accept", crate::theme::Kind::Primary)).inner.clicked() {
                            self.accept(s, &item.rel, &item.release_id, &ctx);
                        }
                        if crate::theme::button(ui, Some(crate::theme::Icon::Cross), "Leave as is", crate::theme::Kind::Secondary).clicked() {
                            match enrich_review::reject(s, &item.rel) {
                                Ok(()) => {
                                    if let Some(items) = &mut self.items {
                                        items.retain(|i| i.rel != item.rel);
                                    }
                                    lines.push(format!("Review: {} left as it is", item.rel));
                                }
                                Err(e) => lines.push(format!("Review: {}: {e:#}", item.rel)),
                            }
                        }
                        if crate::theme::icon_button(ui, crate::theme::Icon::Folder, 32.0, "Open folder").clicked() {
                            open_folder(&s.misc_dir.join(&item.rel));
                        }
                        ui.separator();
                        let other = self.other.entry(item.rel.clone()).or_default();
                        ui.add(egui::TextEdit::singleline(other).hint_text("other release: MusicBrainz link or ID").desired_width(280.0));
                        let other = other.trim().to_string();
                        if ui.add_enabled(!other.is_empty(), egui::Button::new("Use this")).clicked() {
                            self.accept(s, &item.rel, &other, &ctx);
                        }
                    });
                });
                ui.add_space(6.0);
            }
        });
        lines
    }
}

fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(program).arg(dir).spawn();
}
