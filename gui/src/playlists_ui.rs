//! The user's playlists on the music server, for the Library page
//! (LocalBeat docs/control.md, step 6). The same playlists as on the phone and
//! to an AI (`localbeat mcp`): the server keeps them. Every call runs on a
//! background thread; the page shows what was last fetched.

use std::collections::HashMap;

use eframe::egui;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};

use localbeat_core::config::Settings;
use localbeat_core::server_library;
use localbeat_core::subsonic::{PlaylistChange, Subsonic};
use serde_json::Value;

/// A playlist as the page needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    /// The user it belongs to.
    pub owner: String,
    /// Someone else's (shared with this user, or public): play only.
    pub readonly: bool,
    pub comment: String,
    /// The server's song IDs, in order.
    pub songs: Vec<String>,
    /// Each song's (artist, title, album), as the server lists it: to find
    /// it when the library shows a local copy instead of the server's.
    pub info: Vec<(String, String, String)>,
    /// Songs the server no longer finds.
    pub missing: u64,
}

impl Playlist {
    fn from_json(v: &Value) -> Option<Playlist> {
        let st = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let id = st("id");
        (!id.is_empty()).then(|| Playlist {
            id,
            name: st("name"),
            owner: st("owner"),
            readonly: v["readonly"].as_bool().unwrap_or(false),
            comment: st("comment"),
            songs: v["entry"].as_array().into_iter().flatten().filter_map(|e| e["id"].as_str().map(str::to_string)).collect(),
            info: v["entry"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["id"].is_string())
                .map(|e| {
                    let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
                    (s("artist"), s("title"), s("album"))
                })
                .collect(),
            missing: v["missing"].as_u64().unwrap_or(0),
        })
    }
}

/// A change to send. Positions count from 0.
pub enum Edit {
    Create { name: String, songs: Vec<String> },
    Add { id: String, songs: Vec<String> },
    Remove { id: String, positions: Vec<usize> },
    SetSongs { id: String, songs: Vec<String> },
    Rename { id: String, name: String },
    Delete { id: String },
}

type Fetched = Result<(Vec<Playlist>, Option<String>), String>;

pub struct Playlists {
    settings: Settings,
    pub lists: Vec<Playlist>,
    pub loaded: bool,
    pub busy: bool,
    pub error: Option<String>,
    /// Something done, for a moment ("Added 3 songs to Road trip").
    pub notice: Option<String>,
    tx: Sender<Fetched>,
    rx: Receiver<Fetched>,
    /// This computer has a music server login (checked at start and on refresh).
    login: bool,
    /// Song ID → library index, for the library it was built from.
    index: Option<(usize, HashMap<String, usize>)>,
    /// Album + title (folded) → library indices, for the same library.
    by_name: HashMap<String, Vec<usize>>,
}

/// All playlists with their songs.
fn fetch(sub: &Subsonic) -> anyhow::Result<Vec<Playlist>> {
    let mut out = Vec::new();
    for p in sub.playlists()? {
        if let Some(id) = p["id"].as_str() {
            out.extend(Playlist::from_json(&sub.playlist(id)?));
        }
    }
    Ok(out)
}

impl Playlists {
    pub fn new(settings: &Settings) -> Self {
        let (tx, rx) = channel();
        let login = server_library::client(settings).is_some();
        Playlists { settings: settings.clone(), lists: Vec::new(), loaded: false, busy: false, error: None, notice: None, tx, rx, login, index: None, by_name: HashMap::new() }
    }

    fn client(&self) -> Option<Subsonic> {
        server_library::client(&self.settings).map(|(c, _)| c)
    }

    /// Whether this computer has a music server login (as of the last refresh).
    pub fn available(&self) -> bool {
        self.login
    }

    /// Fetches the playlists again (in the background).
    pub fn refresh(&mut self, ctx: &egui::Context) {
        self.login = self.client().is_some();
        self.run(ctx, None, None);
    }

    /// Sends a change, then fetches the playlists again. `done` is said once it worked.
    pub fn edit(&mut self, ctx: &egui::Context, e: Edit, done: impl Into<String>) {
        self.run(ctx, Some(e), Some(done.into()));
    }

    fn run(&mut self, ctx: &egui::Context, e: Option<Edit>, done: Option<String>) {
        let Some(sub) = self.client() else {
            self.error = Some("This computer has no music server login (Settings: join the server).".into());
            return;
        };
        self.busy = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<Vec<Playlist>> {
                if let Some(e) = e {
                    match e {
                        Edit::Create { name, songs } => drop(sub.create_playlist(&name, &songs)?),
                        Edit::Add { id, songs } => sub.update_playlist(&id, &PlaylistChange { add: songs, ..Default::default() })?,
                        Edit::Remove { id, positions } => sub.update_playlist(&id, &PlaylistChange { remove: positions, ..Default::default() })?,
                        Edit::SetSongs { id, songs } => drop(sub.set_playlist_songs(&id, &songs)?),
                        Edit::Rename { id, name } => sub.update_playlist(&id, &PlaylistChange { name: Some(name), ..Default::default() })?,
                        Edit::Delete { id } => sub.delete_playlist(&id)?,
                    }
                }
                fetch(&sub)
            })();
            let _ = tx.send(r.map(|l| (l, done)).map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
    }

    /// Takes in what the background thread found. Call every frame.
    pub fn poll(&mut self) {
        while let Ok(r) = self.rx.try_recv() {
            self.busy = false;
            self.loaded = true;
            match r {
                Ok((lists, done)) => {
                    self.lists = lists;
                    self.error = None;
                    if done.is_some() {
                        self.notice = done;
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Playlist> {
        self.lists.iter().find(|p| p.id == id)
    }

    /// The user's own playlists (the ones songs can be added to).
    pub fn mine(&self) -> Vec<(String, String)> {
        self.lists.iter().filter(|p| !p.readonly).map(|p| (p.id.clone(), p.name.clone())).collect()
    }

    /// The server's song ID for a library song, if it's one of the server's.
    pub fn song_id(&self, path: &Path) -> Option<String> {
        server_library::song_id(&self.settings, path)
    }

    /// Library indices of a playlist's songs (those in the library now), with
    /// their positions in the playlist. A server song the library hides
    /// because this computer has the album too (the local copy wins) is
    /// found by album + title, preferring the same artist: else it would be
    /// left out (2 of 25 in "Electronic", from a local Trance Nation: America).
    pub fn songs_in(&mut self, p: &Playlist, songs: &[localbeat_core::library::Song]) -> Vec<(usize, usize)> {
        use localbeat_core::library::norm;
        let key = songs.as_ptr() as usize ^ songs.len();
        if self.index.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut map = HashMap::new();
            let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, s) in songs.iter().enumerate() {
                if let Some(id) = self.song_id(&s.path) {
                    map.insert(id, i);
                }
                by_name.entry(format!("{}\u{1}{}", norm(&s.album), norm(&s.title))).or_default().push(i);
            }
            self.index = Some((key, map));
            self.by_name = by_name;
        }
        let map = &self.index.as_ref().unwrap().1;
        p.songs
            .iter()
            .enumerate()
            .filter_map(|(pos, id)| {
                if let Some(&i) = map.get(id) {
                    return Some((pos, i));
                }
                let (artist, title, album) = p.info.get(pos)?;
                let found = self.by_name.get(&format!("{}\u{1}{}", norm(album), norm(title)))?;
                let a = norm(artist);
                let i = found.iter().copied().find(|&i| norm(&songs[i].artist) == a).or_else(|| found.first().copied())?;
                Some((pos, i))
            })
            .collect()
    }
}
