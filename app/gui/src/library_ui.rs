//! The music side of the app: the library (Albums, Recently added, Artists,
//! Songs, search, album pages), the now-playing bar on every page, and the
//! Up Next queue. Playback is localbeat-player; the library comes from
//! core::library (scanned in the background, cached).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use eframe::egui::{self, Color32, Rect, Sense, Vec2, pos2, vec2};
use localbeat_core::config::Settings;
use localbeat_core::library::{Library, Origin, Song, norm};
use localbeat_player::{Player, State, Status};

use crate::cover_cache::CoverCache;
use crate::media_keys::SharedLibrary;
use crate::theme::{self, Icon, Kind};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Albums,
    Recent,
    Artists,
    Songs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlbumSort {
    Artist,
    Title,
    Year,
}

/// Something the user asked for that the page can't do while drawing.
enum Action {
    /// Play these songs (library indices), starting with `start`'s position.
    Play(Vec<usize>, usize),
    Shuffle(Vec<usize>),
    PlayNext(Vec<usize>),
    Append(Vec<usize>),
    OpenAlbum(usize),
    OpenArtist(usize),
    OpenFolder(PathBuf),
}

/// What's kept between runs (`<config>/player.json`).
#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq)]
struct Session {
    volume: f32,
    queue: Vec<PathBuf>,
    index: usize,
    position: f64,
    shuffle: bool,
    repeat: bool,
}

type ScanResult = Result<(Arc<Library>, localbeat_core::library::ScanStats), String>;

pub struct LibraryUi {
    settings: Settings,
    pub lib: Option<Arc<Library>>,
    by_path: HashMap<PathBuf, usize>,
    /// Lower-case "title artist album" per song, and "title artist" per album.
    song_text: Vec<String>,
    album_text: Vec<String>,
    scan: Option<Receiver<ScanResult>>,
    scan_progress: Arc<(AtomicUsize, AtomicUsize)>,
    rescan_wanted: bool,
    pub covers: CoverCache,
    pub player: Arc<Player>,
    /// The library for the media-keys thread.
    shared_lib: SharedLibrary,
    pub page: Page,
    /// Open album page.
    pub album: Option<usize>,
    artist: Option<usize>,
    pub search: String,
    pub search_id: egui::Id,
    pub queue_open: bool,
    shuffle: bool,
    repeat: bool,
    volume: f32,
    seek_preview: Option<f32>,
    sort: AlbumSort,
    /// The last status, to notice the end of the queue (repeat).
    last: Status,
    actions: Vec<Action>,
    log: Vec<String>,
    /// LOCALBEAT_DEMO value: "album", "play", "search", "artists"... (screenshots).
    pub demo: Option<String>,
    /// Where the page panels end (set by main.rs each frame): the bar's top.
    pub bar_top: Option<f32>,
    saved: Option<Session>,
    saved_at: std::time::Instant,
}

impl LibraryUi {
    pub fn new(settings: &Settings, ctx: &egui::Context) -> Self {
        let player = Arc::new(Player::new());
        let session: Session = std::fs::read(settings.config_dir.join("player.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Session { volume: 0.8, ..Default::default() });
        let volume = session.volume.clamp(0.0, 1.0);
        player.set_volume(volume * volume);
        if !session.queue.is_empty() {
            // Back where we were, paused.
            player.cue(session.queue.clone(), session.index.min(session.queue.len() - 1), session.position);
        }
        player.set_repeat(session.repeat);
        let shared_lib: SharedLibrary = Default::default();
        let media_err = crate::media_keys::spawn(player.clone(), shared_lib.clone(), settings.cache_dir.clone(), ctx.clone()).err();
        let mut me = LibraryUi {
            settings: settings.clone(),
            lib: None,
            by_path: HashMap::new(),
            song_text: Vec::new(),
            album_text: Vec::new(),
            scan: None,
            scan_progress: Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))),
            rescan_wanted: false,
            covers: CoverCache::new(settings.cache_dir.clone(), ctx.clone()),
            player,
            shared_lib,
            page: Page::Albums,
            album: None,
            artist: None,
            search: String::new(),
            search_id: egui::Id::unique("library-search"),
            queue_open: false,
            shuffle: session.shuffle,
            repeat: session.repeat,
            volume,
            seek_preview: None,
            sort: AlbumSort::Artist,
            last: Status::default(),
            actions: Vec::new(),
            log: media_err.into_iter().collect(),
            demo: None,
            bar_top: None,
            saved: Some(session),
            saved_at: std::time::Instant::now(),
        };
        me.rescan(ctx);
        me
    }

    /// Scans the library again in the background (after a rip, or asked).
    pub fn rescan(&mut self, ctx: &egui::Context) {
        if self.scan.is_some() {
            self.rescan_wanted = true;
            return;
        }
        let (tx, rx) = channel();
        let (settings, progress, ctx) = (self.settings.clone(), self.scan_progress.clone(), ctx.clone());
        let shared = self.shared_lib.clone();
        std::thread::spawn(move || {
            let r = Library::scan(&settings, &|done, total| {
                progress.0.store(done, Ordering::Relaxed);
                progress.1.store(total, Ordering::Relaxed);
            })
            .map(|(lib, st)| (Arc::new(lib), st))
            .map_err(|e| format!("{e:#}"));
            // The media-keys thread gets it now, even if the window is hidden.
            if let Ok((lib, _)) = &r {
                *shared.lock().unwrap() = Some(lib.clone());
            }
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.scan = Some(rx);
    }

    pub fn set_demo_volume(&mut self) {
        self.volume = 0.0;
        self.player.set_volume(0.0);
        // A demo doesn't overwrite the real session.
        self.saved_at = std::time::Instant::now() + Duration::from_secs(1_000_000);
    }

    /// Per frame: scan results, cover loads, repeat, repaint while playing.
    /// Returns lines for the activity log.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<String> {
        self.covers.poll(ctx);
        if let Some(r) = self.scan.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.scan = None;
            match r {
                Ok((lib, st)) => {
                    self.log.push(format!(
                        "Library: {} songs, {} albums ({} files read{})",
                        lib.songs.len(),
                        lib.albums.len(),
                        st.read,
                        if st.failed.is_empty() { String::new() } else { format!(", {} unreadable", st.failed.len()) }
                    ));
                    self.by_path = lib.songs.iter().enumerate().map(|(i, s)| (s.path.clone(), i)).collect();
                    self.song_text = lib.songs.iter().map(|s| norm(&format!("{} {} {} {}", s.title, s.artist, s.album, s.album_artist))).collect();
                    self.album_text = lib.albums.iter().map(|a| norm(&format!("{} {}", a.title, a.artist))).collect();
                    // Keep the open album/artist if they still exist.
                    let old = self.lib.take();
                    let key = |l: &Option<Arc<Library>>, i: Option<usize>| i.and_then(|i| l.as_ref()?.albums.get(i).map(|a| a.key.clone()));
                    let open = key(&old, self.album);
                    self.album = open.and_then(|k| lib.albums.iter().position(|a| a.key == k));
                    let artist = self.artist.and_then(|i| old.as_ref()?.artists.get(i).map(|a| norm(&a.name)));
                    self.artist = artist.and_then(|n| lib.artists.iter().position(|a| norm(&a.name) == n));
                    self.lib = Some(lib);
                    self.covers.clear();
                    self.run_demo();
                }
                Err(e) => self.log.push(format!("Library scan failed: {e}")),
            }
            if std::mem::take(&mut self.rescan_wanted) {
                self.rescan(ctx);
            }
        }
        let s = self.player.status();
        // Volume may have been changed from the desktop's media widget.
        let v = self.player.volume().sqrt();
        if (v - self.volume).abs() > 0.001 && self.volume > 0.0 {
            self.volume = v;
        }
        if s.error != self.last.error {
            if let Some(e) = &s.error {
                self.log.push(format!("Player: {e}"));
            }
        }
        if s.state == State::Playing {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if self.saved_at.elapsed() > Duration::from_secs(10) {
            self.save_session();
        }
        if self.scan.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.last = s;
        std::mem::take(&mut self.log)
    }

    /// Saves volume, queue and position (if they changed).
    pub fn save_session(&mut self) {
        self.saved_at = std::time::Instant::now();
        let s = &self.last;
        let session = Session {
            volume: self.volume,
            queue: self.player.queue(),
            index: s.index.unwrap_or(0),
            position: s.position,
            shuffle: self.shuffle,
            repeat: self.repeat,
        };
        if self.saved.as_ref() == Some(&session) {
            return;
        }
        let path = self.settings.config_dir.join("player.json");
        let tmp = path.with_extension("json.part");
        let ok = serde_json::to_vec(&session)
            .ok()
            .and_then(|b| std::fs::create_dir_all(&self.settings.config_dir).and_then(|_| std::fs::write(&tmp, b)).ok())
            .and_then(|_| std::fs::rename(&tmp, &path).ok());
        if ok.is_some() {
            self.saved = Some(session);
        }
    }

    fn run_demo(&mut self) {
        let (Some(demo), Some(lib)) = (self.demo.take(), self.lib.clone()) else { return };
        let ok = lib.albums.iter().position(|a| a.title == "OK Computer").unwrap_or(0);
        match demo.as_str() {
            "album" => self.album = Some(ok),
            "play" => {
                self.album = Some(ok);
                self.play(lib.albums[ok].songs.clone(), 3);
                self.queue_open = true;
            }
            "search" => self.search = "love".into(),
            "artists" => self.page = Page::Artists,
            "songs" => self.page = Page::Songs,
            "recent" => self.page = Page::Recent,
            _ => {}
        }
    }

    /// Space, arrows, Ctrl+F (when no text field has the keyboard).
    pub fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, left, right, find) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.command && i.key_pressed(egui::Key::F),
            )
        });
        if space {
            self.player.toggle();
        }
        let pos = self.last.position;
        if left {
            self.player.seek((pos - 10.0).max(0.0));
        }
        if right {
            self.player.seek(pos + 10.0);
        }
        if find {
            ctx.memory_mut(|m| m.request_focus(self.search_id));
        }
    }

    fn play(&mut self, songs: Vec<usize>, start: usize) {
        let Some(lib) = &self.lib else { return };
        let playable: Vec<usize> = songs.iter().copied().filter(|&i| !lib.songs[i].drm).collect();
        if playable.is_empty() {
            return;
        }
        let first = songs.get(start).copied().filter(|&i| !lib.songs[i].drm).unwrap_or(playable[0]);
        let mut order = playable.clone();
        let mut at = order.iter().position(|&i| i == first).unwrap_or(0);
        if self.shuffle {
            order.retain(|&i| i != first);
            shuffle(&mut order);
            order.insert(0, first);
            at = 0;
        }
        let paths = order.iter().map(|&i| lib.songs[i].path.clone()).collect();
        self.player.load(paths, at);
    }

    fn run_actions(&mut self) {
        for a in std::mem::take(&mut self.actions) {
            let Some(lib) = self.lib.clone() else { return };
            let paths = |v: &[usize]| v.iter().filter(|&&i| !lib.songs[i].drm).map(|&i| lib.songs[i].path.clone()).collect::<Vec<_>>();
            match a {
                Action::Play(v, start) => self.play(v, start),
                Action::Shuffle(v) => {
                    let was = self.shuffle;
                    self.shuffle = true;
                    let start = (rand_u64() as usize) % v.len().max(1);
                    self.play(v, start);
                    self.shuffle = was;
                }
                Action::PlayNext(v) => {
                    if self.last.state == State::Stopped && self.last.queue_len == 0 {
                        self.play(v, 0);
                    } else {
                        self.player.insert_next(paths(&v));
                    }
                }
                Action::Append(v) => {
                    if self.last.state == State::Stopped && self.last.queue_len == 0 {
                        self.play(v, 0);
                    } else {
                        self.player.append(paths(&v));
                    }
                }
                Action::OpenAlbum(i) => {
                    self.album = Some(i);
                    self.search.clear();
                }
                Action::OpenArtist(i) => {
                    self.artist = Some(i);
                    self.page = Page::Artists;
                    self.album = None;
                    self.search.clear();
                }
                Action::OpenFolder(p) => open_folder(&p),
            }
        }
    }

    /// The song playing now (library index).
    fn now(&self) -> Option<usize> {
        self.last.path.as_ref().and_then(|p| self.by_path.get(p).copied())
    }

    // ------------------------------------------------------------ pages

    /// The page area (central panel contents).
    pub fn page_ui(&mut self, ui: &mut egui::Ui) {
        let Some(lib) = self.lib.clone() else {
            self.loading(ui);
            return;
        };
        if !self.search.trim().is_empty() {
            self.search_page(ui, &lib);
        } else if let Some(a) = self.album.filter(|&a| a < lib.albums.len()) {
            self.album_page(ui, &lib, a);
        } else {
            match self.page {
                Page::Albums => {
                    let mut order: Vec<usize> = (0..lib.albums.len()).collect();
                    match self.sort {
                        AlbumSort::Artist => {}
                        AlbumSort::Title => order.sort_by_key(|&i| localbeat_core::library::sort_name(&lib.albums[i].title)),
                        AlbumSort::Year => order.sort_by_key(|&i| (std::cmp::Reverse(lib.albums[i].year), localbeat_core::library::sort_name(&lib.albums[i].artist))),
                    }
                    let subtitle = format!("{} albums · {} songs", lib.albums.len(), lib.songs.len());
                    let mut sort = self.sort;
                    theme::page_header(ui, "Albums", &subtitle, |ui| {
                        for (s, label) in [(AlbumSort::Year, "Year"), (AlbumSort::Title, "Title"), (AlbumSort::Artist, "Artist")] {
                            if ui.selectable_label(sort == s, label).clicked() {
                                sort = s;
                            }
                        }
                        ui.label(egui::RichText::new("Sort by").color(theme::pal(ui).faint));
                    });
                    self.sort = sort;
                    self.grid(ui, &lib, &order, "albums");
                }
                Page::Recent => {
                    theme::page_header(ui, "Recently added", "Newest arrivals first: fresh rips and new files", |_| {});
                    let order = lib.recently_added(60);
                    self.grid(ui, &lib, &order, "recent");
                }
                Page::Artists => self.artists_page(ui, &lib),
                Page::Songs => self.songs_page(ui, &lib),
            }
        }
        self.run_actions();
    }

    fn loading(&self, ui: &mut egui::Ui) {
        let (done, total) = (self.scan_progress.0.load(Ordering::Relaxed), self.scan_progress.1.load(Ordering::Relaxed));
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            let (rect, _) = ui.allocate_exact_size(vec2(120.0, 120.0), Sense::hover());
            let t = ui.input(|i| i.time);
            theme::disc(ui.painter(), rect.center(), 56.0, Some((t * 1.6) as f32), None, ui.visuals().dark_mode);
            ui.ctx().request_repaint_after(Duration::from_millis(33));
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Reading your library").font(theme::bold(22.0)).color(theme::pal(ui).text));
            if total > 0 {
                ui.label(egui::RichText::new(format!("{done} of {total} files")).color(theme::pal(ui).dim));
            }
        });
    }

    /// A grid of album tiles, only the visible rows drawn.
    fn grid(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, albums: &[usize], id: &str) {
        let gap = 22.0;
        let avail = ui.available_width() - 4.0;
        let cols = ((avail + gap) / (190.0 + gap)).floor().max(1.0) as usize;
        let tile = (avail - gap * (cols - 1) as f32) / cols as f32;
        let row_h = tile + 52.0;
        let rows = albums.len().div_ceil(cols);
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show_rows(ui, row_h, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for c in 0..cols {
                        let Some(&a) = albums.get(r * cols + c) else { break };
                        self.tile(ui, lib, a, tile);
                    }
                });
            }
        });
    }

    fn tile(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, a: usize, size: f32) {
        let album = &lib.albums[a];
        let (rect, resp) = ui.allocate_exact_size(vec2(size, size + 52.0), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let p = theme::pal(ui);
        let cover_rect = Rect::from_min_size(rect.min, Vec2::splat(size));
        let hovered = resp.hovered();
        let lift = if hovered { -3.0 } else { 0.0 };
        let cover_rect = cover_rect.translate(vec2(0.0, lift));
        if hovered {
            let shadow = egui::Shadow { offset: [0, 10], blur: 24, spread: 0, color: Color32::from_black_alpha(120) };
            ui.painter().add(shadow.as_shape(cover_rect, 12));
        }
        self.paint_cover(ui, lib, a, cover_rect, 256);
        let playing_here = self.now().is_some_and(|n| album.songs.contains(&n));
        // Play button on hover (or while this album plays).
        let btn = Rect::from_center_size(cover_rect.right_bottom() - vec2(28.0, 28.0), Vec2::splat(40.0));
        let btn_resp = ui.interact(btn, ui.scope_id().with(("tile-play", a)), Sense::click());
        if hovered || btn_resp.hovered() || playing_here {
            theme::prism_rect(ui.painter(), btn, 20.0, if btn_resp.hovered() { 1.0 } else { 0.92 });
            let playing = playing_here && self.last.state == State::Playing;
            theme::paint_icon(ui.painter(), btn.shrink(11.0), if playing { Icon::Pause } else { Icon::Play }, Color32::WHITE);
        }
        let title = theme::one_line(ui, &album.title, theme::medium(14.0), p.text, size);
        let sub = match album.year {
            Some(y) => format!("{} · {y}", album.artist),
            None => album.artist.clone(),
        };
        let sub = theme::one_line(ui, &sub, egui::FontId::proportional(13.0), p.dim, size);
        let y = cover_rect.bottom() + 8.0 - lift;
        ui.painter().galley(pos2(rect.left(), y), title, p.text);
        ui.painter().galley(pos2(rect.left(), y + 20.0), sub, p.dim);
        if btn_resp.clicked() {
            if playing_here {
                self.player.toggle();
            } else {
                self.actions.push(Action::Play(album.songs.clone(), 0));
            }
        } else if resp.clicked() {
            self.actions.push(Action::OpenAlbum(a));
        }
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let songs = album.songs.clone();
        let dir = album.dirs.first().cloned();
        resp.context_menu(|ui| {
            self.song_menu(ui, songs.clone(), dir.clone());
        });
    }

    /// The cover, or a disc where there's none (or while it loads).
    fn paint_cover(&mut self, ui: &egui::Ui, lib: &Arc<Library>, a: usize, rect: Rect, px: u32) {
        match self.covers.get(lib, a, px) {
            Some(c) => {
                egui::Image::new(&c.tex).corner_radius(rect.width() * 0.055).paint_at(ui, rect);
            }
            None => {
                let p = theme::pal(ui);
                ui.painter().rect_filled(rect, rect.width() * 0.055, p.card);
                theme::disc(ui.painter(), rect.center(), rect.width() * 0.36, None, None, ui.visuals().dark_mode);
            }
        }
    }

    fn song_menu(&mut self, ui: &mut egui::Ui, songs: Vec<usize>, dir: Option<PathBuf>) {
        if ui.button("Play").clicked() {
            self.actions.push(Action::Play(songs.clone(), 0));
            ui.close();
        }
        if ui.button("Play next").clicked() {
            self.actions.push(Action::PlayNext(songs.clone()));
            ui.close();
        }
        if ui.button("Add to Up Next").clicked() {
            self.actions.push(Action::Append(songs.clone()));
            ui.close();
        }
        if let Some(d) = dir {
            ui.separator();
            if ui.button("Show in folder").clicked() {
                self.actions.push(Action::OpenFolder(d));
                ui.close();
            }
        }
    }

    fn album_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, a: usize) {
        let album = &lib.albums[a];
        let p = theme::pal(ui);
        // Wash of the cover's colour behind the header.
        let accent = self.covers.get(lib, a, 256).map(|c| c.accent).unwrap_or(Color32::from_rgb(60, 60, 110));
        let area = ui.clip_rect();
        let wash = Rect::from_min_size(area.min, vec2(area.width(), 420.0));
        let mut mesh = egui::Mesh::default();
        let top = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if ui.visuals().dark_mode { 150 } else { 90 });
        mesh.colored_vertex(wash.left_top(), top);
        mesh.colored_vertex(wash.right_top(), top);
        mesh.colored_vertex(wash.left_bottom(), Color32::TRANSPARENT);
        mesh.colored_vertex(wash.right_bottom(), Color32::TRANSPARENT);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 3, 2);
        ui.painter().add(egui::Shape::mesh(mesh));

        let back = match self.page {
            Page::Recent => "Recently added",
            Page::Artists => "Artists",
            Page::Songs => "Songs",
            Page::Albums => "Albums",
        };
        if ui.add(egui::Label::new(egui::RichText::new(format!("‹  {back}")).color(p.dim)).sense(Sense::click())).clicked() {
            self.album = None;
        }
        ui.add_space(8.0);
        let songs: Vec<&Song> = album.songs.iter().map(|&i| &lib.songs[i]).collect();
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(232.0), Sense::hover());
            let shadow = egui::Shadow { offset: [0, 16], blur: 40, spread: 0, color: Color32::from_black_alpha(150) };
            ui.painter().add(shadow.as_shape(rect, 14));
            self.paint_cover(ui, lib, a, rect, 512);
            ui.add_space(18.0);
            ui.vertical(|ui| {
                ui.add_space(26.0);
                ui.horizontal(|ui| {
                    if album.origin == Origin::Flac {
                        theme::badge(ui, "CD RIP", theme::OK).on_hover_text("Ripped and verified with LocalBeat Studio");
                    }
                    if let Some(f) = format_label(&songs) {
                        theme::badge(ui, &f, if songs.iter().all(|s| s.lossless()) { theme::prism_at(0.1) } else { p.dim });
                    }
                });
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&album.title).font(theme::bold(34.0)).color(p.text));
                let artist = ui.add(egui::Label::new(egui::RichText::new(&album.artist).font(theme::medium(19.0)).color(theme::prism_at(0.3))).sense(Sense::click()));
                if artist.clicked() {
                    if let Some(i) = lib.artists.iter().position(|x| x.albums.contains(&a)) {
                        self.actions.push(Action::OpenArtist(i));
                    }
                }
                if artist.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let mut meta = Vec::new();
                if let Some(y) = album.year {
                    meta.push(y.to_string());
                }
                if let Some(g) = &album.genre {
                    meta.push(g.clone());
                }
                meta.push(format!("{} songs, {}", album.songs.len(), long_duration(album.seconds)));
                ui.label(egui::RichText::new(meta.join(" · ")).color(p.dim));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Some(Icon::Play), "Play", Kind::Primary).clicked() {
                        self.actions.push(Action::Play(album.songs.clone(), 0));
                    }
                    if theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                        self.actions.push(Action::Shuffle(album.songs.clone()));
                    }
                    if theme::button(ui, Some(Icon::Plus), "Up Next", Kind::Secondary).on_hover_text("Add the album to the end of Up Next").clicked() {
                        self.actions.push(Action::Append(album.songs.clone()));
                    }
                    if let Some(d) = album.dirs.first() {
                        if theme::icon_button(ui, Icon::Folder, 32.0, "Show in folder").clicked() {
                            self.actions.push(Action::OpenFolder(d.clone()));
                        }
                    }
                });
            });
        });
        ui.add_space(18.0);
        let now = self.now();
        let time = ui.input(|i| i.time);
        let playing = self.last.state == State::Playing;
        egui::ScrollArea::vertical().id_salt(("album", a)).auto_shrink([false, false]).show(ui, |ui| {
            let mut disc = None;
            for (k, &si) in album.songs.iter().enumerate() {
                let s = &lib.songs[si];
                if album.discs > 1 && s.disc != disc {
                    disc = s.disc;
                    ui.add_space(8.0);
                    theme::section_label(ui, &format!("Disc {}", s.disc.unwrap_or(1)));
                }
                let show_artist = !s.artist.is_empty() && norm(&s.artist) != norm(&album.artist);
                if let Some(click) = song_row(ui, s, k + 1, show_artist, now == Some(si), playing, time, None) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(album.songs.clone(), k)),
                        RowClick::Menu(m) => self.actions.push(match m {
                            MenuChoice::PlayNext => Action::PlayNext(vec![si]),
                            MenuChoice::Append => Action::Append(vec![si]),
                            MenuChoice::Folder => Action::OpenFolder(s.path.parent().map(|d| d.to_path_buf()).unwrap_or_default()),
                        }),
                    }
                }
            }
            ui.add_space(14.0);
            if let Some(d) = album.dirs.first() {
                ui.label(egui::RichText::new(d.display().to_string()).small().color(p.faint));
            }
            ui.add_space(20.0);
        });
    }

    fn artists_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let p = theme::pal(ui);
        if self.artist.is_none_or(|i| i >= lib.artists.len()) {
            self.artist = (!lib.artists.is_empty()).then_some(0);
        }
        let sel = self.artist;
        egui::Panel::left("artist-list")
            .resizable(false)
            .exact_size(250.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 14, top: 0, bottom: 0 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                theme::section_label(ui, &format!("{} artists", lib.artists.len()));
                ui.add_space(4.0);
                egui::ScrollArea::vertical().id_salt("artists").auto_shrink([false, false]).show_rows(ui, 30.0, lib.artists.len(), |ui, range| {
                    for i in range {
                        let a = &lib.artists[i];
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                        let selected = sel == Some(i);
                        if selected {
                            ui.painter().rect_filled(rect, 8.0, p.card_hover);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 8.0, p.card);
                        }
                        let g = theme::one_line(ui, &a.name, if selected { theme::medium(14.0) } else { egui::FontId::proportional(14.0) }, if selected { p.text } else { p.dim }, rect.width() - 46.0);
                        ui.painter().galley(pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, p.text);
                        ui.painter().text(rect.right_center() - vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, a.albums.len().to_string(), egui::FontId::proportional(12.0), p.faint);
                        if resp.clicked() {
                            self.artist = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = self.artist {
            let a = &lib.artists[i];
            let songs: Vec<usize> = a.albums.iter().flat_map(|&al| lib.albums[al].songs.iter().copied()).collect();
            let subtitle = format!("{} albums · {} songs", a.albums.len(), songs.len());
            theme::page_header(ui, &a.name, &subtitle, |ui| {
                if theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                    self.actions.push(Action::Shuffle(songs.clone()));
                }
                if theme::button(ui, Some(Icon::Play), "Play all", Kind::Primary).clicked() {
                    self.actions.push(Action::Play(songs.clone(), 0));
                }
            });
            let albums = a.albums.clone();
            self.grid(ui, lib, &albums, "artist-albums");
        }
    }

    fn songs_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let order: Vec<usize> = lib.albums.iter().flat_map(|a| a.songs.iter().copied()).collect();
        let subtitle = format!("{} songs · {}", order.len(), long_duration(lib.songs.iter().map(|s| s.seconds).sum()));
        theme::page_header(ui, "Songs", &subtitle, |ui| {
            if theme::button(ui, Some(Icon::Shuffle), "Shuffle all", Kind::Primary).clicked() {
                self.actions.push(Action::Shuffle(order.clone()));
            }
        });
        self.song_list(ui, lib, &order, "songs");
    }

    /// A long list of songs with album/artist columns, virtualised.
    fn song_list(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, order: &[usize], id: &str) {
        let now = self.now();
        let time = ui.input(|i| i.time);
        let playing = self.last.state == State::Playing;
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show_rows(ui, 38.0, order.len(), |ui, range| {
            for k in range {
                let si = order[k];
                let s = &lib.songs[si];
                let album = if s.album.is_empty() { None } else { Some(s.album.as_str()) };
                if let Some(click) = song_row(ui, s, 0, true, now == Some(si), playing, time, album) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(order.to_vec(), k)),
                        RowClick::Menu(m) => self.actions.push(match m {
                            MenuChoice::PlayNext => Action::PlayNext(vec![si]),
                            MenuChoice::Append => Action::Append(vec![si]),
                            MenuChoice::Folder => Action::OpenFolder(s.path.parent().map(|d| d.to_path_buf()).unwrap_or_default()),
                        }),
                    }
                }
            }
        });
    }

    fn search_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let p = theme::pal(ui);
        let words: Vec<String> = norm(&self.search).split(' ').map(str::to_string).collect();
        let hit = |t: &str| words.iter().all(|w| t.contains(w.as_str()));
        let artists: Vec<usize> = (0..lib.artists.len()).filter(|&i| hit(&norm(&lib.artists[i].name))).take(12).collect();
        let albums: Vec<usize> = (0..lib.albums.len()).filter(|&i| hit(&self.album_text[i])).take(60).collect();
        let songs: Vec<usize> = lib.albums.iter().flat_map(|a| a.songs.iter().copied()).filter(|&i| hit(&self.song_text[i])).take(300).collect();
        let subtitle = format!("{} artists · {} albums · {} songs", artists.len(), albums.len(), songs.len());
        theme::page_header(ui, &format!("“{}”", self.search.trim()), &subtitle, |_| {});
        if artists.is_empty() && albums.is_empty() && songs.is_empty() {
            ui.label(egui::RichText::new("Nothing matches.").color(p.dim));
            return;
        }
        if !artists.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for &i in &artists {
                    if theme::button(ui, Some(Icon::Artists), &lib.artists[i].name, Kind::Secondary).clicked() {
                        self.actions.push(Action::OpenArtist(i));
                    }
                }
            });
            ui.add_space(10.0);
        }
        // Albums in a short strip, songs below.
        if !albums.is_empty() {
            theme::section_label(ui, "Albums");
            let h = 246.0;
            egui::ScrollArea::horizontal().id_salt("search-albums").max_height(h).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 18.0;
                    for &a in &albums {
                        self.tile(ui, lib, a, 180.0);
                    }
                });
            });
            ui.add_space(8.0);
        }
        if !songs.is_empty() {
            theme::section_label(ui, "Songs");
            self.song_list(ui, lib, &songs, "search-songs");
        }
    }

    // ------------------------------------------------------------ now playing

    /// The bar at the bottom of every page.
    pub fn now_playing(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let s = self.last.clone();
        // Below where the panels above end (see bar_top), down to the bottom.
        let clip = ui.clip_rect();
        let top = self.bar_top.unwrap_or(clip.top()).clamp(clip.top(), clip.bottom() - 40.0);
        let outer = Rect::from_min_max(pos2(clip.left(), top), clip.max);
        ui.allocate_rect(Rect::from_min_max(ui.max_rect().min, pos2(ui.max_rect().right(), clip.bottom())), Sense::hover());
        ui.painter().line_segment([outer.left_top(), outer.right_top()], egui::Stroke::new(1.0, p.line));
        let full = outer.shrink2(vec2(16.0, 10.0));
        let lib = self.lib.clone();
        let song = self.now().and_then(|i| Some((i, lib.as_ref()?.songs.get(i)?.clone())));
        // Left: cover and names.
        let left = Rect::from_min_size(full.min, vec2(320.0, full.height()));
        ui.scope_builder(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::click());
            let album = song.as_ref().and_then(|(i, _)| lib.as_ref()?.albums.iter().position(|a| a.songs.contains(i)));
            match (album, &lib) {
                (Some(a), Some(l)) => self.paint_cover(ui, l, a, rect, 256),
                _ => {
                    ui.painter().rect_filled(rect, 8.0, p.card);
                    theme::disc(ui.painter(), rect.center(), 20.0, None, None, ui.visuals().dark_mode);
                }
            }
            if resp.clicked() {
                if let Some(a) = album {
                    self.actions.push(Action::OpenAlbum(a));
                }
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                let w = ui.available_width() - 8.0;
                match &song {
                    Some((_, x)) => {
                        let t = theme::one_line(ui, &x.title, theme::medium(14.5), p.text, w);
                        ui.label(t);
                        let artist = if x.artist.is_empty() { x.album_artist.clone() } else { x.artist.clone() };
                        let sub = if x.album.is_empty() { artist } else { format!("{artist} — {}", x.album) };
                        ui.label(theme::one_line(ui, &sub, egui::FontId::proportional(13.0), p.dim, w));
                    }
                    None => {
                        ui.label(egui::RichText::new("Nothing playing").color(p.dim));
                        ui.label(egui::RichText::new("Pick an album and press play").small().color(p.faint));
                    }
                }
            });
        });
        // Right: format, volume, queue.
        let right = Rect::from_min_max(pos2(full.right() - 300.0, full.top()), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
            let q = theme::icon_button(ui, Icon::Queue, 34.0, "Up Next");
            if self.queue_open {
                ui.painter().circle_stroke(q.rect.center(), 16.0, egui::Stroke::new(1.5, theme::ACCENT));
            }
            if q.clicked() {
                self.queue_open = !self.queue_open;
            }
            ui.add_space(6.0);
            if let Some(v) = theme::mini_slider(ui, 96.0, self.volume) {
                self.volume = v;
                self.player.set_volume(v * v);
            }
            let icon = if self.volume == 0.0 { Icon::Mute } else { Icon::Volume };
            if theme::icon_button(ui, icon, 30.0, "Mute / unmute").clicked() {
                self.volume = if self.volume == 0.0 { 0.8 } else { 0.0 };
                self.player.set_volume(self.volume * self.volume);
            }
            ui.add_space(10.0);
            if let Some(f) = &s.format {
                let text = match f.bits {
                    Some(b) => format!("{} {}/{}", f.codec, b, khz(f.sample_rate)),
                    None => format!("{} {}", f.codec, khz(f.sample_rate)),
                };
                theme::badge(ui, &text, if f.lossless { theme::prism_at(0.1) } else { p.dim })
                    .on_hover_text(if f.lossless { "Lossless: exactly what's on the CD" } else { "Lossy (compressed) file" });
            }
        });
        // Centre: transport and seek bar.
        let mid_w = (full.width() - 320.0 - 300.0).clamp(240.0, 640.0);
        let mid = Rect::from_center_size(full.center(), vec2(mid_w, full.height()));
        ui.scope_builder(egui::UiBuilder::new().max_rect(mid).layout(egui::Layout::top_down(egui::Align::Center)), |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let row_w = 34.0 * 4.0 + 46.0 + 8.0 * 4.0;
                ui.add_space((ui.available_width() - row_w) / 2.0);
                ui.spacing_mut().item_spacing.x = 8.0;
                let sh = theme::icon_button(ui, Icon::Shuffle, 34.0, "Shuffle (when you next play an album or list)");
                if self.shuffle {
                    ui.painter().circle_filled(sh.rect.center_bottom() + vec2(0.0, -2.0), 2.0, theme::prism_at(0.3));
                }
                if sh.clicked() {
                    self.shuffle = !self.shuffle;
                }
                if theme::icon_button(ui, Icon::Prev, 34.0, "Previous (or start of the song)").clicked() {
                    self.player.prev();
                }
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::click());
                theme::prism_rect(ui.painter(), rect, 23.0, if resp.hovered() { 1.0 } else { 0.9 });
                let icon = if s.state == State::Playing { Icon::Pause } else { Icon::Play };
                theme::paint_icon(ui.painter(), rect.shrink(14.0), icon, Color32::WHITE);
                if resp.clicked() {
                    if s.queue_len == 0 {
                        // Nothing queued: play the album on screen, else everything.
                        if let (Some(a), Some(l)) = (self.album, &lib) {
                            self.actions.push(Action::Play(l.albums[a].songs.clone(), 0));
                        }
                    } else {
                        self.player.toggle();
                    }
                }
                if theme::icon_button(ui, Icon::Next, 34.0, "Next").clicked() {
                    self.player.next();
                }
                let rp = theme::icon_button(ui, Icon::Repeat, 34.0, "Repeat the queue");
                if self.repeat {
                    ui.painter().circle_filled(rp.rect.center_bottom() + vec2(0.0, -2.0), 2.0, theme::prism_at(0.3));
                }
                if rp.clicked() {
                    self.repeat = !self.repeat;
                    self.player.set_repeat(self.repeat);
                }
            });
            ui.horizontal(|ui| {
                let dur = s.duration.unwrap_or(0.0);
                let pos = self.seek_preview.map(|f| f as f64 * dur).unwrap_or(s.position);
                let label = |t: f64| egui::RichText::new(mmss(t)).font(egui::FontId::monospace(12.0)).color(p.dim);
                ui.label(label(pos));
                let w = ui.available_width() - 48.0;
                let frac = if dur > 0.0 { (s.position / dur) as f32 } else { 0.0 };
                if let Some(f) = theme::seek_bar(ui, w, frac, &mut self.seek_preview) {
                    if dur > 0.0 {
                        self.player.seek(f as f64 * dur);
                    }
                }
                ui.label(label(dur));
            });
        });
        self.run_actions();
    }

    /// The Up Next panel (right side).
    pub fn queue_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Up Next").font(theme::bold(20.0)).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, Icon::Cross, 28.0, "Close").clicked() {
                    self.queue_open = false;
                }
            });
        });
        ui.add_space(6.0);
        let queue = self.player.queue();
        let Some(lib) = self.lib.clone() else { return };
        if queue.is_empty() {
            ui.label(egui::RichText::new("Nothing queued. Play an album, or right-click one and choose Add to Up Next.").color(p.dim));
            return;
        }
        let cur = self.last.index.unwrap_or(0);
        let mut jump = None;
        egui::ScrollArea::vertical().id_salt("queue").auto_shrink([false, false]).show(ui, |ui| {
            for (i, path) in queue.iter().enumerate().skip(cur) {
                if i == cur {
                    theme::section_label(ui, "Now playing");
                } else if i == cur + 1 {
                    ui.add_space(8.0);
                    theme::section_label(ui, "Next");
                }
                let (title, artist) = match self.by_path.get(path) {
                    Some(&si) => (lib.songs[si].title.clone(), lib.songs[si].artist.clone()),
                    None => (path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), String::new()),
                };
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
                if i == cur {
                    ui.painter().rect_filled(rect, 8.0, p.card_hover);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 8.0, p.card);
                }
                let w = rect.width() - 20.0;
                let t = theme::one_line(ui, &title, if i == cur { theme::medium(14.0) } else { egui::FontId::proportional(14.0) }, p.text, w);
                ui.painter().galley(rect.left_top() + vec2(10.0, 4.0), t, p.text);
                let a = theme::one_line(ui, &artist, egui::FontId::proportional(12.5), p.dim, w);
                ui.painter().galley(rect.left_top() + vec2(10.0, 23.0), a, p.dim);
                if resp.clicked() && i != cur {
                    jump = Some(i);
                }
                if i > cur + 300 {
                    ui.label(egui::RichText::new(format!("and {} more", queue.len() - i)).color(p.faint));
                    break;
                }
            }
        });
        if let Some(i) = jump {
            self.player.jump(i);
        }
    }
}

enum MenuChoice {
    PlayNext,
    Append,
    Folder,
}

enum RowClick {
    Play,
    Menu(MenuChoice),
}

/// One song in a list: number (or the playing bars), title, artist,
/// album, length. Click plays; right-click for more.
#[allow(clippy::too_many_arguments)]
fn song_row(ui: &mut egui::Ui, s: &Song, number: usize, show_artist: bool, is_now: bool, playing: bool, time: f64, album: Option<&str>) -> Option<RowClick> {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return None;
    }
    if is_now {
        ui.painter().rect_filled(rect, 8.0, p.card_hover);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 8.0, p.card);
    }
    let y = rect.center().y;
    let num_rect = Rect::from_center_size(pos2(rect.left() + 22.0, y), vec2(18.0, 14.0));
    if is_now {
        theme::equalizer(ui.painter(), num_rect, time, playing);
        if playing {
            ui.ctx().request_repaint_after(Duration::from_millis(80));
        }
    } else if resp.hovered() && !s.drm {
        theme::paint_icon(ui.painter(), num_rect.expand(2.0), Icon::Play, p.text);
    } else if number > 0 {
        ui.painter().text(num_rect.center(), egui::Align2::CENTER_CENTER, number.to_string(), egui::FontId::monospace(12.5), p.faint);
    } else {
        theme::paint_icon(ui.painter(), num_rect.shrink(1.0), Icon::Songs, p.faint);
    }
    let text_left = rect.left() + 46.0;
    let right_w = 150.0;
    let avail = rect.width() - 46.0 - right_w;
    let (title_w, rest_w) = if album.is_some() { (avail * 0.42, avail * 0.58) } else { (avail * 0.6, avail * 0.4) };
    let title_color = if s.drm { p.faint } else if is_now { theme::prism_at(0.3) } else { p.text };
    let t = theme::one_line(ui, &s.title, if is_now { theme::medium(14.5) } else { egui::FontId::proportional(14.5) }, title_color, title_w - 12.0);
    ui.painter().galley(pos2(text_left, y - t.size().y / 2.0), t, title_color);
    let mut x = text_left + title_w;
    if show_artist && !s.artist.is_empty() {
        let w = if album.is_some() { rest_w * 0.45 } else { rest_w };
        let a = theme::one_line(ui, &s.artist, egui::FontId::proportional(13.5), p.dim, w - 12.0);
        ui.painter().galley(pos2(x, y - a.size().y / 2.0), a, p.dim);
        x += w;
    }
    if let Some(al) = album {
        let w = rest_w * 0.55;
        let a = theme::one_line(ui, al, egui::FontId::proportional(13.5), p.faint, w - 12.0);
        ui.painter().galley(pos2(x, y - a.size().y / 2.0), a, p.faint);
    }
    // Right: lock / format, length.
    ui.painter().text(rect.right_center() - vec2(12.0, 0.0), egui::Align2::RIGHT_CENTER, mmss(s.seconds), egui::FontId::monospace(12.5), p.dim);
    if s.drm {
        theme::paint_icon(ui.painter(), Rect::from_center_size(pos2(rect.right() - 80.0, y), Vec2::splat(13.0)), Icon::Lock, theme::WARN);
    } else if album.is_some() {
        ui.painter().text(pos2(rect.right() - 70.0, y), egui::Align2::RIGHT_CENTER, &s.codec, theme::medium(11.0), if s.lossless() { theme::prism_at(0.1) } else { p.faint });
    }
    let mut out = None;
    if resp.clicked() || resp.double_clicked() {
        if !s.drm {
            out = Some(RowClick::Play);
        }
    }
    let resp = if s.drm { resp.on_hover_text("Protected by iTunes DRM: can't be played") } else { resp };
    resp.context_menu(|ui| {
        if ui.add_enabled(!s.drm, egui::Button::new("Play next")).clicked() {
            out = Some(RowClick::Menu(MenuChoice::PlayNext));
            ui.close();
        }
        if ui.add_enabled(!s.drm, egui::Button::new("Add to Up Next")).clicked() {
            out = Some(RowClick::Menu(MenuChoice::Append));
            ui.close();
        }
        ui.separator();
        if ui.button("Show in folder").clicked() {
            out = Some(RowClick::Menu(MenuChoice::Folder));
            ui.close();
        }
    });
    out
}

/// "FLAC 16/44.1" for an album whose songs share one format.
fn format_label(songs: &[&Song]) -> Option<String> {
    let first = songs.iter().find(|s| !s.drm)?;
    let same = songs.iter().filter(|s| !s.drm).all(|s| s.codec == first.codec && s.sample_rate == first.sample_rate && s.bits == first.bits);
    if !same {
        return Some("Mixed formats".into());
    }
    let rate = first.sample_rate.map(khz).unwrap_or_default();
    Some(match (first.bits, first.lossless()) {
        (Some(b), true) => format!("{} {b}/{rate}", first.codec),
        _ => match first.bitrate {
            Some(k) => format!("{} {k} kbps", first.codec),
            None => first.codec.clone(),
        },
    })
}

fn khz(rate: u32) -> String {
    let k = rate as f64 / 1000.0;
    if k.fract() == 0.0 { format!("{k:.0}") } else { format!("{k:.1}") }
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn long_duration(s: f64) -> String {
    let m = (s / 60.0).round() as u64;
    if m >= 120 { format!("{} hours", m / 60) } else if m >= 60 { format!("{} h {} min", m / 60, m % 60) } else { format!("{m} min") }
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish()
}

fn shuffle<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = (rand_u64() as usize) % (i + 1);
        v.swap(i, j);
    }
}

fn open_folder(p: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(not(target_os = "windows"))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}
