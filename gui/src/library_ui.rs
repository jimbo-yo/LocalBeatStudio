//! The music side of the app: the library (Albums, Recently added, Artists,
//! Songs, search, album pages), the now-playing bar on every page, and the
//! queue. Playback is localbeat-player; the library comes from
//! core::library (scanned in the background, cached).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use eframe::egui::{self, Color32, Rect, Sense, Vec2, pos2, vec2};
use localbeat_core::config::Settings;
use localbeat_core::library::{Library, Origin, ScanStats, Song, norm};
use localbeat_core::server_library::{self, Catalogue, Fetcher};
use localbeat_engine::remove;
use localbeat_player::{Player, Repeat, State, Status};

use crate::cover_cache::CoverCache;
use crate::media_keys::SharedLibrary;
use crate::theme::{self, Icon, Kind};

mod explore;
mod nav;
pub use explore::{GroupKind, GroupView};
pub use nav::NavView;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    /// Shuffle all, Random album, Recently added, Moods, Genres, Decades.
    Explore,
    Albums,
    Artists,
    Songs,
    /// The user's playlists on the music server.
    Playlists,
}

/// How often the music server's catalogue is fetched again while connected
/// (new rips show without a rescan; the phone does the same).
const CATALOGUE_REFRESH: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How long to wait before asking a music server that didn't answer again.
const SERVER_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// LOCALBEAT_DEMO=playlist opens the first playlist once the list is fetched.
const DEMO_FIRST_PLAYLIST: &str = "\u{0}first";

/// The name box for a new playlist or a renamed one.
struct NameDialog {
    title: &'static str,
    text: String,
    /// New: these songs (library indices) go in it. Rename: this playlist.
    new_with: Option<Vec<usize>>,
    rename: Option<String>,
}

/// The Now Playing view's right-hand tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NowTab {
    Queue,
    Details,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlbumSort {
    Artist,
    Title,
    /// Original release year, newest first.
    Year,
    /// Newest arrival first.
    Recent,
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
    /// A genre, mood or decade page.
    OpenGroup(GroupView),
    OpenFolder(PathBuf),
    /// Add these songs to the playlist with this ID.
    AddToPlaylist(String, Vec<usize>),
    /// Ask for a name, then make a playlist of these songs.
    NewPlaylist(Vec<usize>),
    /// In the open playlist: take out the song at this position, or move it by this much.
    PlaylistRemove(usize),
    PlaylistMove(usize, isize),
    /// Ask, then take these songs out of the library (server and here).
    AskRemove(Vec<usize>),
}

/// "Remove from library": asked, then done in the background.
struct RemoveDialog {
    /// Library indices.
    songs: Vec<usize>,
    /// "“Revolver” by The Beatles", "“Taxman”", "3 songs".
    what: String,
    plan: remove::Plan,
    /// This computer has a writer key (an admin computer).
    server_ok: bool,
    state: RemoveState,
    /// Offered after a re-rip reached the server: what the old copy is
    /// (said above the usual lines, with "Remove old copy" / "Keep it").
    old_copy: Option<String>,
}

enum RemoveState {
    Confirm,
    Working(Receiver<Result<remove::Report, String>>),
    Done(Result<remove::Report, String>),
}

/// What's kept between runs (`<config>/player.json`).
#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq)]
struct Session {
    volume: f32,
    queue: Vec<PathBuf>,
    index: usize,
    position: f64,
    shuffle: bool,
    /// Repeat the queue (as saved before Repeat one existed).
    repeat: bool,
    #[serde(default)]
    repeat_one: bool,
}

/// The LocalBeat server, as the library sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ServerView {
    /// No music server login on this computer.
    #[default]
    Off,
    /// Its albums are in the library (`songs` of them).
    Connected { songs: usize },
    /// Not reachable (away from home without the tunnel, or it's off):
    /// only what's on this computer shows.
    NotConnected(String),
}

/// A library to show, and what the server said. A scan can send more than
/// one: first with the server's catalogue as kept on disk, then again if
/// the server's own has changed.
type ScanResult = Result<(Arc<Library>, ScanStats, ServerView), String>;

pub struct LibraryUi {
    settings: Settings,
    /// Genres, moods and decades of `lib` (keyed by its address).
    browse: Option<(usize, Arc<localbeat_core::browse::Browse>)>,
    /// Unified Back/Forward navigation history for the LIBRARY section.
    nav: nav::NavHistory,
    /// Explore's genres by size (else A–Z).
    genres_by_size: bool,
    /// While the music server isn't connected: when to look again. A slow
    /// first answer (the tunnel waking up) left the library without the
    /// server's albums until a restart.
    server_retry: Option<std::time::Instant>,
    /// The scan running is such a retry (its "Library:" line isn't logged).
    retrying: bool,
    /// While connected: when to fetch the server's catalogue again.
    catalogue_check: Option<std::time::Instant>,
    pub lib: Option<Arc<Library>>,
    by_path: HashMap<PathBuf, usize>,
    /// Lower-case "title artist album" per song, and "title artist" per album.
    song_text: Vec<String>,
    album_text: Vec<String>,
    scan: Option<Receiver<ScanResult>>,
    scan_progress: Arc<(AtomicUsize, AtomicUsize)>,
    rescan_wanted: bool,
    pub server: ServerView,
    /// Downloads the server songs about to play.
    fetcher: Fetcher,
    /// What it was last asked for.
    wanted: Vec<PathBuf>,
    last_fetch_error: Option<(PathBuf, String)>,
    pub covers: CoverCache,
    pub player: Arc<Player>,
    /// The library for the media-keys thread.
    shared_lib: SharedLibrary,
    pub search: String,
    pub search_id: egui::Id,
    pub queue_open: bool,
    now_tab: NowTab,
    /// Extra tags of the song in Details, read when it changes.
    details: Option<(PathBuf, Vec<(String, String)>)>,
    /// Scroll the queue to the current song once after opening.
    scroll_to_now: bool,
    shuffle: bool,
    repeat: Repeat,
    /// The queue as it was before Shuffle was turned on (to put it back).
    unshuffled: Option<Vec<PathBuf>>,
    /// The Queue panel row being dragged.
    queue_drag: Option<usize>,
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
    /// The music server's playlists (fetched in the background).
    pub playlists: crate::playlists_ui::Playlists,
    name_dialog: Option<NameDialog>,
    /// A playlist waiting for "delete?" to be confirmed.
    confirm_delete: Option<String>,
    remove_dialog: Option<RemoveDialog>,
    /// Albums just sent to the server whose old copies are still to be
    /// looked for (outbox names, "Artist/Album").
    old_copy_checks: Vec<String>,
    /// For starting background work from actions.
    ctx: Option<egui::Context>,
    /// The bottom bar asked for Now Playing: main.rs switches to the Library tab.
    pub show_library: bool,
}

impl LibraryUi {
    pub fn new(settings: &Settings, ctx: &egui::Context, hwnd: Option<crate::media_keys::Hwnd>) -> Self {
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
        let repeat = match (session.repeat_one, session.repeat) {
            (true, _) => Repeat::One,
            (false, true) => Repeat::All,
            _ => Repeat::Off,
        };
        player.set_repeat(repeat);
        let shared_lib: SharedLibrary = Default::default();
        let media_err = crate::media_keys::spawn(player.clone(), shared_lib.clone(), settings.cache_dir.clone(), ctx.clone(), hwnd).err();
        let mut me = LibraryUi {
            settings: settings.clone(),
            lib: None,
            by_path: HashMap::new(),
            song_text: Vec::new(),
            album_text: Vec::new(),
            scan: None,
            scan_progress: Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))),
            rescan_wanted: false,
            server: ServerView::Off,
            fetcher: Fetcher::start(settings.clone()),
            wanted: Vec::new(),
            last_fetch_error: None,
            covers: CoverCache::new(settings.clone(), ctx.clone()),
            player,
            shared_lib,
            browse: None,
            nav: nav::NavHistory::default(),
            genres_by_size: true,
            server_retry: None,
            retrying: false,
            catalogue_check: None,
            search: String::new(),
            search_id: egui::Id::unique("library-search"),
            queue_open: false,
            now_tab: NowTab::Queue,
            details: None,
            scroll_to_now: false,
            shuffle: session.shuffle,
            repeat,
            unshuffled: None,
            queue_drag: None,
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
            playlists: crate::playlists_ui::Playlists::new(settings),
            name_dialog: None,
            confirm_delete: None,
            remove_dialog: None,
            old_copy_checks: Vec::new(),
            ctx: None,
            show_library: false,
        };
        me.rescan(ctx);
        me
    }

    // ------------------------------------------------------------ navigation

    pub fn is_now_open(&self) -> bool {
        matches!(self.nav.current(), NavView::NowPlaying)
    }

    pub fn sidebar_page(&self) -> Option<Page> {
        self.nav.current().sidebar_page()
    }

    pub fn open_page(&mut self, page: Page) {
        self.push_nav(NavView::from_page(page));
    }

    /// Opens the full-size Now Playing view.
    pub fn open_now(&mut self) {
        self.scroll_to_now = true;
        self.push_nav(NavView::NowPlaying);
    }

    pub(super) fn push_nav(&mut self, view: NavView) {
        if !matches!(view, NavView::Search(_)) {
            self.search.clear();
        }
        self.nav.push(view);
    }

    pub fn go_back(&mut self) {
        if self.nav.back() {
            self.sync_search_with_nav();
        }
    }

    pub fn go_forward(&mut self) {
        if self.nav.forward() {
            self.sync_search_with_nav();
        }
    }

    fn sync_search_with_nav(&mut self) {
        match self.nav.current() {
            NavView::Search(q) => self.search = q.clone(),
            _ => self.search.clear(),
        }
    }

    /// Called whenever the sidebar search text changes.
    pub fn on_search_changed(&mut self) {
        let q = self.search.trim().to_string();
        if q.is_empty() {
            if matches!(self.nav.current(), NavView::Search(_)) {
                if !self.nav.back() {
                    self.nav.replace_current(NavView::Explore);
                }
                self.sync_search_with_nav();
            }
        } else if matches!(self.nav.current(), NavView::Search(_)) {
            self.nav.replace_current(NavView::Search(q));
        } else {
            self.nav.push(NavView::Search(q));
        }
    }

    pub fn clear_search(&mut self) {
        self.search.clear();
        self.on_search_changed();
    }

    fn open_playlist_id(&self) -> Option<String> {
        match self.nav.current() {
            NavView::Playlist(id) => Some(id.clone()),
            _ => None,
        }
    }

    fn resolve_album(lib: &Library, artist: &str, title: &str) -> Option<usize> {
        lib.albums.iter().position(|a| norm(&a.artist) == artist && norm(&a.title) == title)
    }

    fn resolve_artist(lib: &Library, key: &str) -> Option<usize> {
        lib.artists.iter().position(|a| norm(&a.name) == key)
    }

    /// Vertical scroll area that restores the entry's saved scroll offset once after Back/Forward.
    pub(super) fn scroll_area(&mut self, id: impl std::hash::Hash + std::fmt::Debug) -> egui::ScrollArea {
        let mut sa = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
        if let Some(y) = self.nav.take_restore_scroll() {
            sa = sa.vertical_scroll_offset(y);
        }
        sa
    }

    /// Fixed top-left Back / Forward buttons + mouse Extra1/Extra2 handling for LIBRARY pages.
    fn nav_bar(&mut self, ui: &mut egui::Ui) {
        let (mouse_back, mouse_fwd) = ui.input(|i| (i.pointer.button_clicked(egui::PointerButton::Extra1), i.pointer.button_clicked(egui::PointerButton::Extra2)));
        if mouse_back {
            self.go_back();
        } else if mouse_fwd {
            self.go_forward();
        }
        let can_back = self.nav.can_back();
        let can_fwd = self.nav.can_forward();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let b = ui.add_enabled_ui(can_back, |ui| theme::icon_button(ui, Icon::Back, 28.0, "Back (Alt+Left)")).inner;
            if can_back && b.clicked() {
                self.go_back();
            }
            let f = ui.add_enabled_ui(can_fwd, |ui| theme::icon_button(ui, Icon::Forward, 28.0, "Forward (Alt+Right)")).inner;
            if can_fwd && f.clicked() {
                self.go_forward();
            }
        });
        ui.add_space(4.0);
    }

    /// Fetches the music server's catalogue again without rescanning this
    /// computer's folders. Only a changed catalogue becomes a new library
    /// (so covers and the open page stay put, and the log stays quiet);
    /// a failed fetch waits for the next turn.
    fn refresh_catalogue(&mut self, ctx: &egui::Context) {
        let Some(lib) = self.lib.clone() else { return };
        let (tx, rx) = channel();
        let (settings, ctx, shared) = (self.settings.clone(), ctx.clone(), self.shared_lib.clone());
        std::thread::spawn(move || {
            let Some((sub, url)) = server_library::client(&settings) else { return };
            let Ok(fresh) = Catalogue::fetch(&settings, &sub, &url) else { return };
            if Catalogue::load(&settings, &url).is_some_and(|k| k.same_songs(&fresh)) {
                return;
            }
            let _ = fresh.save(&settings);
            let songs: Vec<Song> = lib.songs.iter().filter(|s| s.origin != Origin::Server).cloned().chain(fresh.songs.iter().cloned()).collect();
            let mut st = ScanStats::default();
            let new = Arc::new(Library::build(songs, &mut st));
            *shared.lock().unwrap() = Some(new.clone());
            let _ = tx.send(Ok((new, st, ServerView::Connected { songs: fresh.songs.len() })));
            ctx.request_repaint();
        });
        self.scan = Some(rx);
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
            let send = |r: ScanResult| {
                // The media-keys thread gets it now, even if the window is hidden.
                if let Ok((lib, _, _)) = &r {
                    *shared.lock().unwrap() = Some(lib.clone());
                }
                let _ = tx.send(r);
                ctx.request_repaint();
            };
            let (lib, st) = match Library::scan(&settings, &|done, total| {
                progress.0.store(done, Ordering::Relaxed);
                progress.1.store(total, Ordering::Relaxed);
            }) {
                Ok(x) => x,
                Err(e) => return send(Err(format!("{e:#}"))),
            };
            // With the server's albums, if it's there (docs/server-library.md).
            let with = |c: &Catalogue| {
                let mut st2 = st.clone();
                let songs = lib.songs.iter().cloned().chain(c.songs.iter().cloned()).collect();
                (Arc::new(Library::build(songs, &mut st2)), st2)
            };
            let Some((sub, url)) = server_library::client(&settings) else {
                return send(Ok((Arc::new(lib), st, ServerView::Off)));
            };
            if let Err(e) = sub.reachable() {
                return send(Ok((Arc::new(lib), st, ServerView::NotConnected(format!("{e:#}")))));
            }
            let kept = Catalogue::load(&settings, &url);
            if let Some(c) = &kept {
                let (l, s2) = with(c);
                send(Ok((l, s2, ServerView::Connected { songs: c.songs.len() })));
            }
            match Catalogue::fetch(&settings, &sub, &url) {
                Ok(fresh) => {
                    if kept.as_ref().is_none_or(|k| !k.same_songs(&fresh)) {
                        let _ = fresh.save(&settings);
                        let (l, s2) = with(&fresh);
                        send(Ok((l, s2, ServerView::Connected { songs: fresh.songs.len() })));
                    }
                }
                Err(e) if kept.is_none() => send(Ok((Arc::new(lib), st, ServerView::NotConnected(format!("{e:#}"))))),
                Err(_) => {}
            }
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
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
            // Fetched once at start (for "Add to playlist"), then on the page's Refresh and after each change.
            if self.playlists.available() {
                self.playlists.refresh(ctx);
            }
        }
        self.playlists.poll();
        let mut results = Vec::new();
        let mut finished = false;
        if let Some(rx) = &self.scan {
            loop {
                match rx.try_recv() {
                    Ok(r) => results.push(r),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.scan = None;
        }
        for r in results {
            match r {
                Ok((lib, st, server)) => {
                    if server != self.server {
                        match &server {
                            ServerView::Connected { songs } => self.log.push(format!("Server: connected, {songs} songs")),
                            ServerView::NotConnected(e) => self.log.push(format!("Server: not connected ({e}): only this computer's albums show")),
                            ServerView::Off => {}
                        }
                        self.server = server;
                    }
                    // A retry while the server stays away changes nothing: no line for it.
                    if !self.retrying || matches!(self.server, ServerView::Connected { .. }) {
                        self.log.push(format!(
                            "Library: {} songs, {} albums ({} files read{})",
                            lib.songs.len(),
                            lib.albums.len(),
                            st.read,
                            if st.failed.is_empty() { String::new() } else { format!(", {} unreadable", st.failed.len()) }
                        ));
                    }
                    self.by_path = lib.songs.iter().enumerate().map(|(i, s)| (s.path.clone(), i)).collect();
                    self.song_text = lib.songs.iter().map(|s| norm(&format!("{} {} {} {}", s.title, s.artist, s.album, s.album_artist))).collect();
                    self.album_text = lib.albums.iter().map(|a| norm(&format!("{} {}", a.title, a.artist))).collect();
                    // NavHistory stores stable album/artist keys, so rescans preserve open pages.
                    self.lib = Some(lib);
                    self.covers.clear();
                    self.run_demo();
                }
                Err(e) => self.log.push(format!("Library scan failed: {e}")),
            }
        }
        if finished {
            // Not connected: try again in a while (the phone does the same).
            self.server_retry = matches!(self.server, ServerView::NotConnected(_)).then(|| std::time::Instant::now() + SERVER_RETRY);
            if self.server_retry.is_none() {
                self.retrying = false;
            }
            self.catalogue_check = matches!(self.server, ServerView::Connected { .. }).then(|| std::time::Instant::now() + CATALOGUE_REFRESH);
        }
        if finished && std::mem::take(&mut self.rescan_wanted) {
            self.rescan(ctx);
        } else if let Some(at) = self.server_retry
            && self.scan.is_none()
        {
            let now = std::time::Instant::now();
            if now >= at {
                if !self.retrying {
                    self.log.push(format!("Server: trying again every {} s", SERVER_RETRY.as_secs()));
                }
                self.server_retry = None;
                self.retrying = true;
                self.rescan(ctx);
            } else {
                ctx.request_repaint_after(at - now);
            }
        } else if let Some(at) = self.catalogue_check
            && self.scan.is_none()
        {
            let now = std::time::Instant::now();
            if now >= at {
                self.catalogue_check = None;
                self.refresh_catalogue(ctx);
            } else {
                ctx.request_repaint_after(at - now);
            }
        }
        let s = self.player.status();
        // Volume may have been changed from the desktop's media widget.
        let v = self.player.volume().sqrt();
        if (v - self.volume).abs() > 0.001 && self.volume > 0.0 {
            self.volume = v;
        }
        if s.error != self.last.error && let Some(e) = &s.error {
            self.log.push(format!("Player: {e}"));
        }
        if s.state == State::Playing {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if self.saved_at.elapsed() > Duration::from_secs(10) {
            self.save_session();
        }
        self.prefetch(&s);
        if let Some((p, e)) = self.fetcher.status().error
            && self.last_fetch_error.as_ref() != Some(&(p.clone(), e.clone()))
        {
            self.log.push(format!("Couldn't get a song from the server: {e}"));
            self.last_fetch_error = Some((p, e));
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
            repeat: self.repeat == Repeat::All,
            repeat_one: self.repeat == Repeat::One,
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
        let ok_nav = lib.albums.get(ok).map(|a| NavView::Album { artist: norm(&a.artist), title: norm(&a.title) });
        match demo.as_str() {
            "album" => {
                if let Some(v) = ok_nav {
                    self.push_nav(v);
                }
            }
            "play" => {
                if let Some(v) = ok_nav {
                    self.push_nav(v);
                }
                self.play(lib.albums[ok].songs.clone(), 3);
                self.queue_open = true;
            }
            "now" | "now-details" => {
                self.play(lib.albums[ok].songs.clone(), 3);
                self.open_now();
                if demo == "now-details" {
                    self.now_tab = NowTab::Details;
                }
            }
            "search" => {
                self.search = "love".into();
                self.on_search_changed();
            }
            "artists" => self.push_nav(NavView::Artists { selected: None }),
            "songs" => self.push_nav(NavView::Songs),
            "recent" => self.push_nav(NavView::Recent),
            "playlists" => self.push_nav(NavView::Playlists),
            "playlist" => self.push_nav(NavView::Playlist(DEMO_FIRST_PLAYLIST.into())),
            _ => {}
        }
    }

    /// Space, arrows, Alt+Left/Right, Esc, Ctrl+F (when no text field has the keyboard).
    pub fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let no_modal = self.remove_dialog.is_none() && self.name_dialog.is_none() && self.confirm_delete.is_none();
        let (space, left, right, alt_left, alt_right, esc, find) = ctx.input(|i| {
            let plain = !i.modifiers.alt && !i.modifiers.command && !i.modifiers.ctrl;
            (
                i.key_pressed(egui::Key::Space),
                plain && i.key_pressed(egui::Key::ArrowLeft),
                plain && i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight),
                no_modal && i.key_pressed(egui::Key::Escape),
                i.modifiers.command && i.key_pressed(egui::Key::F),
            )
        });
        if alt_left || esc {
            self.go_back();
        } else if alt_right {
            self.go_forward();
        }
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

    /// Server songs about to play: the one at `index` and the next few.
    fn upcoming(&self, queue: &[PathBuf], index: usize) -> Vec<PathBuf> {
        let dir = server_library::songs_dir(&self.settings);
        queue.iter().skip(index).filter(|p| p.starts_with(&dir)).take(4).cloned().collect()
    }

    /// Keeps the server songs about to play downloading (or ready).
    fn prefetch(&mut self, s: &Status) {
        let want = self.upcoming(&self.player.queue(), s.index.unwrap_or(0));
        if want != self.wanted {
            self.fetcher.want(want.clone());
            self.wanted = want;
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
        let paths: Vec<PathBuf> = order.iter().map(|&i| lib.songs[i].path.clone()).collect();
        // Server songs: on their way before the player opens the first one.
        let want = self.upcoming(&paths, at);
        if !want.is_empty() {
            self.fetcher.want(want.clone());
            self.wanted = want;
        }
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
                    if let Some(al) = lib.albums.get(i) {
                        self.push_nav(NavView::Album { artist: norm(&al.artist), title: norm(&al.title) });
                    }
                }
                Action::OpenGroup(v) => {
                    self.push_nav(NavView::Group(v));
                }
                Action::OpenArtist(i) => {
                    let selected = lib.artists.get(i).map(|a| norm(&a.name));
                    self.push_nav(NavView::Artists { selected });
                }
                Action::OpenFolder(p) => open_folder(&p),
                Action::AddToPlaylist(id, v) => {
                    let ids: Vec<String> = v.iter().filter_map(|&i| self.playlists.song_id(&lib.songs[i].path)).collect();
                    let name = self.playlists.get(&id).map(|p| p.name.clone()).unwrap_or_default();
                    let left = v.len() - ids.len();
                    if ids.is_empty() {
                        self.playlists.notice = Some("Only the music server's songs can go in its playlists: these are only on this computer.".into());
                    } else if let Some(ctx) = self.ctx.clone() {
                        let done = format!(
                            "Added {} to {name}{}",
                            if ids.len() == 1 { "1 song".to_string() } else { format!("{} songs", ids.len()) },
                            if left > 0 { format!(" ({left} only on this computer left out)") } else { String::new() }
                        );
                        self.playlists.edit(&ctx, crate::playlists_ui::Edit::Add { id, songs: ids }, done);
                    }
                }
                Action::NewPlaylist(v) => {
                    self.name_dialog = Some(NameDialog { title: "New playlist", text: String::new(), new_with: Some(v), rename: None });
                }
                Action::PlaylistRemove(pos) => {
                    if let (Some(id), Some(ctx)) = (self.open_playlist_id(), self.ctx.clone()) {
                        self.playlists.edit(&ctx, crate::playlists_ui::Edit::Remove { id, positions: vec![pos] }, "Removed from the playlist");
                    }
                }
                Action::AskRemove(v) if !v.is_empty() && self.remove_dialog.is_none() => {
                    let songs: Vec<&Song> = v.iter().map(|&i| &lib.songs[i]).collect();
                    // The whole of one album: say which; else the song or a count.
                    let album = lib.albums.iter().find(|a| {
                        let mut x = a.songs.clone();
                        let mut y = v.clone();
                        x.sort_unstable();
                        y.sort_unstable();
                        x == y
                    });
                    let what = match (album, songs.as_slice()) {
                        (Some(a), _) => format!("“{}” by {}", a.title, a.artist),
                        (None, [one]) => format!("“{}” by {}", one.title, one.artist),
                        (None, many) => format!("{} songs", many.len()),
                    };
                    let plan = remove::plan(&self.settings, &songs);
                    let server_ok = remove::can_remove_from_server(&localbeat_core::config::Prefs::load(&self.settings).server);
                    self.remove_dialog = Some(RemoveDialog { songs: v, what, plan, server_ok, state: RemoveState::Confirm, old_copy: None });
                }
                Action::AskRemove(_) => {}
                Action::PlaylistMove(pos, by) => {
                    let (Some(id), Some(ctx)) = (self.open_playlist_id(), self.ctx.clone()) else { continue };
                    let Some(p) = self.playlists.get(&id) else { continue };
                    let mut songs = p.songs.clone();
                    let to = pos as isize + by;
                    if pos < songs.len() && to >= 0 && (to as usize) < songs.len() {
                        let s = songs.remove(pos);
                        songs.insert(to as usize, s);
                        self.playlists.edit(&ctx, crate::playlists_ui::Edit::SetSongs { id, songs }, "Moved");
                    }
                }
            }
        }
    }

    /// The extra items for song rows' menus: "Add to playlist" when this
    /// computer has a music server login; Move/Remove on a playlist's page.
    fn row_menu(&self, in_playlist: Option<(usize, usize)>, go_to_album: bool) -> RowMenu {
        RowMenu { playlists: self.playlists.available().then(|| self.playlists.mine()), in_playlist, go_to_album }
    }

    /// What a song row's menu asks for, as an action.
    fn menu_action(&mut self, m: MenuChoice, si: usize, path: &std::path::Path) {
        self.actions.push(match m {
            MenuChoice::PlayNext => Action::PlayNext(vec![si]),
            MenuChoice::Append => Action::Append(vec![si]),
            MenuChoice::Folder => Action::OpenFolder(path.parent().map(|d| d.to_path_buf()).unwrap_or_default()),
            MenuChoice::AddTo(id) => Action::AddToPlaylist(id, vec![si]),
            MenuChoice::NewPlaylist => Action::NewPlaylist(vec![si]),
            MenuChoice::Remove(pos) => Action::PlaylistRemove(pos),
            MenuChoice::Move(pos, by) => Action::PlaylistMove(pos, by),
            MenuChoice::RemoveFromLibrary => Action::AskRemove(vec![si]),
            MenuChoice::GoToAlbum => match self.album_of(si) {
                Some(a) => Action::OpenAlbum(a),
                None => return,
            },
            MenuChoice::GoToArtist => match self.album_of(si).and_then(|a| self.artist_of(a)) {
                Some(i) => Action::OpenArtist(i),
                None => return,
            },
        });
    }

    /// The album a song (library index) is on.
    fn album_of(&self, si: usize) -> Option<usize> {
        self.lib.as_ref()?.albums.iter().position(|x| x.songs.contains(&si))
    }

    /// The artist an album is filed under.
    fn artist_of(&self, a: usize) -> Option<usize> {
        self.lib.as_ref()?.artists.iter().position(|x| x.albums.contains(&a))
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
        let current = self.nav.current().clone();
        if !matches!(current, NavView::NowPlaying) {
            self.nav_bar(ui);
        }
        match current {
            NavView::NowPlaying => self.now_view(ui, &lib),
            NavView::Search(q) => self.search_page(ui, &lib, &q),
            NavView::Album { artist, title } => match Self::resolve_album(&lib, &artist, &title) {
                Some(a) => self.album_page(ui, &lib, a),
                None => {
                    ui.label(egui::RichText::new("This album is no longer in the library.").color(theme::pal(ui).dim));
                }
            },
            NavView::Group(g) => self.group_page(ui, &lib, g),
            NavView::Explore => self.explore_page(ui, &lib),
            NavView::Albums => {
                let mut order: Vec<usize> = (0..lib.albums.len()).collect();
                match self.sort {
                    AlbumSort::Artist => {}
                    AlbumSort::Title => order.sort_by_key(|&i| localbeat_core::library::sort_name(&lib.albums[i].title)),
                    AlbumSort::Year => order.sort_by_key(|&i| (std::cmp::Reverse(lib.albums[i].year), localbeat_core::library::sort_name(&lib.albums[i].artist))),
                    AlbumSort::Recent => order.sort_by_key(|&i| std::cmp::Reverse(lib.albums[i].added)),
                }
                let mut subtitle = format!("{} albums · {} songs", lib.albums.len(), lib.songs.len());
                match &self.server {
                    ServerView::Connected { .. } => {
                        let n = lib.albums.iter().filter(|a| a.origin == Origin::Server).count();
                        subtitle.push_str(&format!(" · {n} on the music server"));
                    }
                    ServerView::NotConnected(_) => subtitle.push_str(" · music server not connected"),
                    ServerView::Off => {}
                }
                let mut sort = self.sort;
                theme::page_header(ui, "Albums", &subtitle, |ui| {
                    // Right to left: the header lays these out from the right edge.
                    for (s, label) in [(AlbumSort::Recent, "Recently added"), (AlbumSort::Year, "Year"), (AlbumSort::Artist, "Artist"), (AlbumSort::Title, "Title")] {
                        if ui.selectable_label(sort == s, label).clicked() {
                            sort = s;
                        }
                    }
                    ui.label(egui::RichText::new("Sort by").color(theme::pal(ui).faint));
                });
                self.sort = sort;
                self.grid(ui, &lib, &order, "albums");
            }
            NavView::Recent => {
                theme::page_header(ui, "Recently added", "Newest arrivals first: fresh rips and new files", |_| {});
                let order = lib.recently_added(60);
                self.grid(ui, &lib, &order, "recent");
            }
            NavView::Artists { selected } => self.artists_page(ui, &lib, selected),
            NavView::Songs => self.songs_page(ui, &lib),
            NavView::Playlists => self.playlists_page(ui, &lib),
            NavView::Playlist(id) => self.open_playlist_view(ui, &lib, id),
        }
        self.playlist_dialogs(ui.ctx());
        self.run_actions();
    }

    // ------------------------------------------------------------ playlists

    /// What the last playlist change said, or why the server couldn't be asked.
    fn playlist_messages(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        if let Some(e) = &self.playlists.error {
            ui.label(egui::RichText::new(format!("Couldn't reach the music server: {e}")).color(theme::WARN));
        }
        if let Some(n) = self.playlists.notice.clone() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(n).color(p.dim));
                if ui.small_button("OK").clicked() {
                    self.playlists.notice = None;
                }
            });
        }
    }

    /// One playlist in the list: name, then songs, length and whose.
    fn playlist_row(ui: &mut egui::Ui, name: &str, sub: &str) -> egui::Response {
        let p = theme::pal(ui);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 54.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 8.0, p.card);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let icon = Rect::from_center_size(pos2(rect.left() + 24.0, rect.center().y), Vec2::splat(20.0));
        theme::paint_icon(ui.painter(), icon, Icon::Queue, p.dim);
        let w = rect.width() - 64.0;
        let t = theme::one_line(ui, name, theme::medium(15.0), p.text, w);
        ui.painter().galley(pos2(rect.left() + 52.0, rect.top() + 8.0), t, p.text);
        let s = theme::one_line(ui, sub, egui::FontId::proportional(13.0), p.dim, w);
        ui.painter().galley(pos2(rect.left() + 52.0, rect.top() + 30.0), s, p.dim);
        resp
    }

    /// Opens a playlist page by ID (resolving `DEMO_FIRST_PLAYLIST` once loaded).
    fn open_playlist_view(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, id: String) {
        let id = if id == DEMO_FIRST_PLAYLIST && self.playlists.loaded {
            if let Some(first) = self.playlists.lists.first().map(|p| p.id.clone()) {
                self.nav.replace_current(NavView::Playlist(first.clone()));
                first
            } else {
                self.go_back();
                return;
            }
        } else {
            id
        };
        if let Some(pl) = self.playlists.get(&id).cloned() {
            self.playlist_page(ui, lib, &pl);
        } else if self.playlists.loaded && !self.playlists.busy {
            self.go_back();
        } else {
            ui.label(egui::RichText::new("Loading playlist…").color(theme::pal(ui).dim));
        }
    }

    /// The user's playlists on the music server (theirs, then shared with them).
    fn playlists_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let ctx = ui.ctx().clone();
        let available = self.playlists.available();
        let lists = self.playlists.lists.clone();
        let subtitle = if !available {
            "This computer has no music server login: join the server on the Settings page".to_string()
        } else if !self.playlists.loaded {
            "Loading…".to_string()
        } else {
            format!("{} playlists · the same on your phone and to an AI (localbeat mcp)", lists.len())
        };
        theme::page_header(ui, "Playlists", &subtitle, |ui| {
            if available && theme::button(ui, Some(Icon::Plus), "New playlist", Kind::Primary).clicked() {
                self.actions.push(Action::NewPlaylist(Vec::new()));
            }
            if available && theme::button(ui, Some(Icon::Refresh), "Refresh", Kind::Secondary).clicked() {
                self.playlists.refresh(&ctx);
            }
        });
        self.playlist_messages(ui);
        let p = theme::pal(ui);
        let summary = |pl: &crate::playlists_ui::Playlist, me: &mut Self| {
            let rows = me.playlists.songs_in(pl, &lib.songs);
            let secs: f64 = rows.iter().map(|&(_, si)| lib.songs[si].seconds).sum();
            let mut s = format!("{} songs · {}", rows.len(), long_duration(secs));
            if pl.readonly && !pl.owner.is_empty() {
                s.push_str(&format!(" · from {}", pl.owner));
            }
            s
        };
        let out = self.scroll_area("playlists").show(ui, |ui| {
            let (mine, shared): (Vec<_>, Vec<_>) = lists.iter().partition(|pl| !pl.readonly);
            if self.playlists.loaded && lists.is_empty() && available {
                ui.label(egui::RichText::new("No playlists yet. Make one here, or right-click songs: Add to playlist.").color(p.dim));
            }
            for (title, group) in [(None, mine), (Some("Shared with you"), shared)] {
                if group.is_empty() {
                    continue;
                }
                if let Some(t) = title {
                    ui.add_space(12.0);
                    theme::section_label(ui, t);
                }
                for pl in group {
                    let sub = summary(pl, self);
                    if Self::playlist_row(ui, &pl.name, &sub).clicked() {
                        self.push_nav(NavView::Playlist(pl.id.clone()));
                    }
                }
            }
        });
        self.nav.set_scroll(out.state.clamped_offset().y);
    }

    /// One playlist's songs, in order.
    fn playlist_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, pl: &crate::playlists_ui::Playlist) {
        let p = theme::pal(ui);
        let rows = self.playlists.songs_in(pl, &lib.songs);
        let order: Vec<usize> = rows.iter().map(|&(_, si)| si).collect();
        let mut subtitle = format!("{} songs · {}", order.len(), long_duration(order.iter().map(|&si| lib.songs[si].seconds).sum()));
        let not_here = pl.songs.len() - rows.len() + pl.missing as usize;
        if not_here > 0 {
            subtitle.push_str(&format!(" · {not_here} not available"));
        }
        if pl.readonly && !pl.owner.is_empty() {
            subtitle.push_str(&format!(" · from {}", pl.owner));
        }
        let ctx = ui.ctx().clone();
        theme::page_header(ui, &pl.name, &subtitle, |ui| {
            if pl.readonly {
                if theme::button(ui, None, "Make a copy", Kind::Secondary).clicked() {
                    self.playlists.edit(&ctx, crate::playlists_ui::Edit::Create { name: pl.name.clone(), songs: pl.songs.clone() }, format!("Copied {}: it's yours to change", pl.name));
                }
            } else {
                if theme::button(ui, None, "Delete", Kind::Danger).clicked() {
                    self.confirm_delete = Some(pl.id.clone());
                }
                if theme::button(ui, None, "Rename", Kind::Secondary).clicked() {
                    self.name_dialog = Some(NameDialog { title: "Rename playlist", text: pl.name.clone(), new_with: None, rename: Some(pl.id.clone()) });
                }
            }
            if !order.is_empty() && theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                self.actions.push(Action::Shuffle(order.clone()));
            }
            if !order.is_empty() && theme::button(ui, Some(Icon::Play), "Play", Kind::Primary).clicked() {
                self.actions.push(Action::Play(order.clone(), 0));
            }
        });
        if !pl.comment.is_empty() {
            ui.label(egui::RichText::new(&pl.comment).color(p.dim));
        }
        self.playlist_messages(ui);
        let now = self.now();
        let time = ui.input(|i| i.time);
        let playing = self.last.state == State::Playing;
        let len = pl.songs.len();
        let out = self.scroll_area(("playlist", &pl.id)).show_rows(ui, 38.0, rows.len(), |ui, range| {
            for k in range {
                let (pos, si) = rows[k];
                let s = &lib.songs[si];
                let menu = self.row_menu(if pl.readonly { None } else { Some((pos, len)) }, true);
                let album = if s.album.is_empty() { None } else { Some(s.album.as_str()) };
                if let Some(click) = song_row(ui, s, k + 1, true, now == Some(si), playing, time, album, &menu) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(order.clone(), k)),
                        RowClick::Menu(m) => self.menu_action(m, si, &s.path),
                    }
                }
            }
        });
        self.nav.set_scroll(out.state.clamped_offset().y);
    }

    /// The name box (new / rename) and "delete?" for playlists.
    /// An album (outbox name) has reached the server as FLAC: once the
    /// library is read, ask whether to remove its other copies (an old
    /// MP3/AAC rip in Miscellaneous, on the server or here).
    pub fn offer_old_copies(&mut self, album: String) {
        if !self.old_copy_checks.contains(&album) {
            self.old_copy_checks.push(album);
        }
    }

    /// The next waiting [`offer_old_copies`] check, when nothing else is
    /// open and the library isn't being read.
    fn check_old_copies(&mut self) {
        if self.old_copy_checks.is_empty() || self.remove_dialog.is_some() || self.scan.is_some() {
            return;
        }
        let Some(lib) = self.lib.clone() else { return };
        while !self.old_copy_checks.is_empty() {
            let album = self.old_copy_checks.remove(0);
            let old = remove::old_copies(&self.settings, &lib.songs, &album);
            if old.is_empty() {
                continue;
            }
            let songs: Vec<&Song> = old.iter().map(|&i| &lib.songs[i]).collect();
            // Each folder of the old copy with its song count and formats.
            let mut folders: std::collections::BTreeMap<String, (usize, std::collections::BTreeSet<String>)> = Default::default();
            for x in &songs {
                let place = match (&x.origin, &x.server_path) {
                    (localbeat_core::library::Origin::Server, Some(p)) => {
                        format!("on the server: {}", p.rsplit_once('/').map(|(d, _)| d).unwrap_or(p))
                    }
                    _ => format!("on this computer: {}", x.path.parent().map(|d| d.display().to_string()).unwrap_or_default()),
                };
                let e = folders.entry(place).or_default();
                e.0 += 1;
                e.1.insert(if x.codec.is_empty() { "?".into() } else { x.codec.clone() });
            }
            let lines: Vec<String> = folders
                .iter()
                .map(|(place, (n, codecs))| format!("• {} ({}), {place}", files(*n), codecs.iter().cloned().collect::<Vec<_>>().join(", ")))
                .collect();
            let (artist, title) = album.split_once('/').unwrap_or(("", &album));
            let intro = format!(
                "“{title}”{} is on the server now as FLAC. The library still has another copy of it:
{}",
                if artist.is_empty() { String::new() } else { format!(" by {artist}") },
                lines.join("
")
            );
            let plan = remove::plan(&self.settings, &songs);
            let server_ok = remove::can_remove_from_server(&localbeat_core::config::Prefs::load(&self.settings).server);
            self.log.push(format!("{album}: an old copy is still in the library ({})", files(old.len())));
            self.remove_dialog = Some(RemoveDialog {
                songs: old,
                what: format!("the old copy of “{title}”"),
                plan,
                server_ok,
                state: RemoveState::Confirm,
                old_copy: Some(intro),
            });
            return;
        }
    }

    /// "Remove from library": what goes where, then the removal in the
    /// background (the server's part waits for its scan), then how it went.
    /// Drawn by main.rs on every page (an old copy is offered whichever is open).
    pub fn remove_dialog(&mut self, ctx: &egui::Context) {
        self.check_old_copies();
        let Some(d) = &mut self.remove_dialog else { return };
        // The background job's answer.
        let answer = match &d.state {
            RemoveState::Working(rx) => match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Disconnected) => Some(Err("the removal stopped unexpectedly".into())),
                Err(TryRecvError::Empty) => None,
            },
            _ => None,
        };
        if let Some(r) = answer {
            self.log.push(match &r {
                Ok(rep) => rep.summary(),
                Err(e) => format!("Remove: {e}"),
            });
            let removed = self.remove_dialog.as_ref().map(|d| d.songs.clone()).unwrap_or_default();
            if let Some(d) = &mut self.remove_dialog {
                d.state = RemoveState::Done(r);
            }
            // An open album that was removed: back to where it was opened from.
            if let NavView::Album { artist, title } = self.nav.current().clone() {
                let gone = self.lib.as_ref().and_then(|l| {
                    let a = Self::resolve_album(l, &artist, &title)?;
                    Some(l.albums[a].songs.iter().all(|s| removed.contains(s)))
                });
                if gone == Some(true) {
                    self.go_back();
                }
            }
            // The library (and the server's catalogue) without them.
            self.rescan(ctx);
        }
        let now = self.now();
        let Some(d) = &mut self.remove_dialog else { return };
        let (mut go, mut close) = (false, false);
        let local = d.plan.local.len();
        let server_only = d.songs.len().saturating_sub(local);
        egui::Window::new(if d.old_copy.is_some() { "Remove the old copy?" } else { "Remove from library?" })
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                match &d.state {
                    RemoveState::Confirm => {
                        if let Some(intro) = &d.old_copy {
                            ui.label(intro);
                            ui.add_space(6.0);
                        }
                        ui.label(egui::RichText::new(format!("Remove {}?", d.what)).strong());
                        ui.add_space(4.0);
                        if d.server_ok && !d.plan.server.is_empty() {
                            ui.label("• From the music server: gone from every device (phone, every Studio). Kept on the Mac in /srv/music/Removed, so it can be put back.");
                        }
                        if local > 0 {
                            ui.label(format!("• From this computer: {} to the Recycle Bin.", files(local)));
                        }
                        if !d.server_ok && server_only > 0 {
                            ui.label(egui::RichText::new(format!(
                                "{} only on the server can't be removed from this computer: only an admin computer can.",
                                files(server_only)
                            ))
                            .color(theme::WARN));
                        } else if !d.server_ok && local > 0 {
                            ui.label(egui::RichText::new("The server's copy, if it has one, stays (only an admin computer can remove it) and shows here instead.").small());
                        }
                        if d.server_ok && !d.plan.server.is_empty() {
                            ui.label(egui::RichText::new("Works on the home network only (the server's SSH is open there).").small());
                        }
                        ui.add_space(8.0);
                        let possible = local > 0 || (d.server_ok && !d.plan.server.is_empty());
                        ui.horizontal(|ui| {
                            let (yes, no) = if d.old_copy.is_some() { ("Remove old copy", "Keep it") } else { ("Remove", "Cancel") };
                            if ui.add_enabled_ui(possible, |ui| theme::button(ui, None, yes, Kind::Danger)).inner.clicked() {
                                go = true;
                            }
                            if theme::button(ui, None, no, Kind::Secondary).clicked() {
                                close = true;
                            }
                        });
                    }
                    RemoveState::Working(_) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Removing {}…", d.what));
                        });
                        ui.label(egui::RichText::new("The server's part waits for its scan, so this can take a minute.").small());
                    }
                    RemoveState::Done(r) => {
                        match r {
                            Ok(rep) => {
                                ui.label(rep.summary());
                                if rep.server_missing > 0 && rep.server_removed == 0 && local > 0 {
                                    ui.label(egui::RichText::new("The server didn't have it (never sent there).").small());
                                }
                                for n in &rep.notes {
                                    ui.label(egui::RichText::new(n).color(theme::WARN));
                                }
                            }
                            Err(e) => {
                                ui.label(egui::RichText::new(e).color(theme::WARN));
                            }
                        }
                        ui.add_space(6.0);
                        if theme::button(ui, None, "OK", Kind::Primary).clicked() {
                            close = true;
                        }
                    }
                }
            });
        if close {
            self.remove_dialog = None;
            return;
        }
        if go {
            // A song being removed can't stay open in the player (Windows
            // won't move an open file).
            if now.is_some_and(|n| d.songs.contains(&n)) {
                self.player.stop();
            }
            let (tx, rx) = channel();
            let (settings, plan, ctx2) = (self.settings.clone(), d.plan.clone(), ctx.clone());
            let server = if d.server_ok { localbeat_core::config::Prefs::load(&settings).server } else { Default::default() };
            std::thread::spawn(move || {
                let r = remove::remove(&settings, &server, &plan).map_err(|e| format!("{e:#}"));
                let _ = tx.send(r);
                ctx2.request_repaint();
            });
            d.state = RemoveState::Working(rx);
        }
    }

    fn playlist_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.name_dialog {
            let (mut ok, mut cancel) = (false, false);
            egui::Window::new(d.title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut d.text).hint_text("Name").desired_width(300.0));
                    if !r.has_focus() && d.text.is_empty() {
                        r.request_focus();
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        ok = true;
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let label = if d.rename.is_some() { "Rename" } else { "Create" };
                        if theme::button(ui, Some(Icon::Check), label, Kind::Primary).clicked() {
                            ok = true;
                        }
                        if theme::button(ui, None, "Cancel", Kind::Secondary).clicked() {
                            cancel = true;
                        }
                    });
                });
            let name = d.text.trim().to_string();
            if ok && !name.is_empty() {
                let d = self.name_dialog.take().unwrap();
                if let Some(id) = d.rename {
                    self.playlists.edit(ctx, crate::playlists_ui::Edit::Rename { id, name: name.clone() }, format!("Renamed to {name}"));
                } else {
                    let songs = d.new_with.unwrap_or_default();
                    let ids: Vec<String> = match &self.lib {
                        Some(lib) => songs.iter().filter_map(|&i| self.playlists.song_id(&lib.songs[i].path)).collect(),
                        None => Vec::new(),
                    };
                    let left = songs.len() - ids.len();
                    let done = format!(
                        "Made {name} with {} songs{}",
                        ids.len(),
                        if left > 0 { format!(" ({left} only on this computer left out)") } else { String::new() }
                    );
                    self.playlists.edit(ctx, crate::playlists_ui::Edit::Create { name, songs: ids }, done);
                }
            } else if cancel {
                self.name_dialog = None;
            }
        }
        if let Some(id) = self.confirm_delete.clone() {
            let name = self.playlists.get(&id).map(|p| p.name.clone()).unwrap_or_default();
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Delete playlist?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Delete “{name}” from all your devices? The songs stay in the library."));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if theme::button(ui, None, "Delete", Kind::Danger).clicked() {
                            yes = true;
                        }
                        if theme::button(ui, None, "Cancel", Kind::Secondary).clicked() {
                            no = true;
                        }
                    });
                });
            if yes {
                self.confirm_delete = None;
                self.go_back();
                self.playlists.edit(ctx, crate::playlists_ui::Edit::Delete { id }, format!("Deleted {name}"));
            } else if no {
                self.confirm_delete = None;
            }
        }
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
        let avail = (ui.available_width() - 16.0).max(180.0);
        let cols = ((avail + gap) / (190.0 + gap)).floor().max(1.0) as usize;
        let tile = (avail - gap * (cols - 1) as f32) / cols as f32;
        let row_h = tile + 52.0;
        let rows = albums.len().div_ceil(cols);
        let out = self.scroll_area(id).show_rows(ui, row_h, rows, |ui, range| {
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
        self.nav.set_scroll(out.state.clamped_offset().y);
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
        let sub_text = match album.year {
            Some(y) => format!("{} · {y}", album.artist),
            None => album.artist.clone(),
        };
        let sub = theme::one_line(ui, &sub_text, egui::FontId::proportional(13.0), p.dim, size);
        let truncated = title.elided || sub.elided;
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
        let resp = if truncated { resp.on_hover_text(format!("{}\n{sub_text}", album.title)) } else { resp };
        let songs = album.songs.clone();
        let dir = album.dirs.first().cloned();
        let artist = lib.artists.iter().position(|x| x.albums.contains(&a));
        resp.context_menu(|ui| {
            self.song_menu(ui, songs.clone(), dir.clone(), artist);
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

    /// An album's right-click menu, in the suite's shared order: Play next, Add to
    /// queue, Add to playlist…, Go to artist; then Studio's own.
    fn song_menu(&mut self, ui: &mut egui::Ui, songs: Vec<usize>, dir: Option<PathBuf>, artist: Option<usize>) {
        if ui.button("Play next").clicked() {
            self.actions.push(Action::PlayNext(songs.clone()));
            ui.close();
        }
        if ui.button("Add to queue").clicked() {
            self.actions.push(Action::Append(songs.clone()));
            ui.close();
        }
        if self.playlists.available() {
            let mine = self.playlists.mine();
            ui.menu_button("Add to playlist", |ui| {
                for (id, name) in mine {
                    if ui.button(name).clicked() {
                        self.actions.push(Action::AddToPlaylist(id, songs.clone()));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("New playlist…").clicked() {
                    self.actions.push(Action::NewPlaylist(songs.clone()));
                    ui.close();
                }
            });
        }
        if let Some(i) = artist
            && ui.button("Go to artist").clicked()
        {
            self.actions.push(Action::OpenArtist(i));
            ui.close();
        }
        // Studio's own items, after the shared ones (LocalBeat docs/ui-refinement.md).
        ui.separator();
        if let Some(d) = dir
            && ui.button("Show in folder").clicked()
        {
            self.actions.push(Action::OpenFolder(d));
            ui.close();
        }
        if ui.button("Remove from library…").clicked() {
            self.actions.push(Action::AskRemove(songs));
            ui.close();
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

        ui.add_space(4.0);
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
                    if !songs.is_empty() && songs.iter().all(|s| is_cd_rip(s)) {
                        theme::badge(ui, "CD rip", theme::OK).on_hover_text("Ripped and verified with LocalBeat Studio");
                    }
                    if album.origin == Origin::Server {
                        theme::badge(ui, "On the music server", theme::ACCENT).on_hover_text("Not on this computer: played from the music server");
                    }
                    if let Some(f) = format_label(&songs) {
                        theme::badge(ui, &f, if songs.iter().all(|s| s.lossless()) { theme::prism_at(0.1) } else { p.dim });
                    }
                });
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&album.title).font(theme::bold(34.0)).color(p.text));
                let artist = ui.add(egui::Label::new(egui::RichText::new(&album.artist).font(theme::medium(19.0)).color(theme::prism_at(0.3))).sense(Sense::click()));
                if artist.clicked() && let Some(i) = lib.artists.iter().position(|x| x.albums.contains(&a)) {
                    self.actions.push(Action::OpenArtist(i));
                }
                if artist.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let mut meta = Vec::new();
                if let Some(y) = album.year {
                    meta.push(y.to_string());
                }
                meta.push(format!("{} songs, {}", album.songs.len(), long_duration(album.seconds)));
                ui.label(egui::RichText::new(meta.join(" · ")).color(p.dim));
                // Genre, mood and decade: each opens its page.
                ui.add_space(6.0);
                self.tag_chips(ui, lib, a);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Some(Icon::Play), "Play", Kind::Primary).clicked() {
                        self.actions.push(Action::Play(album.songs.clone(), 0));
                    }
                    if theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                        self.actions.push(Action::Shuffle(album.songs.clone()));
                    }
                    if theme::button(ui, Some(Icon::Plus), "Add to queue", Kind::Secondary).on_hover_text("Add the album to the end of the queue").clicked() {
                        self.actions.push(Action::Append(album.songs.clone()));
                    }
                    if let Some(d) = album.dirs.first() && theme::icon_button(ui, Icon::Folder, 32.0, "Show in folder").clicked() {
                        self.actions.push(Action::OpenFolder(d.clone()));
                    }
                    if theme::button(ui, None, "Remove…", Kind::Secondary)
                        .on_hover_text("Take this album out of the library: off the server and this computer")
                        .clicked()
                    {
                        self.actions.push(Action::AskRemove(album.songs.clone()));
                    }
                });
            });
        });
        ui.add_space(18.0);
        let now = self.now();
        let time = ui.input(|i| i.time);
        let playing = self.last.state == State::Playing;
        let menu = self.row_menu(None, false);
        let out = self.scroll_area(("album", a)).show(ui, |ui| {
            let mut disc = None;
            for (k, &si) in album.songs.iter().enumerate() {
                let s = &lib.songs[si];
                if album.discs > 1 && s.disc != disc {
                    disc = s.disc;
                    ui.add_space(8.0);
                    theme::section_label(ui, &format!("Disc {}", s.disc.unwrap_or(1)));
                }
                let show_artist = !s.artist.is_empty() && norm(&s.artist) != norm(&album.artist);
                if let Some(click) = song_row(ui, s, k + 1, show_artist, now == Some(si), playing, time, None, &menu) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(album.songs.clone(), k)),
                        RowClick::Menu(m) => self.menu_action(m, si, &s.path),
                    }
                }
            }
            ui.add_space(14.0);
            if let Some(d) = album.dirs.first() {
                ui.label(egui::RichText::new(d.display().to_string()).small().color(p.faint));
            }
            ui.add_space(20.0);
        });
        self.nav.set_scroll(out.state.clamped_offset().y);
    }

    fn artists_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, selected_key: Option<String>) {
        let p = theme::pal(ui);
        let mut sel = selected_key.as_deref().and_then(|k| Self::resolve_artist(lib, k));
        if sel.is_none() && !lib.artists.is_empty() {
            sel = Some(0);
        }
        let mut picked = None;
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
                        let elided = g.elided;
                        ui.painter().galley(pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, p.text);
                        ui.painter().text(rect.right_center() - vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, a.albums.len().to_string(), egui::FontId::proportional(12.0), p.faint);
                        if resp.clicked() {
                            picked = Some(i);
                        }
                        if elided {
                            resp.on_hover_text(&a.name);
                        }
                    }
                });
            });
        if let Some(i) = picked {
            sel = Some(i);
            self.nav.replace_current(NavView::Artists { selected: Some(norm(&lib.artists[i].name)) });
        }
        if let Some(i) = sel {
            let a = &lib.artists[i];
            let songs: Vec<usize> = a.albums.iter().flat_map(|&al| lib.albums[al].songs.iter().copied()).collect();
            let subtitle = format!("{} albums · {} songs", a.albums.len(), songs.len());
            theme::page_header(ui, &a.name, &subtitle, |ui| {
                // Right to left: Play, then Shuffle.
                if theme::button(ui, Some(Icon::Shuffle), "Shuffle", Kind::Secondary).clicked() {
                    self.actions.push(Action::Shuffle(songs.clone()));
                }
                if theme::button(ui, Some(Icon::Play), "Play", Kind::Primary).clicked() {
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
        let menu = self.row_menu(None, true);
        let out = self.scroll_area(id).show_rows(ui, 38.0, order.len(), |ui, range| {
            for k in range {
                let si = order[k];
                let s = &lib.songs[si];
                let album = if s.album.is_empty() { None } else { Some(s.album.as_str()) };
                if let Some(click) = song_row(ui, s, 0, true, now == Some(si), playing, time, album, &menu) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(order.to_vec(), k)),
                        RowClick::Menu(m) => self.menu_action(m, si, &s.path),
                    }
                }
            }
        });
        self.nav.set_scroll(out.state.clamped_offset().y);
    }

    fn search_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, query: &str) {
        let p = theme::pal(ui);
        let words: Vec<String> = norm(query).split(' ').map(str::to_string).collect();
        let hit = |t: &str| words.iter().all(|w| t.contains(w.as_str()));
        let artists: Vec<usize> = (0..lib.artists.len()).filter(|&i| hit(&norm(&lib.artists[i].name))).take(12).collect();
        let albums: Vec<usize> = (0..lib.albums.len()).filter(|&i| hit(&self.album_text[i])).take(60).collect();
        let songs: Vec<usize> = lib.albums.iter().flat_map(|a| a.songs.iter().copied()).filter(|&i| hit(&self.song_text[i])).take(300).collect();
        let playlists: Vec<(String, String)> =
            self.playlists.lists.iter().filter(|pl| hit(&norm(&pl.name))).map(|pl| (pl.id.clone(), pl.name.clone())).collect();
        let subtitle = format!("{} playlists · {} artists · {} albums · {} songs", playlists.len(), artists.len(), albums.len(), songs.len());
        theme::page_header(ui, &format!("“{}”", query.trim()), &subtitle, |_| {});
        // The phone's order: Moods, Genres, Decades, Playlists, Artists, Albums, Songs.
        let tags = self.search_chips(ui, lib, &hit);
        if tags == 0 && playlists.is_empty() && artists.is_empty() && albums.is_empty() && songs.is_empty() {
            ui.label(egui::RichText::new("Nothing matches.").color(p.dim));
            return;
        }
        if !playlists.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (id, name) in &playlists {
                    if theme::button(ui, Some(Icon::Queue), name, Kind::Secondary).clicked() {
                        self.push_nav(NavView::Playlist(id.clone()));
                    }
                }
            });
            ui.add_space(6.0);
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
            if resp.on_hover_text("Now Playing").clicked() && song.is_some() {
                // As the sidebar's Now Playing: from any tab, and over a search.
                self.open_now();
                self.show_library = true;
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
            let q = theme::icon_button(ui, Icon::Queue, 34.0, "Queue");
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
                let sh = theme::icon_button(ui, Icon::Shuffle, 34.0, if self.shuffle { "Shuffle: on" } else { "Shuffle: off" });
                if sh.clicked() {
                    self.toggle_shuffle();
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
                        if let (NavView::Album { artist, title }, Some(l)) = (self.nav.current().clone(), &lib)
                            && let Some(a) = Self::resolve_album(l, &artist, &title)
                        {
                            self.actions.push(Action::Play(l.albums[a].songs.clone(), 0));
                        }
                    } else {
                        self.player.toggle();
                    }
                }
                if theme::icon_button(ui, Icon::Next, 34.0, "Next").clicked() {
                    self.player.next();
                }
                let rp = theme::icon_button(ui, Icon::Repeat, 34.0, match self.repeat {
                    Repeat::Off => "Repeat: off",
                    Repeat::All => "Repeat: all",
                    Repeat::One => "Repeat: this song",
                });
                if rp.clicked() {
                    self.cycle_repeat();
                }
                self.mode_marks(ui, &sh, &rp, 2.0);
            });
            ui.horizontal(|ui| {
                let dur = s.duration.unwrap_or(0.0);
                let pos = self.seek_preview.map(|f| f as f64 * dur).unwrap_or(s.position);
                let label = |t: f64| egui::RichText::new(mmss(t)).font(egui::FontId::monospace(12.0)).color(p.dim);
                ui.label(label(pos));
                let w = ui.available_width() - 48.0;
                let frac = if dur > 0.0 { (s.position / dur) as f32 } else { 0.0 };
                if let Some(f) = theme::seek_bar(ui, w, frac, &mut self.seek_preview) && dur > 0.0 {
                    self.player.seek(f as f64 * dur);
                }
                ui.label(label(dur));
            });
        });
        self.run_actions();
    }

    pub fn playing(&self) -> bool {
        self.last.state == State::Playing
    }

    pub fn has_song(&self) -> bool {
        self.last.path.is_some()
    }

    /// The full-size Now Playing view: big cover, names, transport on the
    /// left; the queue or the song's details on the right.
    fn now_view(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let p = theme::pal(ui);
        let s = self.last.clone();
        let song = self.now().map(|i| (i, lib.songs[i].clone()));
        let album = song.as_ref().and_then(|(i, _)| lib.albums.iter().position(|a| a.songs.contains(i)));
        // The cover's colour washed over the page.
        let accent = album.and_then(|a| self.covers.get(lib, a, 256)).map(|c| c.accent).unwrap_or(Color32::from_rgb(70, 60, 130));
        let area = ui.clip_rect();
        let mut mesh = egui::Mesh::default();
        let strong = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if ui.visuals().dark_mode { 175 } else { 100 });
        let soft = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if ui.visuals().dark_mode { 60 } else { 35 });
        mesh.colored_vertex(area.left_top(), strong);
        mesh.colored_vertex(area.right_top(), soft);
        mesh.colored_vertex(area.left_bottom(), soft);
        mesh.colored_vertex(area.right_bottom(), Color32::TRANSPARENT);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 3, 2);
        ui.painter().add(egui::Shape::mesh(mesh));

        self.nav_bar(ui);
        let Some((si, x)) = song else {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(egui::RichText::new("Nothing playing").font(theme::bold(26.0)).color(p.text));
                ui.label(egui::RichText::new("Pick an album and press play").color(p.dim));
            });
            return;
        };
        ui.add_space(8.0);
        let avail = ui.available_rect_before_wrap();
        let left_w = (avail.width() * 0.46).clamp(320.0, 640.0);
        let left = Rect::from_min_size(avail.min, vec2(left_w, avail.height()));
        let right = Rect::from_min_max(pos2(avail.left() + left_w + 36.0, avail.top()), avail.max);
        ui.allocate_rect(avail, Sense::hover());

        // ---- left: cover, names, transport
        ui.scope_builder(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::top_down(egui::Align::Center)), |ui| {
            let cover = (left_w - 20.0).min(avail.height() - 250.0).max(180.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(cover), Sense::hover());
            let shadow = egui::Shadow { offset: [0, 22], blur: 56, spread: 0, color: Color32::from_black_alpha(170) };
            ui.painter().add(shadow.as_shape(rect, 16));
            match album {
                Some(a) => self.paint_cover(ui, lib, a, rect, 512),
                None => {
                    ui.painter().rect_filled(rect, 16.0, p.card);
                    theme::disc(ui.painter(), rect.center(), cover * 0.36, None, None, ui.visuals().dark_mode);
                }
            }
            ui.add_space(14.0);
            let w = cover;
            ui.label(theme::one_line(ui, &x.title, theme::bold(26.0), p.text, w));
            let artist = if x.artist.is_empty() { x.album_artist.clone() } else { x.artist.clone() };
            let ar = ui.add(egui::Label::new(theme::one_line(ui, &artist, theme::medium(17.0), theme::prism_at(0.3), w)).sense(Sense::click()));
            if ar.clicked() && let Some(i) = album.and_then(|a| lib.artists.iter().position(|ar| ar.albums.contains(&a))) {
                self.actions.push(Action::OpenArtist(i));
            }
            let mut line = x.album.clone();
            if let Some(y) = x.year {
                line = if line.is_empty() { y.to_string() } else { format!("{line} · {y}") };
            }
            let al = ui.add(egui::Label::new(theme::one_line(ui, &line, egui::FontId::proportional(14.5), p.dim, w)).sense(Sense::click()));
            if al.clicked() && let Some(a) = album {
                self.actions.push(Action::OpenAlbum(a));
            }
            for r in [&ar, &al] {
                if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let badges_w = 220.0;
                ui.add_space(((ui.available_width() - badges_w) / 2.0).max(0.0));
                if let Some(f) = &s.format {
                    let text = match f.bits {
                        Some(b) => format!("{} {}/{}", f.codec, b, khz(f.sample_rate)),
                        None => format!("{} {}", f.codec, khz(f.sample_rate)),
                    };
                    theme::badge(ui, &text, if f.lossless { theme::prism_at(0.1) } else { p.dim });
                }
                if is_cd_rip(&x) {
                    theme::badge(ui, "CD rip", theme::OK).on_hover_text("Ripped and verified with LocalBeat Studio");
                }
            });
            ui.add_space(10.0);
            // Seek bar, full width of the cover.
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                let dur = s.duration.unwrap_or(0.0);
                let pos = self.seek_preview.map(|f| f as f64 * dur).unwrap_or(s.position);
                let label = |t: f64| egui::RichText::new(mmss(t)).font(egui::FontId::monospace(12.5)).color(p.dim);
                ui.label(label(pos));
                let frac = if dur > 0.0 { (s.position / dur) as f32 } else { 0.0 };
                if let Some(f) = theme::seek_bar(ui, (w - 96.0).max(80.0), frac, &mut self.seek_preview) && dur > 0.0 {
                    self.player.seek(f as f64 * dur);
                }
                ui.label(label(dur));
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let row_w = 40.0 * 4.0 + 64.0 + 14.0 * 4.0;
                ui.add_space(((ui.available_width() - row_w) / 2.0).max(0.0));
                ui.spacing_mut().item_spacing.x = 14.0;
                let sh = theme::icon_button(ui, Icon::Shuffle, 40.0, if self.shuffle { "Shuffle: on" } else { "Shuffle: off" });
                if sh.clicked() {
                    self.toggle_shuffle();
                }
                if theme::icon_button(ui, Icon::Prev, 40.0, "Previous (or start of the song)").clicked() {
                    self.player.prev();
                }
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::click());
                theme::prism_rect(ui.painter(), rect, 32.0, if resp.hovered() { 1.0 } else { 0.9 });
                let icon = if s.state == State::Playing { Icon::Pause } else { Icon::Play };
                theme::paint_icon(ui.painter(), rect.shrink(20.0), icon, Color32::WHITE);
                if resp.clicked() {
                    self.player.toggle();
                }
                if theme::icon_button(ui, Icon::Next, 40.0, "Next").clicked() {
                    self.player.next();
                }
                let rp = theme::icon_button(ui, Icon::Repeat, 40.0, match self.repeat {
                    Repeat::Off => "Repeat: off",
                    Repeat::All => "Repeat: all",
                    Repeat::One => "Repeat: this song",
                });
                if rp.clicked() {
                    self.cycle_repeat();
                }
                self.mode_marks(ui, &sh, &rp, 2.5);
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - 150.0) / 2.0).max(0.0));
                let icon = if self.volume == 0.0 { Icon::Mute } else { Icon::Volume };
                if theme::icon_button(ui, icon, 28.0, "Mute / unmute").clicked() {
                    self.volume = if self.volume == 0.0 { 0.8 } else { 0.0 };
                    self.player.set_volume(self.volume * self.volume);
                }
                if let Some(v) = theme::mini_slider(ui, 110.0, self.volume) {
                    self.volume = v;
                    self.player.set_volume(v * v);
                }
            });
        });

        // ---- right: Queue / Details
        ui.scope_builder(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in [(NowTab::Queue, "Queue"), (NowTab::Details, "Details")] {
                    let on = self.now_tab == tab;
                    let text = egui::RichText::new(label).font(if on { theme::bold(18.0) } else { theme::medium(18.0) }).color(if on { p.text } else { p.faint });
                    let r = ui.add(egui::Label::new(text).sense(Sense::click()));
                    if on {
                        let y = r.rect.bottom() + 3.0;
                        theme::prism_rect(ui.painter(), Rect::from_min_max(pos2(r.rect.left(), y), pos2(r.rect.right(), y + 3.0)), 1.5, 1.0);
                    }
                    if r.clicked() {
                        self.now_tab = tab;
                    }
                    if r.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    ui.add_space(16.0);
                }
            });
            ui.add_space(12.0);
            match self.now_tab {
                NowTab::Queue => self.now_queue(ui, lib),
                NowTab::Details => self.now_details(ui, &x, si),
            }
        });
        self.run_actions();
    }

    /// The queue in the Now Playing view: the whole of it, current one marked.
    fn now_queue(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        let p = theme::pal(ui);
        let queue = self.player.queue();
        let cur = self.last.index.unwrap_or(0);
        let left: f64 = queue.iter().skip(cur).filter_map(|q| self.by_path.get(q)).map(|&i| lib.songs[i].seconds).sum();
        ui.label(egui::RichText::new(format!("{} songs · {} left", queue.len(), long_duration(left))).color(p.dim));
        ui.add_space(4.0);
        let time = ui.input(|i| i.time);
        let playing = self.last.state == State::Playing;
        let mut jump = None;
        let scroll = std::mem::take(&mut self.scroll_to_now);
        egui::ScrollArea::vertical().id_salt("now-queue").auto_shrink([false, false]).show(ui, |ui| {
            for (k, path) in queue.iter().enumerate() {
                let Some(&si) = self.by_path.get(path) else { continue };
                let x = &lib.songs[si];
                let is_now = k == cur;
                let r = song_row(ui, x, k + 1, true, is_now, playing, time, None, &RowMenu::default());
                if is_now && scroll {
                    ui.scroll_to_rect(ui.min_rect().with_min_y(ui.min_rect().bottom() - 38.0), Some(egui::Align::Center));
                }
                if let Some(RowClick::Play) = r {
                    jump = Some(k);
                }
            }
        });
        if let Some(k) = jump {
            self.player.jump(k);
        }
    }

    /// The song's details: what the library knows, plus extra tags.
    fn now_details(&mut self, ui: &mut egui::Ui, x: &Song, si: usize) {
        let p = theme::pal(ui);
        if self.details.as_ref().map(|(path, _)| path) != Some(&x.path) {
            self.details = Some((x.path.clone(), song_details(&x.path)));
        }
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut add = |k: &str, v: String| {
            if !v.is_empty() {
                rows.push((k.into(), v));
            }
        };
        add("Title", x.title.clone());
        add("Artist", x.artist.clone());
        add("Album", x.album.clone());
        if !x.album_artist.is_empty() && x.album_artist != x.artist {
            add("Album artist", x.album_artist.clone());
        }
        add("Track", x.track.map(|t| t.to_string()).unwrap_or_default());
        add("Disc", x.disc.filter(|&d| d > 1).map(|d| d.to_string()).unwrap_or_default());
        add("Year", x.year.map(|y| y.to_string()).unwrap_or_default());
        add("Genre", x.genre.clone().unwrap_or_default());
        add("Length", mmss(x.seconds));
        let fmt = match (x.bits, x.sample_rate) {
            (Some(b), Some(r)) => format!("{} · {b}-bit / {} kHz", x.codec, khz(r)),
            (None, Some(r)) => format!("{} · {} kHz", x.codec, khz(r)),
            _ => x.codec.clone(),
        };
        add("Format", fmt);
        add("Bitrate", x.bitrate.map(|b| format!("{b} kbps")).unwrap_or_default());
        add("File size", format!("{:.1} MB", x.size as f64 / 1e6));
        add(
            "Source",
            match x.origin {
                Origin::Flac => "CD rip (verified FLAC library)",
                Origin::Misc => "Miscellaneous library",
                Origin::Server if is_cd_rip(x) => "CD rip (verified FLAC library, on the music server)",
                Origin::Server => "The music server",
            }
            .into(),
        );
        add("Added", date(x.added));
        if let Some((_, extra)) = &self.details {
            for (k, v) in extra {
                add(k, v.clone());
            }
        }
        add("File", x.path.display().to_string());
        let chips = self.lib.clone().zip(self.album_of(si));
        egui::ScrollArea::vertical().id_salt("now-details").auto_shrink([false, false]).show(ui, |ui| {
            if let Some((lib, a)) = &chips {
                self.tag_chips(ui, lib, *a);
                ui.add_space(10.0);
            }
            egui::Grid::new("details").num_columns(2).spacing([18.0, 9.0]).show(ui, |ui| {
                for (k, v) in &rows {
                    ui.label(egui::RichText::new(k).color(p.faint));
                    let link = match k.as_str() {
                        "MusicBrainz release" => Some(format!("https://musicbrainz.org/release/{v}")),
                        "MusicBrainz recording" => Some(format!("https://musicbrainz.org/recording/{v}")),
                        _ => None,
                    };
                    match link {
                        Some(url) => {
                            ui.hyperlink_to(v, url);
                        }
                        None => {
                            ui.add(egui::Label::new(egui::RichText::new(v).color(p.text)).wrap().selectable(true));
                        }
                    }
                    ui.end_row();
                }
            });
            if x.path.parent().is_some() {
                ui.add_space(10.0);
                if theme::button(ui, Some(Icon::Folder), "Show in folder", Kind::Secondary).clicked() {
                    open_folder(x.path.parent().unwrap());
                }
            }
        });
    }

    /// The Queue panel (right side): the song playing, then what's next.
    /// Header like the phone's: "N songs · m:ss left · shuffled", Save, Clear.
    /// Drag a row to move it; × takes it out.
    pub fn queue_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let queue = self.player.queue();
        let cur = self.last.index.unwrap_or(0).min(queue.len().saturating_sub(1));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Queue").font(theme::bold(20.0)).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, Icon::Cross, 28.0, "Close").clicked() {
                    self.queue_open = false;
                }
                if queue.is_empty() {
                    return;
                }
                if ui.add_enabled(queue.len() > cur + 1, egui::Button::new("Clear")).on_hover_text("Take out everything after the song playing").clicked() {
                    self.player.rearrange((0..=cur).collect());
                }
                if self.playlists.available() && ui.button("Save").on_hover_text("Make a playlist of the queue").clicked() {
                    let songs: Vec<usize> = queue.iter().filter_map(|q| self.by_path.get(q).copied()).collect();
                    self.actions.push(Action::NewPlaylist(songs));
                }
            });
        });
        let Some(lib) = self.lib.clone() else { return };
        if queue.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Nothing queued. Play an album, or right-click one and choose Add to queue.").color(p.dim));
            return;
        }
        let seconds = |path: &PathBuf| self.by_path.get(path).map_or(0.0, |&si| lib.songs[si].seconds);
        let left = queue[cur + 1..].iter().map(seconds).sum::<f64>() + (seconds(&queue[cur]) - self.last.position).max(0.0);
        let mut line = format!("{} · {} left", if queue.len() == 1 { "1 song".into() } else { format!("{} songs", queue.len()) }, mmss(left));
        if self.shuffle {
            line.push_str(" · shuffled");
        }
        ui.label(egui::RichText::new(line).color(p.dim));
        ui.add_space(6.0);
        let mut jump = None;
        let mut remove = None;
        let mut moved = None;
        let row_h = 42.0;
        egui::ScrollArea::vertical().id_salt("queue").auto_shrink([false, false]).show(ui, |ui| {
            let mut rows: Vec<(usize, Rect)> = Vec::new();
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
                let sense = if i == cur { Sense::click() } else { Sense::click_and_drag() };
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), sense);
                rows.push((i, rect));
                let dragging = self.queue_drag == Some(i);
                if i == cur || dragging {
                    ui.painter().rect_filled(rect, 8.0, p.card_hover);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 8.0, p.card);
                }
                let x_rect = Rect::from_center_size(rect.right_center() - vec2(18.0, 0.0), Vec2::splat(24.0));
                let show_x = i != cur && (resp.hovered() || ui.rect_contains_pointer(x_rect)) && self.queue_drag.is_none();
                let w = rect.width() - if show_x { 46.0 } else { 20.0 };
                let t = theme::one_line(ui, &title, if i == cur { theme::medium(14.0) } else { egui::FontId::proportional(14.0) }, p.text, w);
                ui.painter().galley(rect.left_top() + vec2(10.0, 4.0), t, p.text);
                let a = theme::one_line(ui, &artist, egui::FontId::proportional(12.5), p.dim, w);
                ui.painter().galley(rect.left_top() + vec2(10.0, 23.0), a, p.dim);
                if show_x {
                    let x = ui.interact(x_rect, ui.scope_id().with(("queue-x", i)), Sense::click());
                    theme::paint_icon(ui.painter(), x_rect.shrink(6.0), Icon::Cross, if x.hovered() { p.text } else { p.dim });
                    if x.on_hover_text("Remove from queue").clicked() {
                        remove = Some(i);
                    }
                }
                if resp.drag_started() {
                    self.queue_drag = Some(i);
                }
                if resp.clicked() && i != cur && remove.is_none() {
                    jump = Some(i);
                }
                resp.context_menu(|ui| {
                    if i != cur && ui.button("Play next").clicked() {
                        moved = Some((i, cur + 1));
                        ui.close();
                    }
                    if i != cur && ui.button("Remove from queue").clicked() {
                        remove = Some(i);
                        ui.close();
                    }
                });
                if i > cur + 300 {
                    ui.label(egui::RichText::new(format!("and {} more", queue.len() - i)).color(p.faint));
                    break;
                }
            }
            // Dragging: a line where the row would land; let go to move it there.
            if let Some(from) = self.queue_drag {
                if let Some(pos) = ui.ctx().pointer_interact_pos() {
                    let to = rows.iter().filter(|(i, r)| *i > cur && pos.y > r.center().y).map(|(i, _)| *i).max().map_or(cur + 1, |i| i + 1);
                    let y = rows.iter().find(|(i, _)| *i == to).map_or_else(|| rows.last().map_or(0.0, |(_, r)| r.bottom()), |(_, r)| r.top());
                    let x = rows.first().map_or(ui.max_rect().x_range(), |(_, r)| r.x_range());
                    ui.painter().hline(x, y, egui::Stroke::new(2.0, theme::prism_at(0.3)));
                    if ui.input(|i| i.pointer.any_released()) {
                        moved = Some((from, to));
                        self.queue_drag = None;
                    }
                } else if ui.input(|i| i.pointer.any_released()) {
                    self.queue_drag = None;
                }
            }
        });
        if let Some(i) = remove {
            self.player.rearrange((0..queue.len()).filter(|&k| k != i).collect());
        } else if let Some((from, to)) = moved {
            self.player.rearrange(moved_order(queue.len(), from, to));
        } else if let Some(i) = jump {
            self.player.jump(i);
        }
    }

    /// Shuffle on: what's after the song playing is shuffled now (and what
    /// you play next starts shuffled). Off: back in the order it was in.
    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        let queue = self.player.queue();
        let cur = self.last.index.unwrap_or(0);
        if queue.len() <= cur + 1 {
            self.unshuffled = None;
            return;
        }
        let mut order: Vec<usize> = (0..queue.len()).collect();
        if self.shuffle {
            self.unshuffled = Some(queue.clone());
            shuffle(&mut order[cur + 1..]);
        } else if let Some(before) = self.unshuffled.take() {
            // Each song's place in the order before shuffling (songs added
            // since go last, as they are).
            let mut used = vec![false; before.len()];
            let mut rank = |path: &PathBuf| {
                let k = before.iter().enumerate().position(|(k, b)| !used[k] && b == path);
                k.inspect(|&k| used[k] = true).unwrap_or(usize::MAX)
            };
            let mut rest: Vec<(usize, usize)> = (cur + 1..queue.len()).map(|i| (rank(&queue[i]), i)).collect();
            rest.sort_by_key(|&(r, i)| (r, i));
            order.truncate(cur + 1);
            order.extend(rest.into_iter().map(|(_, i)| i));
        } else {
            return;
        }
        self.player.rearrange(order);
    }

    /// Repeat cycles off → all → one, as on the phone.
    fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        self.player.set_repeat(self.repeat);
    }

    /// Marks the shuffle and repeat buttons: a dot when on, "1" for repeat one.
    fn mode_marks(&self, ui: &egui::Ui, shuffle: &egui::Response, repeat: &egui::Response, r: f32) {
        let c = theme::prism_at(0.3);
        if self.shuffle {
            ui.painter().circle_filled(shuffle.rect.center_bottom() + vec2(0.0, -2.0), r, c);
        }
        match self.repeat {
            Repeat::Off => {}
            Repeat::All => {
                ui.painter().circle_filled(repeat.rect.center_bottom() + vec2(0.0, -2.0), r, c);
            }
            Repeat::One => {
                ui.painter().text(repeat.rect.right_top() + vec2(-6.0, 7.0), egui::Align2::CENTER_CENTER, "1", theme::bold(10.0), c);
            }
        }
    }
}

/// Positions 0..n with the song at `from` moved so it lands before what was at `to`.
fn moved_order(n: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if from >= n {
        return order;
    }
    let item = order.remove(from);
    let to = if to > from { to - 1 } else { to }.min(order.len());
    order.insert(to, item);
    order
}

#[cfg(test)]
mod queue_tests {
    use super::moved_order;

    #[test]
    fn shuffle_mixes_songs() {
        // 50 "albums" of 12 songs in a row: after shuffling, a song should
        // seldom still be followed by the next one from its album.
        let mut v: Vec<usize> = (0..600).collect();
        super::shuffle(&mut v);
        let kept = v.windows(2).filter(|w| w[1] == w[0] + 1).count();
        eprintln!("neighbours kept: {kept} of 599; first 24: {:?}", &v[..24]);
        assert!(kept < 20, "{kept} songs still followed by their album's next song");
    }

    #[test]
    fn moving_a_queue_row() {
        assert_eq!(moved_order(5, 3, 1), [0, 3, 1, 2, 4], "up");
        assert_eq!(moved_order(5, 1, 4), [0, 2, 3, 1, 4], "down: lands before what was at 4");
        assert_eq!(moved_order(5, 1, 5), [0, 2, 3, 4, 1], "to the end");
        assert_eq!(moved_order(3, 1, 1), [0, 1, 2], "onto itself");
    }
}

enum MenuChoice {
    PlayNext,
    Append,
    Folder,
    /// Add to the playlist with this ID.
    AddTo(String),
    NewPlaylist,
    /// In the open playlist: remove the song at this position, or move it.
    Remove(usize),
    Move(usize, isize),
    /// Take the song out of the library (asks first).
    RemoveFromLibrary,
    GoToAlbum,
    GoToArtist,
}

/// What a song row's right-click menu offers besides playing.
#[derive(Default)]
struct RowMenu {
    /// "Add to playlist": the user's playlists (ID, name); None without a music server login.
    playlists: Option<Vec<(String, String)>>,
    /// On a playlist's page: the row's position and the playlist's length (Move, Remove).
    in_playlist: Option<(usize, usize)>,
    /// "Go to album" (not on the album's own page).
    go_to_album: bool,
}

enum RowClick {
    Play,
    Menu(MenuChoice),
}

/// One song in a list: number (or the playing bars), title, artist,
/// album, length. Click plays; right-click for more.
#[allow(clippy::too_many_arguments)]
fn song_row(ui: &mut egui::Ui, s: &Song, number: usize, show_artist: bool, is_now: bool, playing: bool, time: f64, album: Option<&str>, menu: &RowMenu) -> Option<RowClick> {
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
    let mut truncated = t.elided;
    ui.painter().galley(pos2(text_left, y - t.size().y / 2.0), t, title_color);
    let mut x = text_left + title_w;
    if show_artist && !s.artist.is_empty() {
        let w = if album.is_some() { rest_w * 0.45 } else { rest_w };
        let a = theme::one_line(ui, &s.artist, egui::FontId::proportional(13.5), p.dim, w - 12.0);
        truncated |= a.elided;
        ui.painter().galley(pos2(x, y - a.size().y / 2.0), a, p.dim);
        x += w;
    }
    if let Some(al) = album {
        let w = rest_w * 0.55;
        let a = theme::one_line(ui, al, egui::FontId::proportional(13.5), p.faint, w - 12.0);
        truncated |= a.elided;
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
    if (resp.clicked() || resp.double_clicked()) && !s.drm {
        out = Some(RowClick::Play);
    }
    let resp = if s.drm {
        resp.on_hover_text("Protected by iTunes DRM: can't be played")
    } else if truncated {
        let mut tip = s.title.clone();
        if show_artist && !s.artist.is_empty() {
            tip.push_str(&format!("\n{}", s.artist));
        }
        if let Some(al) = album {
            tip.push_str(&format!(" — {al}"));
        }
        resp.on_hover_text(tip)
    } else {
        resp
    };
    resp.context_menu(|ui| {
        if ui.add_enabled(!s.drm, egui::Button::new("Play next")).clicked() {
            out = Some(RowClick::Menu(MenuChoice::PlayNext));
            ui.close();
        }
        if ui.add_enabled(!s.drm, egui::Button::new("Add to queue")).clicked() {
            out = Some(RowClick::Menu(MenuChoice::Append));
            ui.close();
        }
        if let Some(lists) = &menu.playlists {
            ui.menu_button("Add to playlist", |ui| {
                for (id, name) in lists {
                    if ui.button(name).clicked() {
                        out = Some(RowClick::Menu(MenuChoice::AddTo(id.clone())));
                        ui.close();
                    }
                }
                if !lists.is_empty() {
                    ui.separator();
                }
                if ui.button("New playlist…").clicked() {
                    out = Some(RowClick::Menu(MenuChoice::NewPlaylist));
                    ui.close();
                }
            });
        }
        if menu.go_to_album && ui.button("Go to album").clicked() {
            out = Some(RowClick::Menu(MenuChoice::GoToAlbum));
            ui.close();
        }
        if ui.button("Go to artist").clicked() {
            out = Some(RowClick::Menu(MenuChoice::GoToArtist));
            ui.close();
        }
        // Studio's own items, after the shared ones.
        if let Some((pos, len)) = menu.in_playlist {
            ui.separator();
            if ui.add_enabled(pos > 0, egui::Button::new("Move up")).clicked() {
                out = Some(RowClick::Menu(MenuChoice::Move(pos, -1)));
                ui.close();
            }
            if ui.add_enabled(pos + 1 < len, egui::Button::new("Move down")).clicked() {
                out = Some(RowClick::Menu(MenuChoice::Move(pos, 1)));
                ui.close();
            }
            if ui.button("Remove from playlist").clicked() {
                out = Some(RowClick::Menu(MenuChoice::Remove(pos)));
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Show in folder").clicked() {
            out = Some(RowClick::Menu(MenuChoice::Folder));
            ui.close();
        }
        if ui.button("Remove from library…").clicked() {
            out = Some(RowClick::Menu(MenuChoice::RemoveFromLibrary));
            ui.close();
        }
    });
    out
}

/// From a verified CD rip: the FLAC library here, or the music server's FLAC
/// library (it serves an album from its FLAC rip; LocalBeat `GLOSSARY.md`).
fn is_cd_rip(s: &Song) -> bool {
    s.origin == Origin::Flac || (s.origin == Origin::Server && s.codec.eq_ignore_ascii_case("flac"))
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

fn song_details(path: &std::path::Path) -> Vec<(String, String)> {
    localbeat_core::library::song_details(path)
}

/// "2026-11-12" from seconds since 1970 (UTC; days-from-civil inverse).
fn date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{}-{m:02}-{d:02}", if m <= 2 { y + 1 } else { y })
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

/// "1 song", "12 songs".
fn files(n: usize) -> String {
    if n == 1 { "1 song".into() } else { format!("{n} songs") }
}
