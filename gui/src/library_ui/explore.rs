//! Explore, and the genre, mood and decade pages, laid out like the phone
//! app's (LocalBeat `docs/ui-refinement.md`, terms in its `GLOSSARY.md`).

use std::sync::Arc;

use eframe::egui::{self, RichText, Sense};
use localbeat_core::browse::{Browse, Group, decade_key};
use localbeat_core::library::{Library, norm, sort_name};

use super::{Action, LibraryUi, Page};
use crate::theme::{self, Icon, Kind};

/// Which genre, mood or decade page is open, and its filters.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupView {
    pub kind: GroupKind,
    /// `Group::key`.
    pub key: String,
    /// On a genre page: the chosen subgenre.
    pub sub: Option<String>,
    /// Decades on genre and mood pages, moods on decade pages.
    pub cross: Option<String>,
    /// Genres on mood and decade pages.
    pub secondary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Genre,
    Mood,
    Decade,
}

impl GroupView {
    pub fn new(kind: GroupKind, key: impl Into<String>) -> GroupView {
        GroupView { kind, key: key.into(), sub: None, cross: None, secondary: None }
    }
}

/// How many albums the Recently added strip shows.
const RECENT_STRIP: usize = 12;

impl LibraryUi {
    /// The genres, moods and decades of the library shown (built once per library).
    pub(super) fn browse(&mut self, lib: &Arc<Library>) -> Arc<Browse> {
        let id = Arc::as_ptr(lib) as usize;
        match &self.browse {
            Some((k, b)) if *k == id => b.clone(),
            _ => {
                let b = Arc::new(Browse::new(lib));
                self.browse = Some((id, b.clone()));
                b
            }
        }
    }

    /// The group page for a genre name on an album or song ("Jangle Pop" opens
    /// its genre with that subgenre chosen).
    pub(super) fn genre_view(&mut self, lib: &Arc<Library>, name: &str) -> Option<GroupView> {
        let (key, sub) = self.browse(lib).find_genre(name)?;
        Some(GroupView { sub, ..GroupView::new(GroupKind::Genre, key) })
    }

    pub(super) fn explore_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let b = self.browse(lib);
        let p = theme::pal(ui);
        let subtitle = format!("{} albums · {} artists · {} songs", lib.albums.len(), lib.artists.len(), lib.songs.len());
        theme::page_header(ui, "Explore", &subtitle, |ui| {
            // Right to left: Shuffle all, then Random album.
            if !lib.albums.is_empty() && theme::button(ui, Some(Icon::Disc), "Random album", Kind::Secondary).clicked() {
                let a = (super::rand_u64() as usize) % lib.albums.len();
                self.actions.push(Action::Play(lib.albums[a].songs.clone(), 0));
                self.actions.push(Action::OpenAlbum(a));
            }
            if theme::button(ui, Some(Icon::Shuffle), "Shuffle all", Kind::Primary).clicked() {
                self.actions.push(Action::Shuffle(lib.albums.iter().flat_map(|a| a.songs.iter().copied()).collect()));
            }
        });
        egui::ScrollArea::vertical().id_salt("explore").auto_shrink([false, false]).show(ui, |ui| {
            // ---- Recently added
            ui.horizontal(|ui| {
                theme::section_label(ui, "Recently added");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Label::new(RichText::new("See all").color(theme::prism_at(0.3))).sense(Sense::click())).clicked() {
                        self.page = Page::Recent;
                    }
                });
            });
            let recent = lib.recently_added(RECENT_STRIP);
            egui::ScrollArea::horizontal().id_salt("explore-recent").max_height(246.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 18.0;
                    for &a in &recent {
                        self.tile(ui, lib, a, 180.0);
                    }
                });
            });
            ui.add_space(14.0);

            // ---- Moods (largest first)
            theme::section_label(ui, "Moods");
            ui.add_space(4.0);
            if b.moods.is_empty() {
                ui.label(RichText::new("No mood tags yet. Use AI Genres & Moods (Tools) to add them.").color(p.dim));
            } else {
                let mut moods: Vec<&Group> = b.moods.iter().collect();
                moods.sort_by(|x, y| y.albums.len().cmp(&x.albums.len()).then_with(|| sort_name(&x.name).cmp(&sort_name(&y.name))));
                ui.horizontal_wrapped(|ui| {
                    for m in moods {
                        if chip(ui, &format!("{}  {}", m.name, m.albums.len()), Tint::Mood, false).clicked() {
                            self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Mood, m.key.clone())));
                        }
                    }
                });
            }
            ui.add_space(16.0);

            // ---- Genres, by size or A–Z
            ui.horizontal(|ui| {
                theme::section_label(ui, "Genres");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (by_size, label) in [(false, "A–Z"), (true, "Size")] {
                        if ui.selectable_label(self.genres_by_size == by_size, label).clicked() {
                            self.genres_by_size = by_size;
                        }
                    }
                    ui.label(RichText::new("Sort by").color(p.faint));
                });
            });
            ui.add_space(4.0);
            if b.genres.is_empty() {
                ui.label(RichText::new("No genre tags yet.").color(p.dim));
            }
            let mut genres: Vec<_> = b.genres.iter().collect();
            if self.genres_by_size {
                genres.sort_by(|x, y| y.group.albums.len().cmp(&x.group.albums.len()).then_with(|| sort_name(&x.group.name).cmp(&sort_name(&y.group.name))));
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                for g in genres {
                    let mut sub = format!("{} albums", g.group.albums.len());
                    if !g.subgenres.is_empty() {
                        let names: Vec<&str> = g.subgenres.iter().take(3).map(|s| s.name.as_str()).collect();
                        sub = format!("{sub} · {}", names.join(", "));
                        if g.subgenres.len() > 3 {
                            sub.push_str(&format!(" +{}", g.subgenres.len() - 3));
                        }
                    }
                    if genre_card(ui, &g.group.name, &sub).clicked() {
                        self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Genre, g.group.key.clone())));
                    }
                }
            });
            ui.add_space(16.0);

            // ---- Decades
            theme::section_label(ui, "Decades");
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for d in &b.decades {
                    if chip(ui, &format!("{}  {}", d.name, d.albums.len()), Tint::Plain, false).clicked() {
                        self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Decade, d.key.clone())));
                    }
                }
            });
            ui.add_space(24.0);
        });
    }

    /// A genre, mood or decade: Play, Shuffle, filter chips, then the albums.
    pub(super) fn group_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, mut view: GroupView) {
        let b = self.browse(lib);
        let p = theme::pal(ui);
        let (name, base, subgenres): (String, &[usize], &[Group]) = match view.kind {
            GroupKind::Genre => match b.genre(&view.key) {
                Some(g) => (g.group.name.clone(), &g.group.albums, &g.subgenres),
                None => return self.close_group(),
            },
            GroupKind::Mood => match b.mood(&view.key) {
                Some(g) => (g.name.clone(), &g.albums, &[]),
                None => return self.close_group(),
            },
            GroupKind::Decade => match b.decade(&view.key) {
                Some(g) => (g.name.clone(), &g.albums, &[]),
                None => return self.close_group(),
            },
        };

        let back = if self.page == Page::Explore { "Explore".to_string() } else { page_name(self.page).to_string() };
        let clicked = ui.add(egui::Label::new(RichText::new(format!("‹  {back}")).color(p.dim)).sense(Sense::click())).clicked();
        let esc = ui.ctx().memory(|m| m.focused().is_none()) && ui.input(|i| i.key_pressed(egui::Key::Escape));
        if clicked || esc {
            return self.close_group();
        }

        // The albums left after the subgenre and the two chip rows.
        let in_sub: Vec<usize> = match view.sub.as_ref().and_then(|s| subgenres.iter().find(|g| &g.key == s)) {
            Some(g) => g.albums.clone(),
            None => base.to_vec(),
        };
        let (cross_kind, secondary_kind) = match view.kind {
            GroupKind::Genre => (Filter::Decade, Filter::None),
            GroupKind::Mood => (Filter::Decade, Filter::Genre),
            GroupKind::Decade => (Filter::Mood, Filter::Genre),
        };
        let matches = |a: usize, kind: Filter, key: &str| match kind {
            Filter::None => true,
            Filter::Decade => decade_key(lib.albums[a].year).as_deref() == Some(key),
            Filter::Mood => lib.albums[a].moods.iter().any(|m| norm(m) == key),
            Filter::Genre => b.genre_of[a].as_deref() == Some(key),
        };
        let shown: Vec<usize> = in_sub
            .iter()
            .copied()
            .filter(|&a| view.cross.as_deref().is_none_or(|k| matches(a, cross_kind, k)))
            .filter(|&a| view.secondary.as_deref().is_none_or(|k| matches(a, secondary_kind, k)))
            .collect();
        let songs: Vec<usize> = shown.iter().flat_map(|&a| lib.albums[a].songs.iter().copied()).collect();

        let subtitle = format!("{} albums · {} songs", shown.len(), songs.len());
        theme::page_header(ui, &name, &subtitle, |ui| {
            // Right to left: Play, then Shuffle.
            if theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                self.actions.push(Action::Shuffle(songs.clone()));
            }
            if theme::button(ui, Some(Icon::Play), "Play", Kind::Primary).clicked() {
                self.actions.push(Action::Play(songs.clone(), 0));
            }
        });

        let mut changed = false;
        if !subgenres.is_empty() {
            theme::section_label(ui, "Subgenres");
            ui.horizontal_wrapped(|ui| {
                if chip(ui, &format!("All ({})", base.len()), Tint::Plain, view.sub.is_none()).clicked() {
                    view.sub = None;
                    view.cross = None;
                    changed = true;
                }
                for s in subgenres {
                    let on = view.sub.as_deref() == Some(s.key.as_str());
                    if chip(ui, &format!("{} ({})", s.name, s.albums.len()), Tint::Plain, on).clicked() {
                        view.sub = if on { None } else { Some(s.key.clone()) };
                        view.cross = None;
                        changed = true;
                    }
                }
            });
            ui.add_space(4.0);
        }
        for (kind, selected) in [(secondary_kind, &mut view.secondary), (cross_kind, &mut view.cross)] {
            let options = filter_options(lib, &b, &in_sub, kind);
            if options.len() < 2 {
                continue;
            }
            ui.horizontal_wrapped(|ui| {
                for (key, label, n) in options {
                    let on = selected.as_deref() == Some(key.as_str());
                    let tint = if kind == Filter::Mood { Tint::Mood } else { Tint::Plain };
                    if chip(ui, &format!("{label} ({n})"), tint, on).clicked() {
                        *selected = if on { None } else { Some(key) };
                        changed = true;
                    }
                }
            });
            ui.add_space(4.0);
        }
        if changed {
            self.group = Some(view.clone());
        }
        ui.add_space(6.0);
        self.grid(ui, lib, &shown, &format!("group-{:?}-{}", view.kind, view.key));
    }

    fn close_group(&mut self) {
        self.group = None;
    }

    /// Tappable genre, mood and decade chips for an album (its page, and Details).
    pub(super) fn tag_chips(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, a: usize) {
        let album = &lib.albums[a];
        if album.genres.is_empty() && album.moods.is_empty() && album.year.is_none() {
            return;
        }
        let genres = album.genres.clone();
        let moods = album.moods.clone();
        let decade = decade_key(album.year);
        ui.horizontal_wrapped(|ui| {
            for g in &genres {
                if chip(ui, g, Tint::Plain, false).clicked()
                    && let Some(v) = self.genre_view(lib, g)
                {
                    self.actions.push(Action::OpenGroup(v));
                }
            }
            for m in &moods {
                if chip(ui, m, Tint::Mood, false).clicked() {
                    self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Mood, norm(m))));
                }
            }
            if let Some(d) = &decade
                && chip(ui, &format!("{d}s"), Tint::Plain, false).clicked()
            {
                self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Decade, d.clone())));
            }
        });
    }

    /// Search's first rows: moods, genres (and subgenres), decades that match.
    /// Returns how many it showed.
    pub(super) fn search_chips(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, hit: &dyn Fn(&str) -> bool) -> usize {
        let b = self.browse(lib);
        let moods: Vec<&Group> = b.moods.iter().filter(|m| hit(&m.key)).collect();
        let mut genres: Vec<(String, GroupView)> = Vec::new();
        for g in &b.genres {
            if hit(&g.group.key) {
                genres.push((g.group.name.clone(), GroupView::new(GroupKind::Genre, g.group.key.clone())));
            }
            for s in g.subgenres.iter().filter(|s| hit(&s.key)) {
                genres.push((s.name.clone(), GroupView { sub: Some(s.key.clone()), ..GroupView::new(GroupKind::Genre, g.group.key.clone()) }));
            }
        }
        genres.dedup_by(|x, y| x.0 == y.0);
        let decades: Vec<&Group> = b.decades.iter().filter(|d| hit(&norm(&d.name)) || hit(&format!("{}s", &d.key[2..]))).collect();
        let total = moods.len() + genres.len() + decades.len();
        if total == 0 {
            return 0;
        }
        ui.horizontal_wrapped(|ui| {
            for m in moods {
                if chip(ui, &m.name, Tint::Mood, false).clicked() {
                    self.search.clear();
                    self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Mood, m.key.clone())));
                }
            }
            for (name, v) in genres {
                if chip(ui, &name, Tint::Plain, false).clicked() {
                    self.search.clear();
                    self.actions.push(Action::OpenGroup(v));
                }
            }
            for d in decades {
                if chip(ui, &d.name, Tint::Plain, false).clicked() {
                    self.search.clear();
                    self.actions.push(Action::OpenGroup(GroupView::new(GroupKind::Decade, d.key.clone())));
                }
            }
        });
        ui.add_space(6.0);
        total
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    None,
    Decade,
    Mood,
    Genre,
}

/// (key, label, albums) for a chip row over `albums`.
fn filter_options(lib: &Library, b: &Browse, albums: &[usize], kind: Filter) -> Vec<(String, String, usize)> {
    let mut counts: Vec<(String, String, usize)> = Vec::new();
    let mut add = |key: String, label: String| match counts.iter_mut().find(|c| c.0 == key) {
        Some(c) => c.2 += 1,
        None => counts.push((key, label, 1)),
    };
    for &a in albums {
        match kind {
            Filter::None => {}
            Filter::Decade => {
                if let Some(d) = decade_key(lib.albums[a].year) {
                    add(d.clone(), format!("{d}s"));
                }
            }
            Filter::Mood => {
                for m in &lib.albums[a].moods {
                    let name = b.mood(&norm(m)).map(|g| g.name.clone()).unwrap_or_else(|| m.clone());
                    add(norm(m), name);
                }
            }
            Filter::Genre => {
                if let Some(g) = b.genre_of[a].as_ref() {
                    add(g.clone(), b.genre(g).map(|x| x.group.name.clone()).unwrap_or_else(|| g.clone()));
                }
            }
        }
    }
    match kind {
        Filter::Decade => counts.sort_by(|x, y| x.0.cmp(&y.0)),
        _ => counts.sort_by(|x, y| y.2.cmp(&x.2).then_with(|| x.1.cmp(&y.1))),
    }
    counts
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tint {
    Plain,
    /// Moods are tinted, as on the phone.
    Mood,
}

/// A small rounded pill; `on` = chosen filter.
fn chip(ui: &mut egui::Ui, text: &str, tint: Tint, on: bool) -> egui::Response {
    let p = theme::pal(ui);
    let accent = if tint == Tint::Mood { theme::prism_at(0.75) } else { theme::ACCENT };
    let (fill, stroke, color) = if on {
        (accent.gamma_multiply(0.35), accent, p.text)
    } else if tint == Tint::Mood {
        (accent.gamma_multiply(0.14), accent.gamma_multiply(0.45), p.text)
    } else {
        (p.card, p.line, p.dim)
    };
    let button = egui::Button::new(RichText::new(text).size(13.0).color(color)).fill(fill).stroke(egui::Stroke::new(1.0, stroke)).corner_radius(12.0);
    let r = ui.add(button);
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// A genre on Explore: its name, size and a few subgenres.
fn genre_card(ui: &mut egui::Ui, name: &str, sub: &str) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(250.0, 64.0), Sense::click());
    let fill = if resp.hovered() { p.card_hover } else { p.card };
    ui.painter().rect(rect, 10.0, fill, egui::Stroke::new(1.0, p.line), egui::StrokeKind::Inside);
    let inner = rect.shrink2(egui::vec2(14.0, 10.0));
    let title = theme::one_line(ui, name, theme::bold(15.0), p.text, inner.width());
    let line = theme::one_line(ui, sub, egui::FontId::proportional(12.5), p.dim, inner.width());
    ui.painter().galley(inner.left_top(), title, p.text);
    ui.painter().galley(inner.left_top() + egui::vec2(0.0, 24.0), line, p.dim);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn page_name(page: Page) -> &'static str {
    match page {
        Page::Explore => "Explore",
        Page::Albums => "Albums",
        Page::Recent => "Recently added",
        Page::Artists => "Artists",
        Page::Songs => "Songs",
        Page::Playlists => "Playlists",
    }
}

