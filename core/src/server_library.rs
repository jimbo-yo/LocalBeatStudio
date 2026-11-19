//! The LocalBeat server's albums in Studio's library (docs/server-library.md,
//! step 4): the server's catalogue as library songs, a copy of it on disk so
//! the library shows at once, and the song cache the player plays from.
//!
//! A server song's `path` is where it's cached here:
//! `<cache>/server-songs/<album id>/<song id>.<ext>`, the album's cover next
//! to it as `cover.jpg` (so the library's cover finder works unchanged).
//! Songs are downloaded when they're about to play, written straight to that
//! path with a `<file>.downloading` marker next to it until complete; the
//! player reads such a file as it grows (localbeat-player). The cache is
//! size-limited: the songs played longest ago go first.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Prefs, Settings};
use crate::library::{Origin, Song};
use crate::subsonic::Subsonic;

/// The song cache's size limit.
pub const CACHE_BYTES: u64 = 4 << 30;
/// Songs per catalogue request.
const PAGE: usize = 500;

pub fn songs_dir(s: &Settings) -> PathBuf {
    s.cache_dir.join("server-songs")
}

/// Ids are the server's own ("a1b2…", "s3c4…"); anything else is dropped so
/// an id can't name a path outside the cache.
fn safe_id(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

pub fn song_path(s: &Settings, album_id: &str, song_id: &str, suffix: &str) -> PathBuf {
    songs_dir(s).join(safe_id(album_id)).join(format!("{}.{}", safe_id(song_id), safe_id(suffix)))
}

/// The server's id for a cached song path (its file name), if it's one.
pub fn song_id(s: &Settings, path: &Path) -> Option<String> {
    path.strip_prefix(songs_dir(s)).ok()?;
    path.file_stem().map(|n| n.to_string_lossy().into_owned())
}

/// The album id of a cached song or cover path (its folder's name).
pub fn album_id(s: &Settings, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(songs_dir(s)).ok()?;
    rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())
}

pub fn marker(path: &Path) -> PathBuf {
    let mut m = path.as_os_str().to_owned();
    m.push(".downloading");
    PathBuf::from(m)
}

/// Downloaded in full.
pub fn complete(path: &Path) -> bool {
    path.is_file() && !marker(path).exists()
}

fn codec(suffix: &str) -> &'static str {
    match suffix {
        "flac" => "FLAC",
        "mp3" => "MP3",
        "m4a" | "m4p" | "aac" => "AAC",
        "ogg" | "oga" => "Vorbis",
        "opus" => "Opus",
        "wav" => "WAV",
        "aif" | "aiff" => "AIFF",
        _ => "",
    }
}

/// A library song from the server's song object.
pub fn song_from(s: &Settings, v: &Value) -> Option<Song> {
    let st = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let id = st("id");
    let album_id = v["albumId"].as_str().or(v["parent"].as_str())?.to_string();
    let suffix = st("suffix").to_ascii_lowercase();
    if id.is_empty() || suffix.is_empty() {
        return None;
    }
    let added = v["created"].as_str().and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok()).map(|d| d.timestamp()).unwrap_or(0);
    let album_artist = v["displayAlbumArtist"].as_str().or(v["albumArtist"].as_str()).unwrap_or("").to_string();
    Some(Song {
        path: song_path(s, &album_id, &id, &suffix),
        size: v["size"].as_u64().unwrap_or(0),
        mtime: added,
        added,
        title: st("title"),
        artist: st("artist"),
        album: st("album"),
        album_artist,
        track: v["track"].as_u64().map(|n| n as u32),
        disc: v["discNumber"].as_u64().map(|n| n as u32),
        year: v["year"].as_u64().filter(|&y| y > 0).map(|y| y as u16),
        genre: v["genre"].as_str().filter(|g| !g.is_empty()).map(str::to_string),
        seconds: v["duration"].as_f64().unwrap_or(0.0),
        codec: codec(&suffix).to_string(),
        sample_rate: v["samplingRate"].as_u64().map(|n| n as u32),
        bits: v["bitDepth"].as_u64().map(|n| n as u8),
        bitrate: v["bitRate"].as_u64().map(|n| n as u32),
        drm: suffix == "m4p",
        origin: Origin::Server,
        picture: false,
        server_path: v["path"].as_str().filter(|p| !p.is_empty()).map(str::to_string),
    })
}

/// The server's catalogue as last fetched, kept in `<cache>/server-library.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Catalogue {
    /// The server it came from (a different server: not used).
    pub server: String,
    pub fetched: String,
    pub songs: Vec<Song>,
}

impl Catalogue {
    fn file(s: &Settings) -> PathBuf {
        s.cache_dir.join("server-library.json")
    }

    /// The copy on disk, if it's from this server.
    pub fn load(s: &Settings, server: &str) -> Option<Catalogue> {
        let c: Catalogue = serde_json::from_slice(&std::fs::read(Self::file(s)).ok()?).ok()?;
        (c.server == server).then_some(c)
    }

    pub fn save(&self, s: &Settings) -> Result<()> {
        let f = Self::file(s);
        std::fs::create_dir_all(&s.cache_dir)?;
        let tmp = f.with_extension("new");
        std::fs::write(&tmp, serde_json::to_vec(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &f).with_context(|| format!("replacing {}", f.display()))
    }

    /// Every song on the server.
    pub fn fetch(s: &Settings, sub: &Subsonic, server: &str) -> Result<Catalogue> {
        let mut songs = Vec::new();
        for page in 0.. {
            let got = sub.songs(page * PAGE, PAGE)?;
            songs.extend(got.iter().filter_map(|v| song_from(s, v)));
            if got.len() < PAGE {
                break;
            }
        }
        Ok(Catalogue { server: server.to_string(), fetched: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), songs })
    }

    /// Same songs as `other` (what changes the library), ignoring when fetched.
    pub fn same_songs(&self, other: &Catalogue) -> bool {
        self.songs == other.songs
    }
}

/// The login for this computer's server, if it has one (the music server's
/// address and login: what playing needs; sending is separate).
pub fn client(s: &Settings) -> Option<(Subsonic, String)> {
    let p = Prefs::load(s).server;
    Subsonic::new(&p).map(|c| (c, p.url.trim().trim_end_matches('/').to_string()))
}

/// Makes sure a server album's `cover.jpg` is in its cache folder (`dir`).
/// `Ok(false)`: not a server album folder.
pub fn ensure_cover(s: &Settings, sub: &Subsonic, dir: &Path) -> Result<bool> {
    let Some(id) = dir.strip_prefix(songs_dir(s)).ok().and_then(|r| r.to_str()).map(safe_id) else { return Ok(false) };
    let f = dir.join("cover.jpg");
    if f.is_file() {
        return Ok(true);
    }
    let bytes = sub.cover(&id, 600)?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("cover.jpg.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &f)?;
    Ok(true)
}

// ---------------------------------------------------------------- the song cache

#[derive(Debug, Clone, Default)]
pub struct FetchStatus {
    /// Being downloaded now.
    pub current: Option<PathBuf>,
    /// The last download that failed, and why.
    pub error: Option<(PathBuf, String)>,
}

/// Downloads the songs about to play, in order, in the background.
pub struct Fetcher {
    tx: Sender<Vec<PathBuf>>,
    status: Arc<Mutex<FetchStatus>>,
}

impl Fetcher {
    /// Starts the download thread; leftovers of downloads cut short (the
    /// app quit) are removed first.
    pub fn start(s: Settings) -> Fetcher {
        clean_unfinished(&songs_dir(&s));
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(FetchStatus::default()));
        let st = status.clone();
        std::thread::Builder::new().name("server-songs".into()).spawn(move || run(s, rx, st)).expect("starting the download thread");
        Fetcher { tx, status }
    }

    /// The server songs to have ready, in the order they'll play (others in
    /// the list are ignored). Each one not yet here is marked as coming at
    /// once, so the player waits for it rather than skipping it.
    pub fn want(&self, paths: Vec<PathBuf>) {
        for p in &paths {
            if !complete(p) {
                prepare(p);
            }
        }
        let _ = self.tx.send(paths);
    }

    pub fn status(&self) -> FetchStatus {
        self.status.lock().unwrap().clone()
    }
}

/// Marker first, then the (empty) file: a reader never sees a file without
/// its marker until it's complete.
fn prepare(p: &Path) {
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::File::create(marker(p));
    if !p.exists() {
        let _ = std::fs::File::create(p);
    }
}

/// A file that won't be finished: gone, marker last.
fn discard(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(marker(p));
}

fn clean_unfinished(dir: &Path) {
    for e in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let p = e.path();
        if let Some(song) = p.to_str().and_then(|x| x.strip_suffix(".downloading")) {
            discard(Path::new(song));
        }
    }
}

fn run(s: Settings, rx: Receiver<Vec<PathBuf>>, status: Arc<Mutex<FetchStatus>>) {
    let mut wanted: Vec<PathBuf> = Vec::new();
    let latest = |wanted: &mut Vec<PathBuf>, block: bool| -> bool {
        let mut got = if block { rx.recv().ok() } else { None };
        if block && got.is_none() {
            return false;
        }
        while let Ok(w) = rx.try_recv() {
            got = Some(w);
        }
        if let Some(new) = got {
            // Ones no longer wanted and not started: their placeholders go.
            let keep: HashSet<&PathBuf> = new.iter().collect();
            for p in wanted.iter().filter(|p| !keep.contains(p) && !complete(p)) {
                discard(p);
            }
            *wanted = new;
        }
        true
    };
    loop {
        if !latest(&mut wanted, true) {
            return;
        }
        'list: loop {
            let todo: Vec<PathBuf> = wanted.clone();
            for p in &todo {
                if complete(p) {
                    // Recently wanted: the last to be evicted.
                    let _ = std::fs::File::options().write(true).open(p).and_then(|f| f.set_modified(SystemTime::now()));
                    continue;
                }
                status.lock().unwrap().current = Some(p.clone());
                let r = download(&s, p);
                let mut st = status.lock().unwrap();
                st.current = None;
                if let Err(e) = r {
                    discard(p);
                    st.error = Some((p.clone(), format!("{e:#}")));
                }
                drop(st);
                evict(&songs_dir(&s), &wanted, CACHE_BYTES);
                // A new list (another album, a skip): start over with it.
                let before = wanted.clone();
                latest(&mut wanted, false);
                if wanted != before {
                    continue 'list;
                }
            }
            break;
        }
    }
}

fn download(s: &Settings, p: &Path) -> Result<()> {
    let (sub, _) = client(s).context("no music server login set")?;
    let id = song_id(s, p).context("not a server song")?;
    if !marker(p).exists() {
        prepare(p);
    }
    let mut body = sub.download(&id)?;
    let mut f = std::fs::File::create(p).with_context(|| format!("writing {}", p.display()))?;
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = body.read(&mut buf).context("receiving the song")?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        // Let a reader see each piece as it lands.
        f.flush()?;
    }
    f.sync_data().ok();
    drop(f);
    std::fs::remove_file(marker(p)).ok();
    Ok(())
}

/// Removes the songs played longest ago while the cache is over `limit`
/// bytes (never one in `keep`, or one still downloading).
pub fn evict(dir: &Path, keep: &[PathBuf], limit: u64) {
    let keep: HashSet<&PathBuf> = keep.iter().collect();
    let mut files: Vec<(SystemTime, u64, PathBuf)> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let p = e.into_path();
            let name = p.file_name()?.to_string_lossy().into_owned();
            if name.starts_with("cover.") || name.ends_with(".downloading") || name.ends_with(".part") {
                return None;
            }
            let md = std::fs::metadata(&p).ok()?;
            Some((md.modified().ok()?, md.len(), p))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, p) in files {
        if total <= limit {
            break;
        }
        if keep.contains(&p) || marker(&p).exists() {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

/// How long the player waits for more of a song still downloading.
pub const WAIT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(name: &str) -> Settings {
        let dir = std::env::temp_dir().join(format!("localbeat-srvlib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Settings::load();
        s.cache_dir = dir.join("cache");
        s.config_dir = dir.join("config");
        s
    }

    #[test]
    fn songs_from_the_server() {
        let s = settings("song");
        let v = json!({
            "id": "s1a2", "albumId": "a9", "title": "Mojo Pin", "album": "Grace", "artist": "Jeff Buckley",
            "displayAlbumArtist": "Jeff Buckley", "track": 1, "discNumber": 1, "year": 1994, "genre": "Rock",
            "size": 31000000, "suffix": "flac", "duration": 342, "bitRate": 900, "created": "2026-11-14T10:00:00Z"
        });
        let x = song_from(&s, &v).unwrap();
        assert_eq!(x.path, songs_dir(&s).join("a9").join("s1a2.flac"));
        assert_eq!((x.origin, x.codec.as_str(), x.track, x.year, x.seconds), (Origin::Server, "FLAC", Some(1), Some(1994), 342.0));
        assert_eq!(x.added, 1_794_650_400);
        assert_eq!(song_id(&s, &x.path).as_deref(), Some("s1a2"));
        assert_eq!(album_id(&s, &x.path).as_deref(), Some("a9"));
        assert!(song_id(&s, Path::new("/home/x/Music/FLAC/a.flac")).is_none());
        // Ids can't climb out of the cache.
        let bad = song_from(&s, &json!({"id": "../../x", "albumId": "..", "suffix": "flac"})).unwrap();
        assert!(bad.path.starts_with(songs_dir(&s)) && !bad.path.to_string_lossy().contains(".."));
        let drm = song_from(&s, &json!({"id": "s2", "albumId": "a1", "suffix": "m4p"})).unwrap();
        assert!(drm.drm);
        assert!(song_from(&s, &json!({"id": "s3", "suffix": "flac"})).is_none());
    }

    #[test]
    fn placeholders_markers_and_eviction() {
        let s = settings("cache");
        let dir = songs_dir(&s);
        let a = song_path(&s, "a1", "s1", "flac");
        let b = song_path(&s, "a1", "s2", "flac");
        prepare(&a);
        assert!(a.is_file() && !complete(&a));
        std::fs::remove_file(marker(&a)).unwrap();
        assert!(complete(&a));
        prepare(&b);
        clean_unfinished(&dir);
        assert!(!b.exists() && !marker(&b).exists(), "unfinished downloads go at startup");
        assert!(a.exists());

        // Over the limit: the oldest goes, unless it's wanted.
        std::fs::write(&a, vec![0u8; 100]).unwrap();
        std::fs::write(&b, vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("a1").join("cover.jpg"), vec![0u8; 500]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&a).unwrap().set_modified(old).unwrap();
        evict(&dir, std::slice::from_ref(&a), 150);
        assert!(a.exists() && !b.exists());
        evict(&dir, &[], 150);
        assert!(a.exists(), "under the limit now");
        evict(&dir, &[], 50);
        assert!(!a.exists() && dir.join("a1").join("cover.jpg").exists(), "covers stay");
        std::fs::remove_dir_all(s.cache_dir.parent().unwrap()).ok();
    }

    /// Online: this computer's server must be set up and reachable.
    #[test]
    #[ignore]
    fn downloads_a_song_from_the_server() {
        let real = Settings::load();
        let (sub, url) = client(&real).expect("no server login set");
        let cat = Catalogue::load(&real, &url).expect("no catalogue kept yet (open Studio once)");
        let song = cat.songs.iter().find(|x| x.codec == "FLAC").expect("a FLAC song");
        let mut s = settings("online");
        s.config_dir = real.config_dir.clone();
        let p = song_path(&s, &album_id(&real, &song.path).unwrap(), &song_id(&real, &song.path).unwrap(), "flac");
        let f = Fetcher::start(s.clone());
        let t = std::time::Instant::now();
        f.want(vec![p.clone()]);
        assert!(p.exists() && !complete(&p), "placeholder at once");
        while !complete(&p) {
            assert!(t.elapsed() < Duration::from_secs(120), "{:?}", f.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..4], b"fLaC");
        assert_eq!(bytes.len() as u64, song.size, "the whole file");
        eprintln!("{} ({:.1} MB) in {:.1} s", song.title, bytes.len() as f64 / 1e6, t.elapsed().as_secs_f64());
        drop(sub);
        std::fs::remove_dir_all(s.cache_dir.parent().unwrap()).ok();
    }

    #[test]
    fn catalogue_round_trip() {
        let s = settings("cat");
        let song = song_from(&s, &json!({"id": "s1", "albumId": "a1", "suffix": "flac", "title": "T"})).unwrap();
        let c = Catalogue { server: "http://h:4533".into(), fetched: "now".into(), songs: vec![song] };
        c.save(&s).unwrap();
        assert_eq!(Catalogue::load(&s, "http://h:4533"), Some(c.clone()));
        assert_eq!(Catalogue::load(&s, "http://other:4533"), None);
        std::fs::remove_dir_all(s.cache_dir.parent().unwrap()).ok();
    }
}
