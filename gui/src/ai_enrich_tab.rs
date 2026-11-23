//! AI Genres & Moods page: export unenriched albums as a self-contained prompt
//! for any AI of the user's choice, import the AI's JSON response into a
//! dedicated review screen, edit or flag questionable albums for a focused
//! follow-up AI discussion (with full tracklists and user notes), and write
//! approved GENRE, MOOD and LOCALBEAT_AI_ENRICHED tags into the audio files
//! (locally and on the LocalBeat server).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Color32, RichText};
use localbeat_core::ai_enrich::{
    self, Confidence, ExportOptions, PendingStore, Proposal, Taxonomy,
};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::library::Library;
use localbeat_engine::retag::{self, ApplyReport};

use crate::theme::{self, Icon, Kind};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Attention,
    Flagged,
    High,
}

pub struct AiEnrichTab {
    taxonomy: Option<Taxonomy>,
    pending: Option<PendingStore>,
    batch_size: usize,
    include_enriched: bool,
    filter: Filter,
    search: String,
    show_import: bool,
    import_text: String,
    import_file: String,
    /// Status banner message (message, is_error).
    banner: Option<(String, bool)>,
    /// Background apply job.
    working: HashSet<String>,
    rx: Receiver<Result<ApplyReport, String>>,
    tx: Sender<Result<ApplyReport, String>>,
    /// True after a successful apply so `main.rs` triggers a library rescan.
    pub rescan_wanted: bool,
}

impl AiEnrichTab {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        AiEnrichTab {
            taxonomy: None,
            pending: None,
            batch_size: ai_enrich::DEFAULT_BATCH_SIZE,
            include_enriched: false,
            filter: Filter::All,
            search: String::new(),
            show_import: false,
            import_text: String::new(),
            import_file: String::new(),
            banner: None,
            working: HashSet::new(),
            rx,
            tx,
            rescan_wanted: false,
        }
    }

    fn ensure_loaded(&mut self, s: &Settings) {
        if self.taxonomy.is_none() {
            self.taxonomy = Some(Taxonomy::load(&s.config_dir));
        }
        if self.pending.is_none() {
            self.pending = Some(PendingStore::load(&s.config_dir));
        }
    }

    /// Pending count for the sidebar badge.
    pub fn pending_count(&mut self, s: &Settings) -> usize {
        self.ensure_loaded(s);
        self.pending.as_ref().map_or(0, |p| p.items.len())
    }

    /// Polls background apply jobs and returns activity log lines.
    pub fn poll(&mut self, s: &Settings, ctx: &egui::Context) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(res) = self.rx.try_recv() {
            self.working.clear();
            match res {
                Ok(report) => {
                    if let Some(pending) = &mut self.pending {
                        for id in &report.albums_applied {
                            pending.items.remove(id);
                        }
                        let _ = pending.save(&s.config_dir);
                    }
                    let summary = report.summary();
                    lines.push(format!("AI Enrich: {summary}"));
                    for n in &report.notes {
                        lines.push(format!("AI Enrich note: {n}"));
                    }
                    if !report.albums_applied.is_empty() {
                        self.rescan_wanted = true;
                    }
                    let banner_msg = if report.notes.is_empty() {
                        summary
                    } else {
                        format!("{summary} ({})", report.notes.join("; "))
                    };
                    self.banner = Some((banner_msg, report.albums_applied.is_empty()));
                }
                Err(e) => {
                    lines.push(format!("AI Enrich failed: {e}"));
                    self.banner = Some((e, true));
                }
            }
        }
        if !self.working.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        lines
    }

    fn start_apply(
        &mut self,
        s: &Settings,
        prefs: &Prefs,
        lib: Arc<Library>,
        proposals: Vec<Proposal>,
        version: String,
        ctx: egui::Context,
    ) {
        if proposals.is_empty() {
            return;
        }
        for p in &proposals {
            self.working.insert(p.album_id.clone());
        }
        let s = s.clone();
        let sp = prefs.server.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = retag::apply_proposals(&s, &sp, &lib, &proposals, &version).map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        s: &Settings,
        prefs: &Prefs,
        lib: Option<Arc<Library>>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        self.ensure_loaded(s);
        let p = theme::pal(ui);
        let Some(lib) = lib else {
            ui.label(RichText::new("Scanning library…").color(p.dim));
            return lines;
        };

        let tax = self.taxonomy.as_ref().unwrap().clone();
        let st = ai_enrich::stats(&lib, &tax, self.pending.as_ref().unwrap());
        let busy = !self.working.is_empty();

        // ---- Status banner ----
        if let Some((msg, is_err)) = self.banner.clone() {
            let color = if is_err { theme::ERR } else { theme::OK };
            egui::Frame::new()
                .fill(p.card)
                .stroke(egui::Stroke::new(1.0, color))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&msg).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.banner = None;
                            }
                        });
                    });
                });
            ui.add_space(8.0);
        }

        // ---- Step 1: Progress + Export / Import card ----
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} of {} albums AI-enriched ({})",
                        st.enriched_current, st.total_albums, tax.version
                    ))
                    .font(theme::bold(15.0)),
                );
                ui.weak(format!(
                    "· {} remaining to export · {} pending review{}",
                    st.remaining_to_export,
                    st.pending_review,
                    if st.flagged > 0 { format!(" · {} flagged", st.flagged) } else { String::new() }
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Edit taxonomy file")
                        .on_hover_text(Taxonomy::file(&s.config_dir).display().to_string())
                        .clicked()
                    {
                        open_path(&Taxonomy::file(&s.config_dir));
                    }
                    if ui.small_button("Reload taxonomy").clicked() {
                        self.taxonomy = Some(Taxonomy::load(&s.config_dir));
                        let new_tax = self.taxonomy.as_ref().unwrap();
                        if let Some(pending) = &mut self.pending {
                            for prop in pending.items.values_mut() {
                                prop.revalidate(new_tax);
                            }
                            let _ = pending.save(&s.config_dir);
                        }
                        self.banner = Some(("Reloaded ai-taxonomy.json".into(), false));
                    }
                });
            });
            let frac = if st.total_albums == 0 {
                0.0
            } else {
                st.enriched_current as f32 / st.total_albums as f32
            };
            ui.add_space(4.0);
            theme::progress(ui, frac, ui.available_width().min(420.0), None);
            ui.add_space(10.0);

            // Export + Import buttons row
            ui.horizontal_wrapped(|ui| {
                ui.label("Batch size:");
                egui::ComboBox::from_id_salt("ai-batch-size")
                    .width(64.0)
                    .selected_text(self.batch_size.to_string())
                    .show_ui(ui, |ui| {
                        for n in [25, 50, 60, 100] {
                            ui.selectable_value(&mut self.batch_size, n, n.to_string());
                        }
                    });
                ui.checkbox(&mut self.include_enriched, "Include already enriched")
                    .on_hover_text("Re-export albums that already have the current AI-enriched tag");
                ui.separator();

                let opts = ExportOptions {
                    batch_size: self.batch_size,
                    include_enriched: self.include_enriched,
                    include_pending: false,
                };
                if theme::button(ui, Some(Icon::Tag), "Copy prompt for AI", Kind::Primary)
                    .on_hover_text("Copy the next batch of unenriched albums + taxonomy to your clipboard to paste into any AI")
                    .clicked()
                {
                    match ai_enrich::export_batch_prompt(&lib, &tax, self.pending.as_ref().unwrap(), &opts) {
                        Some(batch) => {
                            ui.ctx().copy_text(batch.prompt);
                            let msg = format!(
                                "Copied prompt for {} album(s) to clipboard ({} left after this batch). Paste it into your AI, then click Import.",
                                batch.album_ids.len(),
                                batch.remaining_after
                            );
                            lines.push(format!("AI Enrich: {msg}"));
                            self.banner = Some((msg, false));
                            self.show_import = true;
                        }
                        None => {
                            self.banner = Some(("No unenriched albums left to export.".into(), false));
                        }
                    }
                }
                if theme::button(ui, Some(Icon::Folder), "Save prompt (.md)", Kind::Secondary)
                    .on_hover_text("Save the next batch as a Markdown file to upload to an AI")
                    .clicked()
                {
                    match ai_enrich::export_batch_prompt(&lib, &tax, self.pending.as_ref().unwrap(), &opts) {
                        Some(batch) => match ai_enrich::save_prompt_file(&s.config_dir, "batch", &batch.prompt) {
                            Ok(path) => {
                                open_path(path.parent().unwrap_or(&path));
                                let msg = format!("Saved {} album(s) to {}", batch.album_ids.len(), path.display());
                                lines.push(format!("AI Enrich: {msg}"));
                                self.banner = Some((msg, false));
                                self.show_import = true;
                            }
                            Err(e) => self.banner = Some((format!("Couldn't save prompt: {e:#}"), true)),
                        },
                        None => self.banner = Some(("No unenriched albums left to export.".into(), false)),
                    }
                }
                let import_label = if self.show_import { "Hide import box" } else { "Import AI reply" };
                if theme::button(ui, Some(Icon::Plus), import_label, Kind::Secondary).clicked() {
                    self.show_import = !self.show_import;
                }
            });

            if self.show_import {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(RichText::new("Paste the AI's reply below (prose and ```json fences are fine):").color(p.dim));
                egui::ScrollArea::vertical().id_salt("ai-import-scroll").max_height(140.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.import_text)
                            .hint_text("Paste JSON or the whole AI response here…")
                            .desired_width(f32::INFINITY)
                            .desired_rows(5)
                            .code_editor(),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let can_paste = !self.import_text.trim().is_empty();
                    if ui
                        .add_enabled_ui(can_paste, |ui| theme::button(ui, Some(Icon::Check), "Import pasted reply", Kind::Primary))
                        .inner
                        .clicked()
                    {
                        let text = self.import_text.clone();
                        self.do_import(s, &lib, &tax, &text, &mut lines);
                    }
                    ui.separator();
                    ui.label("Or file:");
                    ui.add(egui::TextEdit::singleline(&mut self.import_file).hint_text("path/to/reply.json").desired_width(240.0));
                    let has_file = !self.import_file.trim().is_empty();
                    if ui.add_enabled(has_file, egui::Button::new("Load file")).clicked() {
                        let path = self.import_file.trim().trim_matches('"').to_string();
                        match std::fs::read_to_string(&path) {
                            Ok(text) => self.do_import(s, &lib, &tax, &text, &mut lines),
                            Err(e) => self.banner = Some((format!("Couldn't read {path}: {e}"), true)),
                        }
                    }
                });
            }
        });

        ui.add_space(10.0);

        // ---- Step 2: Review toolbar ----
        let (all_n, attn_n, flag_n, high_n) = {
            let items = &self.pending.as_ref().unwrap().items;
            (
                items.len(),
                items.values().filter(|x| x.needs_attention()).count(),
                items.values().filter(|x| x.flagged).count(),
                items.values().filter(|x| x.confidence == Confidence::High && !x.flagged && x.warnings.is_empty()).count(),
            )
        };

        ui.horizontal_wrapped(|ui| {
            for (f, label) in [
                (Filter::All, format!("All ({all_n})")),
                (Filter::Attention, format!("Needs attention ({attn_n})")),
                (Filter::Flagged, format!("Flagged ({flag_n})")),
                (Filter::High, format!("High confidence ({high_n})")),
            ] {
                if ui.selectable_label(self.filter == f, label).clicked() {
                    self.filter = f;
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Filter review list…").desired_width(170.0));
        });

        ui.add_space(6.0);

        // Bulk actions row
        ui.horizontal_wrapped(|ui| {
            if busy {
                ui.spinner();
                ui.label(format!("Writing tags to {} album(s)…", self.working.len()));
            } else {
                if ui
                    .add_enabled_ui(flag_n > 0, |ui| {
                        theme::button(ui, Some(Icon::Tag), &format!("Copy {flag_n} flagged for AI discussion"), Kind::Secondary)
                    })
                    .inner
                    .on_hover_text("Exports flagged albums with full tracklists, previous labels and your notes to discuss with an AI")
                    .clicked()
                {
                    if let Some((prompt, n)) = ai_enrich::export_followup_prompt(&lib, &tax, self.pending.as_ref().unwrap()) {
                        ui.ctx().copy_text(prompt);
                        let msg = format!("Copied follow-up prompt for {n} flagged album(s). Paste into your AI, then import the updated JSON.");
                        lines.push(format!("AI Enrich: {msg}"));
                        self.banner = Some((msg, false));
                        self.show_import = true;
                    }
                }
                if ui
                    .add_enabled_ui(flag_n > 0, |ui| {
                        theme::button(ui, Some(Icon::Folder), "Save flagged (.md)", Kind::Secondary)
                    })
                    .inner
                    .clicked()
                {
                    if let Some((prompt, n)) = ai_enrich::export_followup_prompt(&lib, &tax, self.pending.as_ref().unwrap()) {
                        match ai_enrich::save_prompt_file(&s.config_dir, "followup", &prompt) {
                            Ok(path) => {
                                open_path(path.parent().unwrap_or(&path));
                                let msg = format!("Saved follow-up prompt for {n} album(s) to {}", path.display());
                                lines.push(format!("AI Enrich: {msg}"));
                                self.banner = Some((msg, false));
                                self.show_import = true;
                            }
                            Err(e) => self.banner = Some((format!("Couldn't save follow-up prompt: {e:#}"), true)),
                        }
                    }
                }
                ui.separator();
                if ui
                    .add_enabled_ui(high_n > 0, |ui| {
                        theme::button(ui, Some(Icon::Check), &format!("Apply {high_n} high-confidence"), Kind::Primary)
                    })
                    .inner
                    .clicked()
                {
                    let batch: Vec<Proposal> = self
                        .pending
                        .as_ref()
                        .unwrap()
                        .items
                        .values()
                        .filter(|x| x.confidence == Confidence::High && !x.flagged && x.warnings.is_empty())
                        .cloned()
                        .collect();
                    self.start_apply(s, prefs, lib.clone(), batch, tax.version.clone(), ui.ctx().clone());
                }
                let ready_n = all_n.saturating_sub(flag_n);
                if ui
                    .add_enabled_ui(ready_n > 0, |ui| {
                        theme::button(ui, Some(Icon::Check), &format!("Apply all unflagged ({ready_n})"), Kind::Secondary)
                    })
                    .inner
                    .clicked()
                {
                    let batch: Vec<Proposal> = self
                        .pending
                        .as_ref()
                        .unwrap()
                        .items
                        .values()
                        .filter(|x| !x.flagged)
                        .cloned()
                        .collect();
                    self.start_apply(s, prefs, lib.clone(), batch, tax.version.clone(), ui.ctx().clone());
                }
            }
        });

        ui.add_space(6.0);
        ui.separator();

        // ---- Step 3: Review cards ----
        let needle = self.search.to_lowercase();
        let mut ids: Vec<String> = self
            .pending
            .as_ref()
            .unwrap()
            .items
            .values()
            .filter(|x| match self.filter {
                Filter::All => true,
                Filter::Attention => x.needs_attention(),
                Filter::Flagged => x.flagged,
                Filter::High => x.confidence == Confidence::High && !x.flagged && x.warnings.is_empty(),
            })
            .filter(|x| {
                needle.is_empty()
                    || x.artist.to_lowercase().contains(&needle)
                    || x.album.to_lowercase().contains(&needle)
                    || x.broad_genre.to_lowercase().contains(&needle)
                    || x.subgenres.iter().any(|g| g.to_lowercase().contains(&needle))
                    || x.moods.iter().any(|m| m.to_lowercase().contains(&needle))
            })
            .map(|x| x.album_id.clone())
            .collect();

        // Sort: flagged first, then low -> medium -> high confidence, then artist/album.
        {
            let map = &self.pending.as_ref().unwrap().items;
            ids.sort_by(|a, b| {
                let (pa, pb) = (&map[a], &map[b]);
                (!pa.flagged, pa.confidence, pa.artist.to_lowercase(), pa.album.to_lowercase())
                    .cmp(&(!pb.flagged, pb.confidence, pb.artist.to_lowercase(), pb.album.to_lowercase()))
            });
        }

        if ids.is_empty() {
            ui.add_space(20.0);
            let msg = if all_n == 0 {
                "No labels waiting for review. Click \"Copy prompt for AI\" above to enrich a batch of albums."
            } else {
                "No pending albums match this filter."
            };
            ui.label(RichText::new(msg).color(p.dim));
            return lines;
        }

        let mut dirty = false;
        let mut to_apply: Option<Proposal> = None;
        let mut to_dismiss: Option<String> = None;

        egui::ScrollArea::vertical().id_salt("ai-review-list").auto_shrink([false, false]).show(ui, |ui| {
            let pending = self.pending.as_mut().unwrap();
            for id in ids {
                let Some(prop) = pending.items.get_mut(&id) else { continue };
                let row_busy = self.working.contains(&id);

                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Header row: Artist — Album (Year) + badges + Apply / Dismiss
                    ui.horizontal_wrapped(|ui| {
                        let yr = prop.year.map(|y| format!(" ({y})")).unwrap_or_default();
                        ui.label(RichText::new(format!("{} — {}{}", prop.artist, prop.album, yr)).font(theme::medium(15.0)));
                        let conf_color = match prop.confidence {
                            Confidence::High => theme::OK,
                            Confidence::Medium => theme::WARN,
                            Confidence::Low => theme::ERR,
                        };
                        theme::badge(ui, prop.confidence.label(), conf_color);
                        if prop.flagged {
                            theme::badge(ui, "Flagged", theme::WARN);
                        }
                        if !prop.old_genres.is_empty() {
                            ui.weak(format!("was: {}", prop.old_genres.join(", ")));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if row_busy {
                                ui.spinner();
                                ui.label("Writing…");
                            } else {
                                if ui.small_button("Dismiss").clicked() {
                                    to_dismiss = Some(id.clone());
                                }
                                if ui
                                    .add_enabled(!busy, egui::Button::new("Apply"))
                                    .clicked()
                                {
                                    to_apply = Some(prop.clone());
                                }
                            }
                        });
                    });

                    // Broad genre + Subgenres editor row
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Broad:").color(p.dim));
                        let prev_broad = prop.broad_genre.clone();
                        egui::ComboBox::from_id_salt(("ai-broad", &id))
                            .selected_text(if prop.broad_genre.is_empty() { "(pick)" } else { &prop.broad_genre })
                            .show_ui(ui, |ui| {
                                for b in tax.broad_names() {
                                    ui.selectable_value(&mut prop.broad_genre, b.to_string(), b);
                                }
                            });
                        if prop.broad_genre != prev_broad {
                            prop.revalidate(&tax);
                            dirty = true;
                        }

                        ui.add_space(8.0);
                        ui.label(RichText::new("Subgenres:").color(p.dim));
                        let mut remove_sub = None;
                        for (i, sub) in prop.subgenres.iter().enumerate() {
                            if chip(ui, sub, theme::ACCENT) {
                                remove_sub = Some(i);
                            }
                        }
                        if let Some(i) = remove_sub {
                            prop.subgenres.remove(i);
                            prop.revalidate(&tax);
                            dirty = true;
                        }
                        let mut pick_sub = String::new();
                        egui::ComboBox::from_id_salt(("ai-sub", &id))
                            .width(120.0)
                            .selected_text("+ Subgenre")
                            .show_ui(ui, |ui| {
                                let primary = tax.subgenres_for(&prop.broad_genre);
                                for sub in &primary {
                                    if !prop.subgenres.iter().any(|x| x.eq_ignore_ascii_case(sub)) {
                                        if ui.selectable_label(false, *sub).clicked() {
                                            pick_sub = (*sub).to_string();
                                        }
                                    }
                                }
                                if !primary.is_empty() {
                                    ui.separator();
                                }
                                for sub in tax.all_subgenres() {
                                    if !primary.contains(&sub) && !prop.subgenres.iter().any(|x| x.eq_ignore_ascii_case(sub)) {
                                        if ui.selectable_label(false, sub).clicked() {
                                            pick_sub = sub.to_string();
                                        }
                                    }
                                }
                            });
                        if !pick_sub.is_empty() {
                            prop.subgenres.push(pick_sub);
                            prop.revalidate(&tax);
                            dirty = true;
                        }
                    });

                    // Moods editor row
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Moods:").color(p.dim));
                        let mut remove_mood = None;
                        for (i, m) in prop.moods.iter().enumerate() {
                            if chip(ui, m, theme::prism_at(0.15)) {
                                remove_mood = Some(i);
                            }
                        }
                        if let Some(i) = remove_mood {
                            prop.moods.remove(i);
                            prop.revalidate(&tax);
                            dirty = true;
                        }
                        let mut pick_mood = String::new();
                        egui::ComboBox::from_id_salt(("ai-mood", &id))
                            .width(110.0)
                            .selected_text("+ Mood")
                            .show_ui(ui, |ui| {
                                for m in &tax.moods {
                                    if !prop.moods.iter().any(|x| x.eq_ignore_ascii_case(m)) {
                                        if ui.selectable_label(false, m).clicked() {
                                            pick_mood = m.clone();
                                        }
                                    }
                                }
                            });
                        if !pick_mood.is_empty() {
                            prop.moods.push(pick_mood);
                            prop.revalidate(&tax);
                            dirty = true;
                        }
                    });

                    if !prop.ai_note.is_empty() {
                        ui.label(RichText::new(format!("AI note: {}", prop.ai_note)).small().color(p.dim));
                    }
                    for w in &prop.warnings {
                        ui.label(RichText::new(format!("⚠ {w}")).small().color(theme::WARN));
                    }

                    // Flag for follow-up discussion + user note
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut prop.flagged, "Flag for AI discussion").changed() {
                            dirty = true;
                        }
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut prop.user_note)
                                .hint_text("Optional note for follow-up AI discussion (e.g. 'more folk than rock')")
                                .desired_width(ui.available_width().max(180.0)),
                        );
                        if resp.changed() {
                            if !prop.user_note.trim().is_empty() {
                                prop.flagged = true;
                            }
                            dirty = true;
                        }
                    });
                });
                ui.add_space(6.0);
            }
        });

        if let Some(id) = to_dismiss {
            if let Some(pending) = &mut self.pending {
                pending.items.remove(&id);
                dirty = true;
            }
        }
        if dirty {
            if let Some(pending) = &self.pending {
                let _ = pending.save(&s.config_dir);
            }
        }
        if let Some(prop) = to_apply {
            self.start_apply(s, prefs, lib, vec![prop], tax.version, ui.ctx().clone());
        }

        lines
    }

    fn do_import(&mut self, s: &Settings, lib: &Library, tax: &Taxonomy, text: &str, lines: &mut Vec<String>) {
        let pending = self.pending.as_mut().unwrap();
        match ai_enrich::import_response(text, lib, tax, pending) {
            Ok(sum) => {
                let _ = pending.save(&s.config_dir);
                self.import_text.clear();
                self.show_import = false;
                let mut msg = format!("Imported {} new, {} updated", sum.added, sum.updated);
                if sum.warning_count > 0 {
                    msg += &format!(" ({} with taxonomy warnings)", sum.warning_count);
                }
                if !sum.unmatched.is_empty() {
                    msg += &format!(" ({} unmatched)", sum.unmatched.len());
                }
                lines.push(format!("AI Enrich: {msg}"));
                self.banner = Some((msg, false));
            }
            Err(e) => {
                self.banner = Some((format!("{e:#}"), true));
            }
        }
    }
}

/// Clickable removable chip: returns true if clicked to remove.
fn chip(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    let text = format!("{label} ×");
    let btn = egui::Button::new(RichText::new(text).small().color(color))
        .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 32))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 110)))
        .corner_radius(6.0);
    ui.add(btn).on_hover_text("Click to remove").clicked()
}

fn open_path(path: &std::path::Path) {
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(program).arg(path).spawn();
}
