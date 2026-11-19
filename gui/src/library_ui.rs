//! The music side of the app: the library (Albums, Recently added, Artists,
//! Songs, search, album pages), the now-playing bar on every page, and the
//! Up Next queue. Playback is localbeat-player; the library comes from
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
    /// The user's playlists on the music server.
    Playlists,
}

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
    repeat: bool,
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
    pub page: Page,
    /// Open album page.
    pub album: Option<usize>,
    /// The search the open album was picked from: its back link returns to it.
    pub album_from_search: Option<String>,
    artist: Option<usize>,
    pub search: String,
    pub search_id: egui::Id,
    pub queue_open: bool,
    /// The full-size Now Playing view is showing.
    pub now_open: bool,
    now_tab: NowTab,
    /// Extra tags of the song in Details, read when it changes.
    details: Option<(PathBuf, Vec<(String, String)>)>,
    /// Scroll Up Next to the current song once after opening.
    scroll_to_now: bool,
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
    /// The music server's playlists (fetched in the background).
    pub playlists: crate::playlists_ui::Playlists,
    /// Open playlist page (its ID).
    pub playlist: Option<String>,
    name_dialog: Option<NameDialog>,
    /// A playlist waiting for "delete?" to be confirmed.
    confirm_delete: Option<String>,
    remove_dialog: Option<RemoveDialog>,
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
        player.set_repeat(session.repeat);
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
            page: Page::Albums,
            album: None,
            album_from_search: None,
            artist: None,
            search: String::new(),
            search_id: egui::Id::unique("library-search"),
            queue_open: false,
            now_open: false,
            now_tab: NowTab::Queue,
            details: None,
            scroll_to_now: false,
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
            playlists: crate::playlists_ui::Playlists::new(settings),
            playlist: None,
            name_dialog: None,
            confirm_delete: None,
            remove_dialog: None,
            ctx: None,
            show_library: false,
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
        }
        if finished && std::mem::take(&mut self.rescan_wanted) {
            self.rescan(ctx);
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
            "now" | "now-details" => {
                self.play(lib.albums[ok].songs.clone(), 3);
                self.open_now();
                if demo == "now-details" {
                    self.now_tab = NowTab::Details;
                }
            }
            "search" => self.search = "love".into(),
            "artists" => self.page = Page::Artists,
            "songs" => self.page = Page::Songs,
            "recent" => self.page = Page::Recent,
            "playlists" => self.page = Page::Playlists,
            "playlist" => {
                self.page = Page::Playlists;
                self.playlist = Some(DEMO_FIRST_PLAYLIST.into());
            }
            _ => {}
        }
    }

    /// Space, arrows, Ctrl+F (when no text field has the keyboard).
    pub fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if self.now_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.now_open = false;
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
                    self.now_open = false;
                    self.album = Some(i);
                    // Picked from a search: back goes to that search, not
                    // to an empty one.
                    let q = std::mem::take(&mut self.search);
                    self.album_from_search = (!q.trim().is_empty()).then_some(q);
                }
                Action::OpenArtist(i) => {
                    self.now_open = false;
                    self.artist = Some(i);
                    self.page = Page::Artists;
                    self.album = None;
                    self.album_from_search = None;
                    self.search.clear();
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
                    if let (Some(id), Some(ctx)) = (self.playlist.clone(), self.ctx.clone()) {
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
                    self.remove_dialog = Some(RemoveDialog { songs: v, what, plan, server_ok, state: RemoveState::Confirm });
                }
                Action::AskRemove(_) => {}
                Action::PlaylistMove(pos, by) => {
                    let (Some(id), Some(ctx)) = (self.playlist.clone(), self.ctx.clone()) else { continue };
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
    fn row_menu(&self, in_playlist: Option<(usize, usize)>) -> RowMenu {
        RowMenu { playlists: self.playlists.available().then(|| self.playlists.mine()), in_playlist }
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
        });
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
            self.now_open = false;
            self.search_page(ui, &lib);
        } else if self.now_open {
            self.now_view(ui, &lib);
        } else if !self.search.trim().is_empty() {
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
                    let mut subtitle = format!("{} albums · {} songs", lib.albums.len(), lib.songs.len());
                    match &self.server {
                        ServerView::Connected { .. } => {
                            let n = lib.albums.iter().filter(|a| a.origin == Origin::Server).count();
                            subtitle.push_str(&format!(" · {n} from the server"));
                        }
                        ServerView::NotConnected(_) => subtitle.push_str(" · server not connected"),
                        ServerView::Off => {}
                    }
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
                Page::Playlists => self.playlists_page(ui, &lib),
            }
        }
        self.playlist_dialogs(ui.ctx());
        self.remove_dialog(ui.ctx());
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

    /// The user's playlists on the music server (theirs, then shared with them).
    fn playlists_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>) {
        // LOCALBEAT_DEMO=playlist: the first playlist, once they're fetched.
        if self.playlist.as_deref() == Some(DEMO_FIRST_PLAYLIST) && self.playlists.loaded {
            self.playlist = self.playlists.lists.first().map(|p| p.id.clone());
        }
        if let Some(id) = self.playlist.clone() {
            if let Some(pl) = self.playlists.get(&id).cloned() {
                self.playlist_page(ui, lib, &pl);
                return;
            }
            // Gone (deleted, here or elsewhere) once the list is fresh.
            if self.playlists.loaded && !self.playlists.busy {
                self.playlist = None;
            }
        }
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
        egui::ScrollArea::vertical().id_salt("playlists").auto_shrink([false, false]).show(ui, |ui| {
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
                        self.playlist = Some(pl.id.clone());
                    }
                }
            }
        });
    }

    /// One playlist's songs, in order.
    fn playlist_page(&mut self, ui: &mut egui::Ui, lib: &Arc<Library>, pl: &crate::playlists_ui::Playlist) {
        let p = theme::pal(ui);
        if ui.add(egui::Label::new(egui::RichText::new("‹  Playlists").color(p.dim)).sense(Sense::click())).clicked() {
            self.playlist = None;
        }
        ui.add_space(8.0);
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
        egui::ScrollArea::vertical().id_salt(("playlist", &pl.id)).auto_shrink([false, false]).show_rows(ui, 38.0, rows.len(), |ui, range| {
            for k in range {
                let (pos, si) = rows[k];
                let s = &lib.songs[si];
                let menu = self.row_menu(if pl.readonly { None } else { Some((pos, len)) });
                let album = if s.album.is_empty() { None } else { Some(s.album.as_str()) };
                if let Some(click) = song_row(ui, s, k + 1, true, now == Some(si), playing, time, album, &menu) {
                    match click {
                        RowClick::Play => self.actions.push(Action::Play(order.clone(), k)),
                        RowClick::Menu(m) => self.menu_action(m, si, &s.path),
                    }
                }
            }
        });
    }

    /// The name box (new / rename) and "delete?" for playlists.
    /// "Remove from library": what goes where, then the removal in the
    /// background (the server's part waits for its scan), then how it went.
    fn remove_dialog(&mut self, ctx: &egui::Context) {
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
            let gone = |a: usize| self.lib.as_ref().is_some_and(|l| a < l.albums.len() && l.albums[a].songs.iter().all(|s| removed.contains(s)));
            if self.album.is_some_and(gone) {
                self.album = None;
                if let Some(q) = self.album_from_search.take() {
                    self.search = q;
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
        egui::Window::new("Remove from library?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                match &d.state {
                    RemoveState::Confirm => {
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
                            if ui.add_enabled_ui(possible, |ui| theme::button(ui, None, "Remove", Kind::Danger)).inner.clicked() {
                                go = true;
                            }
                            if theme::button(ui, None, "Cancel", Kind::Secondary).clicked() {
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
                self.playlist = None;
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

        let back = match (&self.album_from_search, self.page) {
            (Some(q), _) => format!("Search “{}”", q.trim()),
            (None, Page::Recent) => "Recently added".into(),
            (None, Page::Artists) => "Artists".into(),
            (None, Page::Songs) => "Songs".into(),
            (None, Page::Albums) => "Albums".into(),
            (None, Page::Playlists) => "Playlists".into(),
        };
        let clicked = ui.add(egui::Label::new(egui::RichText::new(format!("‹  {back}")).color(p.dim)).sense(Sense::click())).clicked();
        // Esc goes back too (as on Now Playing), unless typing somewhere.
        let esc = ui.ctx().memory(|m| m.focused().is_none()) && ui.input(|i| i.key_pressed(egui::Key::Escape));
        if clicked || esc {
            self.album = None;
            if let Some(q) = self.album_from_search.take() {
                self.search = q;
            }
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
                    if album.origin == Origin::Server {
                        theme::badge(ui, "ON THE SERVER", theme::ACCENT).on_hover_text("Not on this computer: played from the LocalBeat server");
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
        let menu = self.row_menu(None);
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
        let menu = self.row_menu(None);
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show_rows(ui, 38.0, order.len(), |ui, range| {
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
            if resp.on_hover_text("Now Playing").clicked() && song.is_some() {
                // As the sidebar's Now Playing: from any tab, and over a search.
                self.search.clear();
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

    /// Opens the full-size Now Playing view.
    pub fn open_now(&mut self) {
        self.now_open = true;
        self.scroll_to_now = true;
    }

    /// The full-size Now Playing view: big cover, names, transport on the
    /// left; Up Next or the song's details on the right.
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

        let back = ui.horizontal(|ui| {
            let r = theme::icon_button(ui, Icon::Back, 30.0, "Back (Esc)");
            let l = ui.add(egui::Label::new(egui::RichText::new("Now Playing").font(theme::medium(15.0)).color(p.dim)).sense(Sense::click()));
            r.clicked() || l.clicked()
        });
        if back.inner {
            self.now_open = false;
            return;
        }
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
                if x.origin == Origin::Flac {
                    theme::badge(ui, "CD RIP", theme::OK).on_hover_text("Ripped and verified with LocalBeat Studio");
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
                let sh = theme::icon_button(ui, Icon::Shuffle, 40.0, "Shuffle (when you next play an album or list)");
                if self.shuffle {
                    ui.painter().circle_filled(sh.rect.center_bottom() + vec2(0.0, -2.0), 2.5, theme::prism_at(0.3));
                }
                if sh.clicked() {
                    self.shuffle = !self.shuffle;
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
                let rp = theme::icon_button(ui, Icon::Repeat, 40.0, "Repeat the queue");
                if self.repeat {
                    ui.painter().circle_filled(rp.rect.center_bottom() + vec2(0.0, -2.0), 2.5, theme::prism_at(0.3));
                }
                if rp.clicked() {
                    self.repeat = !self.repeat;
                    self.player.set_repeat(self.repeat);
                }
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

        // ---- right: Up Next / Details
        ui.scope_builder(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in [(NowTab::Queue, "Up Next"), (NowTab::Details, "Details")] {
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

    /// Up Next in the Now Playing view: the whole queue, current one marked.
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
    fn now_details(&mut self, ui: &mut egui::Ui, x: &Song, _si: usize) {
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
                Origin::Server => "The LocalBeat server",
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
        egui::ScrollArea::vertical().id_salt("now-details").auto_shrink([false, false]).show(ui, |ui| {
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
    /// Add to the playlist with this ID.
    AddTo(String),
    NewPlaylist,
    /// In the open playlist: remove the song at this position, or move it.
    Remove(usize),
    Move(usize, isize),
    /// Take the song out of the library (asks first).
    RemoveFromLibrary,
}

/// What a song row's right-click menu offers besides playing.
#[derive(Default)]
struct RowMenu {
    /// "Add to playlist": the user's playlists (ID, name); None without a music server login.
    playlists: Option<Vec<(String, String)>>,
    /// On a playlist's page: the row's position and the playlist's length (Move, Remove).
    in_playlist: Option<(usize, usize)>,
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
    if (resp.clicked() || resp.double_clicked()) && !s.drm {
        out = Some(RowClick::Play);
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
