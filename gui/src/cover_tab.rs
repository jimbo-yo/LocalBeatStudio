//! Missing Cover Art page: finds albums without cover art across local and
//! server libraries, searches Apple iTunes + Cover Art Archive for candidates
//! with visual previews, lets you cycle alternates / tweak search / load a
//! local image / export stubborn albums to an AI to identify, and embeds
//! approved covers into every audio file + `cover.jpg` (locally and on the
//! LocalBeat server, preserving `mtime`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Color32, RichText};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::cover_finder::{self, CoverCandidate, CoverImage};
use localbeat_core::library::{Library, Origin};
use localbeat_engine::cover_apply::{self, CoverApplyReport, CoverUpdate};

use crate::theme::{self, Icon, Kind};

struct AlbumCoverState {
    /// Editable search query override (artist, album).
    query_artist: String,
    query_album: String,
    candidates: Vec<CoverCandidate>,
    selected: usize,
    searched: bool,
    searching: bool,
    approved: bool,
    /// Optional local file override loaded by the user.
    custom_image: Option<CoverImage>,
    /// Local image path input box.
    file_input: String,
    /// Status note for this album (e.g. error).
    note: Option<String>,
}

enum CoverMsg {
    SearchDone {
        album_key: String,
        candidates: Vec<CoverCandidate>,
    },
    ThumbLoaded {
        url: String,
        rgba: Vec<u8>,
        width: usize,
        height: usize,
    },
    BatchSearchProgress {
        done: usize,
        total: usize,
    },
    BatchSearchFinished,
    ApplyFinished(Result<CoverApplyReport, String>),
}

pub struct CoverTab {
    items: HashMap<String, AlbumCoverState>,
    thumbs: HashMap<String, egui::TextureHandle>,
    thumbs_inflight: HashSet<String>,
    filter_search: String,
    only_unmatched: bool,
    show_ai_import: bool,
    ai_import_text: String,
    /// Background batch search progress (done, total).
    batch_progress: Option<(usize, usize)>,
    applying: bool,
    banner: Option<(String, bool)>,
    rx: Receiver<CoverMsg>,
    tx: Sender<CoverMsg>,
    /// Set true after applying covers so `main.rs` triggers a library rescan.
    pub rescan_wanted: bool,
}

impl CoverTab {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        CoverTab {
            items: HashMap::new(),
            thumbs: HashMap::new(),
            thumbs_inflight: HashSet::new(),
            filter_search: String::new(),
            only_unmatched: false,
            show_ai_import: false,
            ai_import_text: String::new(),
            batch_progress: None,
            applying: false,
            banner: None,
            rx,
            tx,
            rescan_wanted: false,
        }
    }

    pub fn missing_count(lib: Option<&Library>) -> usize {
        lib.map_or(0, |l| cover_finder::missing_cover_albums(l).len())
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                CoverMsg::SearchDone { album_key, candidates } => {
                    let st = self.items.entry(album_key).or_insert_with(|| AlbumCoverState {
                        query_artist: String::new(),
                        query_album: String::new(),
                        candidates: Vec::new(),
                        selected: 0,
                        searched: true,
                        searching: false,
                        approved: false,
                        custom_image: None,
                        file_input: String::new(),
                        note: None,
                    });
                    st.searching = false;
                    st.searched = true;
                    st.selected = 0;
                    // Auto-approve strong matches (>= 78%) so reviewing 40 albums is effortless!
                    st.approved = candidates.first().is_some_and(|c| c.score >= 0.78);
                    st.candidates = candidates;
                    st.note = None;
                }
                CoverMsg::ThumbLoaded { url, rgba, width, height } => {
                    self.thumbs_inflight.remove(&url);
                    let color = egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba);
                    let tex = ctx.load_texture(format!("cov:{url}"), color, egui::TextureOptions::LINEAR);
                    self.thumbs.insert(url, tex);
                }
                CoverMsg::BatchSearchProgress { done, total } => {
                    self.batch_progress = Some((done, total));
                }
                CoverMsg::BatchSearchFinished => {
                    self.batch_progress = None;
                    let found = self.items.values().filter(|s| !s.candidates.is_empty()).count();
                    let approved = self.items.values().filter(|s| s.approved).count();
                    self.banner = Some((
                        format!("Search finished: found covers for {found} album(s), {approved} auto-approved. Review below and click Apply Approved."),
                        false,
                    ));
                }
                CoverMsg::ApplyFinished(res) => {
                    self.applying = false;
                    match res {
                        Ok(report) => {
                            for key in &report.albums_applied {
                                self.items.remove(key);
                            }
                            let summary = report.summary();
                            lines.push(format!("Cover Art: {summary}"));
                            for n in &report.notes {
                                lines.push(format!("Cover Art: {n}"));
                            }
                            let has_err = report.albums_applied.is_empty() && !report.notes.is_empty();
                            let msg = if report.notes.is_empty() {
                                summary
                            } else {
                                format!("{summary} ({})", report.notes.join("; "))
                            };
                            self.banner = Some((msg, has_err));
                            if !report.albums_applied.is_empty() {
                                self.rescan_wanted = true;
                            }
                        }
                        Err(e) => {
                            lines.push(format!("Cover Art apply failed: {e}"));
                            self.banner = Some((e, true));
                        }
                    }
                }
            }
            ctx.request_repaint();
        }
        lines
    }

    fn request_thumb(&mut self, url: &str, ctx: &egui::Context) {
        if url.is_empty() || self.thumbs.contains_key(url) || !self.thumbs_inflight.insert(url.to_string()) {
            return;
        }
        let url = url.to_string();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Ok(img) = cover_finder::download_cover(&url) {
                if let Ok(decoded) = image::load_from_memory(&img.data) {
                    let rgba = decoded.thumbnail(200, 200).to_rgba8();
                    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                    let _ = tx.send(CoverMsg::ThumbLoaded { url, rgba: rgba.into_raw(), width: w, height: h });
                    ctx.request_repaint();
                }
            }
        });
    }

    fn start_single_search(&mut self, album_key: String, artist: String, album: String, ctx: &egui::Context) {
        if let Some(st) = self.items.get_mut(&album_key) {
            st.searching = true;
            st.note = None;
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let candidates = cover_finder::search_covers(&artist, &album);
            let _ = tx.send(CoverMsg::SearchDone { album_key, candidates });
            ctx.request_repaint();
        });
    }

    fn start_batch_search(&mut self, jobs: Vec<(String, String, String)>, ctx: &egui::Context) {
        let total = jobs.len();
        if total == 0 {
            return;
        }
        for (key, _, _) in &jobs {
            if let Some(st) = self.items.get_mut(key) {
                st.searching = true;
            }
        }
        self.batch_progress = Some((0, total));
        self.banner = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for (i, (album_key, artist, album)) in jobs.into_iter().enumerate() {
                let candidates = cover_finder::search_covers(&artist, &album);
                let _ = tx.send(CoverMsg::SearchDone { album_key, candidates });
                let _ = tx.send(CoverMsg::BatchSearchProgress { done: i + 1, total });
                ctx.request_repaint();
            }
            let _ = tx.send(CoverMsg::BatchSearchFinished);
            ctx.request_repaint();
        });
    }

    fn start_apply(
        &mut self,
        s: &Settings,
        prefs: &Prefs,
        lib: Arc<Library>,
        plan: Vec<(String, Option<CoverImage>, Option<String>)>,
        ctx: &egui::Context,
    ) {
        if plan.is_empty() {
            return;
        }
        self.applying = true;
        self.banner = Some((format!("Downloading and embedding cover art for {} album(s)…", plan.len()), false));
        let s = s.clone();
        let sp = prefs.server.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut updates = Vec::new();
            let mut notes = Vec::new();
            for (album_key, custom, url) in plan {
                let img = if let Some(c) = custom {
                    Ok(c)
                } else if let Some(u) = url {
                    cover_finder::download_cover(&u)
                } else {
                    continue;
                };
                match img {
                    Ok(c) => updates.push(CoverUpdate { album_key, data: c.data, mime: c.mime }),
                    Err(e) => notes.push(format!("download failed: {e:#}")),
                }
            }
            let res = cover_apply::apply_covers(&s, &sp, &lib, &updates)
                .map(|mut r| {
                    r.notes.extend(notes);
                    r
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(CoverMsg::ApplyFinished(res));
            ctx.request_repaint();
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &Prefs, lib: Option<Arc<Library>>) -> Vec<String> {
        let mut log = Vec::new();
        let p = theme::pal(ui);
        let ctx = ui.ctx().clone();

        let Some(lib) = lib else {
            ui.add_space(24.0);
            ui.label(RichText::new("Scanning library…").color(p.dim));
            return log;
        };

        let missing = cover_finder::missing_cover_albums(&lib);

        // Ensure every missing album has an entry in `self.items`.
        for a in &missing {
            self.items.entry(a.key.clone()).or_insert_with(|| AlbumCoverState {
                query_artist: a.artist.clone(),
                query_album: a.title.clone(),
                candidates: Vec::new(),
                selected: 0,
                searched: false,
                searching: false,
                approved: false,
                custom_image: None,
                file_input: String::new(),
                note: None,
            });
        }

        let approved_count = missing
            .iter()
            .filter(|a| self.items.get(&a.key).is_some_and(|st| st.approved && (st.custom_image.is_some() || !st.candidates.is_empty())))
            .count();
        let unmatched_count = missing
            .iter()
            .filter(|a| self.items.get(&a.key).is_some_and(|st| st.searched && st.candidates.is_empty() && st.custom_image.is_none()))
            .count();

        // ---- Action bar ----
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} album(s) missing cover art", missing.len()))
                        .font(theme::medium(15.0))
                        .color(p.text),
                );
                if let Some((done, total)) = self.batch_progress {
                    ui.add_space(10.0);
                    theme::badge(ui, &format!("Searching {done}/{total}…"), theme::ACCENT);
                } else if approved_count > 0 {
                    ui.add_space(10.0);
                    theme::badge(ui, &format!("{approved_count} approved"), theme::OK);
                }
                if unmatched_count > 0 {
                    ui.add_space(6.0);
                    theme::badge(ui, &format!("{unmatched_count} unmatched"), theme::WARN);
                }
            });
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                let busy = self.batch_progress.is_some() || self.applying;
                let search_label = if self.batch_progress.is_some() { "Searching…" } else { "Search Missing Covers" };
                if ui
                    .add_enabled_ui(!busy && !missing.is_empty(), |ui| {
                        theme::button(ui, Some(Icon::Refresh), search_label, Kind::Primary)
                    })
                    .inner
                    .on_hover_text("Searches Apple iTunes + Cover Art Archive for every album missing cover art")
                    .clicked()
                {
                    let jobs: Vec<(String, String, String)> = missing
                        .iter()
                        .filter_map(|a| {
                            let st = self.items.get(&a.key)?;
                            (!st.searched || st.candidates.is_empty())
                                .then(|| (a.key.clone(), st.query_artist.clone(), st.query_album.clone()))
                        })
                        .collect();
                    if jobs.is_empty() {
                        let all: Vec<_> = missing
                            .iter()
                            .filter_map(|a| {
                                let st = self.items.get(&a.key)?;
                                Some((a.key.clone(), st.query_artist.clone(), st.query_album.clone()))
                            })
                            .collect();
                        self.start_batch_search(all, &ctx);
                    } else {
                        self.start_batch_search(jobs, &ctx);
                    }
                }

                if ui
                    .add_enabled_ui(!busy && approved_count > 0, |ui| {
                        theme::button(ui, Some(Icon::Check), &format!("Apply Approved ({approved_count})"), Kind::Primary)
                    })
                    .inner
                    .on_hover_text("Embeds approved covers into every audio file + cover.jpg (locally and on the server)")
                    .clicked()
                {
                    let plan: Vec<_> = missing
                        .iter()
                        .filter_map(|a| {
                            let st = self.items.get(&a.key)?;
                            if !st.approved {
                                return None;
                            }
                            let url = st.candidates.get(st.selected).map(|c| c.full_url.clone());
                            if st.custom_image.is_some() || url.is_some() {
                                Some((a.key.clone(), st.custom_image.clone(), url))
                            } else {
                                None
                            }
                        })
                        .collect();
                    self.start_apply(s, prefs, lib.clone(), plan, &ctx);
                }

                if unmatched_count > 0 {
                    if theme::button(ui, None, &format!("Copy AI Prompt ({unmatched_count})"), Kind::Secondary)
                        .on_hover_text("Copies a prompt for any AI to identify the canonical Artist & Album names for unmatched folders")
                        .clicked()
                    {
                        let hard: Vec<_> = missing
                            .iter()
                            .copied()
                            .filter(|a| self.items.get(&a.key).is_some_and(|st| st.searched && st.candidates.is_empty() && st.custom_image.is_none()))
                            .collect();
                        let prompt = cover_finder::export_ai_identify_prompt(&lib, &hard);
                        ctx.copy_text(prompt);
                        self.banner = Some((format!("Copied AI identification prompt for {} unmatched album(s) to clipboard.", hard.len()), false));
                        log.push(format!("Cover Art: copied AI identify prompt for {} album(s)", hard.len()));
                    }
                    if theme::button(ui, None, "Import AI Hints", Kind::Secondary).clicked() {
                        self.show_ai_import = !self.show_ai_import;
                    }
                }
            });
        });

        if let Some((msg, is_err)) = &self.banner {
            ui.add_space(8.0);
            let tint = if *is_err { theme::ERR } else { theme::OK };
            egui::Frame::new()
                .fill(tint.gamma_multiply(0.12))
                .stroke(egui::Stroke::new(1.0, tint.gamma_multiply(0.5)))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(msg).color(p.text));
                });
        }

        if self.show_ai_import {
            ui.add_space(8.0);
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Paste the AI's JSON response (canonical Artist & Album names):").font(theme::medium(13.5)).color(p.text));
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.ai_import_text)
                            .desired_width(f32::INFINITY)
                            .desired_rows(5)
                            .code_editor(),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Some(Icon::Check), "Apply Hints & Re-search", Kind::Primary).clicked() {
                        match cover_finder::import_ai_identify_response(&self.ai_import_text) {
                            Ok(hints) => {
                                let by_short: HashMap<String, String> = missing
                                    .iter()
                                    .map(|a| (cover_finder::album_short_id(a), a.key.clone()))
                                    .collect();
                                let mut jobs = Vec::new();
                                for h in hints {
                                    if let Some(key) = by_short.get(&h.id) {
                                        if let Some(st) = self.items.get_mut(key) {
                                            st.query_artist = h.search_artist.clone();
                                            st.query_album = h.search_album.clone();
                                        }
                                        jobs.push((key.clone(), h.search_artist, h.search_album));
                                    }
                                }
                                let n = jobs.len();
                                self.show_ai_import = false;
                                self.ai_import_text.clear();
                                if n > 0 {
                                    self.start_batch_search(jobs, &ctx);
                                } else {
                                    self.banner = Some(("No album IDs in the AI response matched the current missing list.".into(), true));
                                }
                            }
                            Err(e) => {
                                self.banner = Some((format!("Could not parse AI JSON: {e:#}"), true));
                            }
                        }
                    }
                    if theme::button(ui, None, "Cancel", Kind::Secondary).clicked() {
                        self.show_ai_import = false;
                    }
                });
            });
        }

        if missing.is_empty() {
            ui.add_space(32.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Every album in your library has cover art!").font(theme::bold(18.0)).color(theme::OK));
            });
            return log;
        }

        // ---- Filter row ----
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter_search).hint_text("Filter missing albums…").desired_width(240.0));
            ui.add_space(8.0);
            ui.checkbox(&mut self.only_unmatched, "Only unmatched");
        });
        ui.add_space(8.0);

        // Collect thumb URLs to fetch & single-album actions triggered during draw.
        let mut to_fetch_thumbs: Vec<String> = Vec::new();
        let mut single_search: Option<(String, String, String)> = None;
        let mut single_apply: Option<(String, Option<CoverImage>, Option<String>)> = None;

        let filter_q = self.filter_search.trim().to_lowercase();
        let busy = self.applying;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for album in &missing {
                if !filter_q.is_empty()
                    && !album.artist.to_lowercase().contains(&filter_q)
                    && !album.title.to_lowercase().contains(&filter_q)
                {
                    continue;
                }
                let Some(st) = self.items.get_mut(&album.key) else { continue };
                if self.only_unmatched && (!st.candidates.is_empty() || st.custom_image.is_some()) {
                    continue;
                }

                let preview_url = st.candidates.get(st.selected).map(|c| c.thumb_url.clone());
                if let Some(u) = &preview_url {
                    if !self.thumbs.contains_key(u) {
                        to_fetch_thumbs.push(u.clone());
                    }
                }

                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        // Left: 96x96 cover preview box
                        let thumb_key = if st.custom_image.is_some() {
                            Some(format!("local:{}", album.key))
                        } else {
                            preview_url.clone()
                        };
                        if let Some(tex) = thumb_key.as_ref().and_then(|k| self.thumbs.get(k)) {
                            ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(96.0, 96.0)).corner_radius(10.0));
                        } else {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 96.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 10.0, p.base);
                            let label = if st.searching { "…" } else { "No cover" };
                            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, theme::medium(12.0), p.faint);
                        }

                        ui.add_space(12.0);

                        // Right: album info + candidate controls + custom search/file
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                let yr = album.year.map(|y| format!(" ({y})")).unwrap_or_default();
                                ui.label(RichText::new(format!("{} — {}{yr}", album.artist, album.title)).font(theme::bold(14.5)).color(p.text));
                                let orig = match album.origin {
                                    Origin::Flac => "FLAC",
                                    Origin::Misc => "Misc",
                                    Origin::Server => "Server",
                                };
                                theme::badge(ui, orig, p.faint);
                                theme::badge(ui, &format!("{} tracks", album.songs.len()), p.faint);
                            });

                            ui.add_space(4.0);

                            // Candidate switcher / status
                            if let Some((cw, ch)) = st.custom_image.as_ref().map(|c| (c.width, c.height)) {
                                ui.horizontal(|ui| {
                                    theme::badge(ui, &format!("Local file · {cw}×{ch}"), theme::OK);
                                    if ui.small_button("Clear file").clicked() {
                                        st.custom_image = None;
                                    }
                                });
                            } else if !st.candidates.is_empty() {
                                let total = st.candidates.len();
                                let cur = &st.candidates[st.selected];
                                ui.horizontal_wrapped(|ui| {
                                    if total > 1 && ui.small_button("◀").clicked() {
                                        st.selected = if st.selected == 0 { total - 1 } else { st.selected - 1 };
                                    }
                                    ui.label(
                                        RichText::new(format!(
                                            "{}/{total}  [{}] {} — {} ({:.0}%)",
                                            st.selected + 1,
                                            cur.source,
                                            cur.artist,
                                            cur.title,
                                            cur.score * 100.0
                                        ))
                                        .small()
                                        .color(p.dim),
                                    );
                                    if total > 1 && ui.small_button("▶").clicked() {
                                        st.selected = (st.selected + 1) % total;
                                    }
                                });
                            } else if st.searching {
                                ui.label(RichText::new("Searching iTunes & Cover Art Archive…").small().color(theme::ACCENT));
                            } else if st.searched {
                                ui.label(RichText::new("No match found — tweak the search below, use AI Prompt, or load a local image file.").small().color(theme::WARN));
                            } else {
                                ui.label(RichText::new("Click Search Missing Covers (or Search below) to look up art.").small().color(p.faint));
                            }

                            ui.add_space(4.0);

                            // Inline tweak-query + local file loader + Approve / Apply
                            ui.horizontal_wrapped(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut st.query_artist).hint_text("Artist").desired_width(130.0));
                                ui.add(egui::TextEdit::singleline(&mut st.query_album).hint_text("Album").desired_width(170.0));
                                if ui.small_button("Search").clicked() && !st.searching {
                                    single_search = Some((album.key.clone(), st.query_artist.clone(), st.query_album.clone()));
                                }
                                ui.add_space(6.0);
                                ui.add(egui::TextEdit::singleline(&mut st.file_input).hint_text("Local image path…").desired_width(160.0));
                                if ui.small_button("Load").clicked() {
                                    let path = PathBuf::from(st.file_input.trim().trim_matches('"'));
                                    match cover_finder::load_local_cover(&path) {
                                        Ok(img) => {
                                            if let Ok(dec) = image::load_from_memory(&img.data) {
                                                let rgba = dec.thumbnail(200, 200).to_rgba8();
                                                let color = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                                                let tex = ctx.load_texture(format!("cov:local:{}", album.key), color, egui::TextureOptions::LINEAR);
                                                self.thumbs.insert(format!("local:{}", album.key), tex);
                                            }
                                            st.custom_image = Some(img);
                                            st.approved = true;
                                            st.note = None;
                                        }
                                        Err(e) => st.note = Some(format!("Couldn't load image: {e:#}")),
                                    }
                                }
                            });

                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                let has_art = st.custom_image.is_some() || !st.candidates.is_empty();
                                ui.add_enabled_ui(has_art, |ui| {
                                    ui.checkbox(&mut st.approved, "Approve");
                                });
                                if ui.add_enabled(!busy && has_art, egui::Button::new("Apply now")).clicked() {
                                    let url = st.candidates.get(st.selected).map(|c| c.full_url.clone());
                                    single_apply = Some((album.key.clone(), st.custom_image.clone(), url));
                                }
                                if let Some(n) = &st.note {
                                    ui.label(RichText::new(n).small().color(Color32::from_rgb(255, 130, 130)));
                                }
                            });
                        });
                    });
                });
                ui.add_space(6.0);
            }
        });

        for u in to_fetch_thumbs {
            self.request_thumb(&u, &ctx);
        }
        if let Some((key, artist, album)) = single_search {
            self.start_single_search(key, artist, album, &ctx);
        }
        if let Some(one) = single_apply {
            self.start_apply(s, prefs, lib, vec![one], &ctx);
        }

        log
    }
}
