//! The Repairs page: every disc with tracks that couldn't be read cleanly
//! (from `<cache>/rejected`), what the last CUETools repair did, and what to
//! do next. Stays put when another disc goes in, unlike the drive's card.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};
use localbeat_core::config::Settings;
use localbeat_core::repair_disc::{self, DamagedDisc};

use crate::theme::{self, Icon, Kind};

pub struct RepairsTab {
    discs: Option<Vec<DamagedDisc>>,
    loading: Option<Receiver<Vec<DamagedDisc>>>,
    /// Repairs running from this page, by rejected folder.
    running: HashMap<PathBuf, Receiver<Result<String, String>>>,
    covers: HashMap<PathBuf, Option<egui::TextureHandle>>,
    /// "Really clear?" asked for this disc.
    confirm_clear: Option<PathBuf>,
    /// The page should look again (a repair finished somewhere).
    pub stale: bool,
}

impl RepairsTab {
    pub fn new() -> Self {
        RepairsTab { discs: None, loading: None, running: HashMap::new(), covers: HashMap::new(), confirm_clear: None, stale: true }
    }

    /// Discs with tracks still missing (for the sidebar count).
    pub fn count(&self) -> usize {
        self.discs.as_ref().map_or(0, |d| d.iter().filter(|x| !x.missing.is_empty()).count())
    }

    fn refresh(&mut self, s: &Settings, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (s, ctx) = (s.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(repair_disc::damaged_discs(&s));
            ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.stale = false;
    }

    /// Per frame, on any page: finished scans and repairs. Lines for the log.
    pub fn poll(&mut self, s: &Settings, ctx: &egui::Context) -> Vec<String> {
        let mut lines = Vec::new();
        if self.stale {
            self.refresh(s, ctx);
        }
        if let Some(d) = self.loading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.discs = Some(d);
            self.loading = None;
        }
        let done: Vec<PathBuf> = self.running.iter().filter_map(|(k, rx)| rx.try_recv().ok().map(|r| (k.clone(), r))).map(|(k, r)| {
            lines.push(match r {
                Ok(t) => format!("Repair: {t}"),
                Err(e) => format!("Repair failed: {e}"),
            });
            k
        }).collect();
        if !done.is_empty() {
            for k in done {
                self.running.remove(&k);
            }
            self.refresh(s, ctx);
        }
        lines
    }

    fn cover(&mut self, ctx: &egui::Context, album: &Path) -> Option<egui::TextureHandle> {
        self.covers
            .entry(album.to_path_buf())
            .or_insert_with(|| {
                let p = ["cover.jpg", "cover.png", "folder.jpg", "Front.jpg"].iter().map(|n| album.join(n)).find(|p| p.exists())?;
                let img = image::open(p).ok()?.thumbnail(256, 256).to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                Some(ctx.load_texture(format!("repair-cover-{}", album.display()), egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), Default::default()))
            })
            .clone()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings) -> Vec<String> {
        let mut lines = Vec::new();
        let p = theme::pal(ui);
        let Some(discs) = self.discs.clone() else {
            ui.label(RichText::new("Looking…").color(p.dim));
            return lines;
        };
        if discs.is_empty() {
            ui.add_space(30.0);
            ui.label(RichText::new("No damaged discs").font(theme::bold(22.0)).color(p.text));
            ui.label(RichText::new("When a track can't be read cleanly, its disc shows up here with what the repair could do.").color(p.dim));
            return lines;
        }
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for d in &discs {
                let running = self.running.contains_key(&d.rejected);
                let cover = self.cover(&ctx, &d.album);
                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 96.0), egui::Sense::hover());
                        match &cover {
                            Some(t) => {
                                egui::Image::new(t).corner_radius(8.0).paint_at(ui, rect);
                            }
                            None => theme::disc(ui.painter(), rect.center(), 44.0, None, None, ui.visuals().dark_mode),
                        }
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(format!("{}{}", d.title, d.disc_suffix)).font(theme::bold(18.0)).color(p.text));
                            ui.label(RichText::new(&d.artist).color(theme::prism_at(0.3)));
                            ui.add_space(4.0);
                            if d.missing.is_empty() {
                                theme::badge(ui, "COMPLETE", theme::OK);
                                ui.label(RichText::new("Every track is in the library now; what's left here are old reads.").color(p.dim));
                            } else {
                                let list: Vec<String> = d.missing.iter().map(|(t, r)| if *r > 0 { format!("{t} ({r} reads)") } else { format!("{t}") }).collect();
                                ui.horizontal_wrapped(|ui| {
                                    theme::badge(ui, &format!("{} MISSING", d.missing.len()), theme::WARN);
                                    ui.label(RichText::new(format!("Track{} {}", if d.missing.len() > 1 { "s" } else { "" }, list.join(", "))).color(p.text));
                                });
                            }
                            if let Some(last) = &d.last {
                                ui.add_space(6.0);
                                ui.label(RichText::new(format!("Last repair {} · {}", last.when, last.entry)).small().color(p.faint));
                                for (t, words) in &last.tracks {
                                    let done = words.starts_with("repaired");
                                    ui.horizontal(|ui| {
                                        let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                                        theme::paint_icon(ui.painter(), r, if done { Icon::Check } else { Icon::Cross }, if done { theme::OK } else { theme::ERR });
                                        ui.label(RichText::new(format!("Track {t}: {words}")).color(if done { p.text } else { p.dim }));
                                    });
                                }
                            } else if !d.missing.is_empty() {
                                ui.add_space(6.0);
                                ui.label(RichText::new("Not repaired yet.").color(p.dim));
                            }
                            if !d.missing.is_empty() {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(
                                        "Next: clean the disc (wipe straight out from the centre) and put it back in. Only the missing tracks are read; the new reads add to these and the repair runs again.",
                                    )
                                    .small()
                                    .color(p.dim),
                                );
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if !d.missing.is_empty() {
                                    let label = if running { "Repairing…" } else { "Try repair again" };
                                    let r = ui.add_enabled_ui(!running && d.cdtoc.is_some(), |ui| theme::button(ui, Some(Icon::Refresh), label, Kind::Primary));
                                    if r.inner.on_hover_text("Run the CUETools repair again on the saved reads (no disc needed)").clicked() {
                                        self.start_repair(s, d, &ctx);
                                        lines.push(format!("Repairing {} – {} again…", d.artist, d.title));
                                    }
                                } else {
                                    let asking = self.confirm_clear.as_ref() == Some(&d.rejected);
                                    let label = if asking { "Really clear?" } else { "Clear leftovers" };
                                    if theme::button(ui, Some(Icon::Cross), label, if asking { Kind::Danger } else { Kind::Secondary }).clicked() {
                                        if asking {
                                            match repair_disc::clear_leftovers(d) {
                                                Ok(()) => lines.push(format!("Cleared the old reads of {} – {}", d.artist, d.title)),
                                                Err(e) => lines.push(format!("Couldn't clear {}: {e:#}", d.rejected.display())),
                                            }
                                            self.confirm_clear = None;
                                            self.stale = true;
                                        } else {
                                            self.confirm_clear = Some(d.rejected.clone());
                                        }
                                    }
                                }
                                if theme::icon_button(ui, Icon::Folder, 32.0, "Show the saved reads").clicked() {
                                    let _ = std::process::Command::new(if cfg!(windows) { "explorer" } else { "xdg-open" }).arg(&d.rejected).spawn();
                                }
                            });
                        });
                    });
                });
                ui.add_space(8.0);
            }
        });
        lines
    }

    fn start_repair(&mut self, s: &Settings, d: &DamagedDisc, ctx: &egui::Context) {
        let Some(cdtoc) = d.cdtoc.clone() else { return };
        let (tx, rx) = channel();
        let (s, d2, ctx) = (s.clone(), d.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = repair_disc::repair_disc(&s, &d2.album, &cdtoc, Some(&d2.rejected), true).and_then(|r| {
                repair_disc::save_record(&r, &d2.album, &d2.rejected, &d2.disc_suffix)?;
                Ok(format!("{} – {}: {}", d2.artist, d2.title, r.summary()))
            });
            let _ = tx.send(r.map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.running.insert(d.rejected.clone(), rx);
    }
}
