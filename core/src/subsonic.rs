//! A small Subsonic API client for the LocalBeat server: the calls Studio
//! needs after sending an album (scan, and is it in the catalogue yet),
//! playing from the server (docs/server-library.md), the user's
//! playlists on the server (LocalBeat docs/control.md), and the server's
//! status (its computer and disks: the Settings page's "Server status").
//!
//! Login is the salted token (`t = md5(password + salt)`), so the secret
//! itself never goes over the wire (the server accepts both).

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::config::ServerPrefs;

/// API version we speak (Navidrome supports it).
const VERSION: &str = "1.16.1";
const CLIENT: &str = "localbeat-studio";

pub struct Subsonic {
    base: String,
    user: String,
    password: String,
}

/// A salt that's new each call (no need for it to be secret).
fn salt() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:x}{:x}", n, std::process::id())
}

pub fn token(password: &str, salt: &str) -> String {
    format!("{:x}", md5::compute(format!("{password}{salt}")))
}

impl Subsonic {
    /// None if the settings don't have a music server login.
    /// When `p.tunnel` is set, routes through the shared embedded WireGuard
    /// loopback bridge (`core/src/tunnel.rs`); when `None`, connects directly
    /// (co-located loopback `127.0.0.1` or legacy OS tunnel).
    pub fn new(p: &ServerPrefs) -> Option<Self> {
        if p.url.trim().is_empty() || p.login.trim().is_empty() || p.secret.is_empty() {
            return None;
        }
        let base = crate::tunnel::resolve_base_url(p, None, None)
            .unwrap_or_else(|_| p.url.trim().trim_end_matches('/').to_string());
        Some(Subsonic {
            base,
            user: p.login.trim().to_string(),
            password: p.secret.clone(),
        })
    }

    /// A request for `method` with the login and `params`.
    fn request(&self, agent: &ureq::Agent, method: &str, params: &[(&str, &str)]) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        let url = format!("{}/rest/{method}", self.base);
        let salt = salt();
        let mut req = agent
            .get(&url)
            .query("u", &self.user)
            .query("t", token(&self.password, &salt))
            .query("s", &salt)
            .query("v", VERSION)
            .query("c", CLIENT)
            .query("f", "json");
        for (k, v) in params {
            req = req.query(*k, *v);
        }
        req
    }

    /// Calls `method` and returns the `subsonic-response` object, or the
    /// server's error message.
    fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        self.call_with(crate::http::agent(), method, params)
    }

    fn call_with(&self, agent: &ureq::Agent, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}/rest/{method}", self.base);
        let mut resp = self.request(agent, method, params).call().with_context(|| format!("asking the server ({url})"))?;
        let v: Value = resp.body_mut().read_json().with_context(|| format!("reading the reply from {url}"))?;
        parse(v)
    }

    pub fn ping(&self) -> Result<()> {
        self.call("ping", &[]).map(|_| ())
    }

    /// A ping that gives up after a few seconds: is the server there now?
    pub fn reachable(&self) -> Result<()> {
        self.call_with(quick_agent(), "ping", &[]).map(|_| ())
    }

    /// Songs `offset..offset + count` of the whole catalogue (search3 with
    /// an empty query lists everything), as the server's song objects.
    pub fn songs(&self, offset: usize, count: usize) -> Result<Vec<Value>> {
        let (o, c) = (offset.to_string(), count.to_string());
        let v = self.call("search3", &[("query", ""), ("artistCount", "0"), ("albumCount", "0"), ("songOffset", &o), ("songCount", &c)])?;
        Ok(v["searchResult3"]["song"].as_array().cloned().unwrap_or_default())
    }

    /// The song's original file, as it comes (for writing to disk as it
    /// arrives).
    pub fn download(&self, id: &str) -> Result<impl Read + use<>> {
        let resp = self.request(transfer_agent(), "download", &[("id", id)]).call().context("downloading from the server")?;
        let json = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("json"));
        if json {
            // An error comes back as a Subsonic reply, not as audio.
            let v: Value = resp.into_body().read_json().context("the server's reply")?;
            parse(v)?;
            bail!("the server sent no file");
        }
        Ok(resp.into_body().into_reader())
    }

    /// The cover image `id` (an album's `coverArt`), `size` pixels.
    pub fn cover(&self, id: &str, size: u32) -> Result<Vec<u8>> {
        let s = size.to_string();
        let resp = self.request(transfer_agent(), "getCoverArt", &[("id", id), ("size", &s)]).call().context("fetching a cover")?;
        let json = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("json"));
        let mut body = resp.into_body();
        if json {
            parse(body.read_json().context("the server's reply")?)?;
            bail!("no cover");
        }
        body.with_config().limit(20 << 20).read_to_vec().context("reading a cover")
    }

    /// Asks the server to look for new and changed files now.
    pub fn start_scan(&self) -> Result<()> {
        self.call("startScan", &[]).map(|_| ())
    }

    /// Whether a scan is running.
    pub fn scanning(&self) -> Result<bool> {
        let v = self.call("getScanStatus", &[])?;
        Ok(v["scanStatus"]["scanning"].as_bool().unwrap_or(false))
    }

    /// Whether the catalogue has an album called `album` by `artist`
    /// (compared case-insensitively; an empty artist matches any).
    pub fn has_album(&self, artist: &str, album: &str) -> Result<bool> {
        let v = self.call("search3", &[("query", album), ("albumCount", "100"), ("artistCount", "0"), ("songCount", "0")])?;
        Ok(albums_match(&v, artist, album))
    }

    /// Every song on the server, as its song objects (search3 pages).
    pub fn all_songs(&self) -> Result<Vec<Value>> {
        const PAGE: usize = 500;
        let mut all = Vec::new();
        for page in 0.. {
            let got = self.songs(page * PAGE, PAGE)?;
            let n = got.len();
            all.extend(got);
            if n < PAGE {
                break;
            }
        }
        Ok(all)
    }

    /// Every album (getAlbumList2 pages): `year` is the original release
    /// year on the LocalBeat server, `genres`, `songCount`.
    pub fn all_albums(&self) -> Result<Vec<Value>> {
        const PAGE: usize = 500;
        let mut all = Vec::new();
        for page in 0.. {
            let (o, c) = ((page * PAGE).to_string(), PAGE.to_string());
            let v = self.call("getAlbumList2", &[("type", "alphabeticalByName"), ("size", &c), ("offset", &o)])?;
            let got = v["albumList2"]["album"].as_array().cloned().unwrap_or_default();
            let n = got.len();
            all.extend(got);
            if n < PAGE {
                break;
            }
        }
        Ok(all)
    }

    /// Calls `method` with `params` in a form POST body (OpenSubsonic
    /// `formPost`: a long song list doesn't fit a URL). The login stays in
    /// the query string.
    fn post(&self, method: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/rest/{method}", self.base);
        let salt = salt();
        let mut resp = crate::http::agent()
            .post(&url)
            .query("u", &self.user)
            .query("t", token(&self.password, &salt))
            .query("s", &salt)
            .query("v", VERSION)
            .query("c", CLIENT)
            .query("f", "json")
            .send_form(params.iter().map(|(k, v)| (*k, v.as_str())))
            .with_context(|| format!("asking the server ({url})"))?;
        let v: Value = resp.body_mut().read_json().with_context(|| format!("reading the reply from {url}"))?;
        parse(v)
    }

    /// The user's playlists, those shared with them and public ones (no songs).
    pub fn playlists(&self) -> Result<Vec<Value>> {
        let v = self.call("getPlaylists", &[])?;
        Ok(v["playlists"]["playlist"].as_array().cloned().unwrap_or_default())
    }

    /// One playlist with its songs (`entry`).
    pub fn playlist(&self, id: &str) -> Result<Value> {
        Ok(self.call("getPlaylist", &[("id", id)])?["playlist"].clone())
    }

    /// A new playlist with these songs, in this order. Returns it.
    pub fn create_playlist(&self, name: &str, song_ids: &[String]) -> Result<Value> {
        let mut p = vec![("name", name.to_string())];
        p.extend(song_ids.iter().map(|id| ("songId", id.clone())));
        Ok(self.post("createPlaylist", &p)?["playlist"].clone())
    }

    /// Replaces a playlist's songs (how a playlist is reordered). Returns it.
    pub fn set_playlist_songs(&self, id: &str, song_ids: &[String]) -> Result<Value> {
        let mut p = vec![("playlistId", id.to_string())];
        p.extend(song_ids.iter().map(|s| ("songId", s.clone())));
        Ok(self.post("createPlaylist", &p)?["playlist"].clone())
    }

    /// Changes a playlist; only what's set in `c` changes.
    pub fn update_playlist(&self, id: &str, c: &PlaylistChange) -> Result<()> {
        let mut p = vec![("playlistId", id.to_string())];
        if let Some(n) = &c.name {
            p.push(("name", n.clone()));
        }
        if let Some(n) = &c.comment {
            p.push(("comment", n.clone()));
        }
        if let Some(b) = c.public {
            p.push(("public", b.to_string()));
        }
        p.extend(c.remove.iter().map(|i| ("songIndexToRemove", i.to_string())));
        p.extend(c.add.iter().map(|s| ("songIdToAdd", s.clone())));
        if let Some(users) = &c.shared_with {
            if users.is_empty() {
                p.push(("sharedWith", String::new()));
            }
            p.extend(users.iter().map(|u| ("sharedWith", u.clone())));
        }
        self.post("updatePlaylist", &p).map(|_| ())
    }

    pub fn delete_playlist(&self, id: &str) -> Result<()> {
        self.call("deletePlaylist", &[("id", id)]).map(|_| ())
    }

    /// The server computer, its disks and the library's size (LocalBeat
    /// server's `localbeatServerStatus` extension). fastfetch can take a few
    /// seconds, and the server adds up the library's folders first.
    pub fn server_status(&self) -> Result<ServerStatus> {
        let v = self.call("getServerStatus", &[]).map_err(|e| {
            if e.to_string().contains("Not implemented") {
                anyhow!("this server doesn't report its status yet (it needs updating)")
            } else {
                e
            }
        })?;
        serde_json::from_value(v["serverStatus"].clone()).context("reading the server's status")
    }

    /// The server's users (people to share a playlist with).
    pub fn users(&self) -> Result<Vec<String>> {
        let v = self.call("getUsers", &[])?;
        Ok(v["users"]["user"].as_array().into_iter().flatten().filter_map(|u| u["username"].as_str().map(str::to_string)).collect())
    }
}

/// [Subsonic::server_status]: the `serverStatus` object.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerStatus {
    pub version: String,
    pub uptime_secs: u64,
    pub songs: u64,
    pub albums: u64,
    pub scanning: bool,
    /// ISO 8601, UTC.
    pub last_scan: Option<String>,
    /// fastfetch's report, plain text (or why there's none: `fastfetch_error`).
    pub fastfetch: Option<String>,
    pub fastfetch_error: Option<String>,
    pub folders: Vec<ServerFolder>,
    pub disks: Vec<ServerDisk>,
    pub opus: OpusProgress,
    pub backup: Option<BackupStatus>,
}

/// Encrypted backup summary from `localbeat-backup` (`/var/lib/localbeat/backup-status.json`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupStatus {
    pub state: String,
    pub paused: bool,
    pub running: bool,
    pub seeded: bool,
    pub seed_percent: f64,
    pub seed_included_bytes: u64,
    pub seed_total_bytes: u64,
    pub last_run_at: Option<String>,
    pub last_run_ok: Option<bool>,
    pub last_error: Option<String>,
    pub snapshot_count: u64,
    pub last_check_at: Option<String>,
    pub last_check_ok: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerFolder {
    /// "FLAC", "Miscellaneous", "Removed", "Opus copies".
    pub name: String,
    /// "library", "removed" (songs taken out, kept for undo) or "cache".
    pub kind: String,
    pub path: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerDisk {
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
    /// The folders on it (as [ServerFolder::name]), and "System" for `/`.
    pub holds: Vec<String>,
}

/// FLAC songs with their Opus copy made, of all that need one.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OpusProgress {
    pub made: u64,
    pub needed: u64,
}

/// What [Subsonic::update_playlist] changes: `remove` holds positions in
/// the playlist as it is now (counted from 0), done before `add`.
#[derive(Debug, Default, Clone)]
pub struct PlaylistChange {
    pub name: Option<String>,
    pub comment: Option<String>,
    pub public: Option<bool>,
    pub remove: Vec<usize>,
    pub add: Vec<String>,
    /// The new list of users it's shared with (empty: nobody).
    pub shared_with: Option<Vec<String>>,
}

/// For "is it there": a few seconds at most.
fn quick_agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(crate::http::USER_AGENT)
            .timeout_global(Some(Duration::from_secs(4)))
            .build()
            .into()
    })
}

/// For songs and covers: no limit on the whole transfer (a long FLAC over a
/// slow link), but give up when the server stops answering.
fn transfer_agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(crate::http::USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .build()
            .into()
    })
}

fn parse(v: Value) -> Result<Value> {
    let r = v.get("subsonic-response").cloned().ok_or_else(|| anyhow!("not a Subsonic reply"))?;
    if r["status"].as_str() == Some("ok") {
        return Ok(r);
    }
    let msg = r["error"]["message"].as_str().unwrap_or("unknown error");
    match r["error"]["code"].as_i64() {
        Some(40 | 41) => bail!("the server refused the login: {msg}"),
        _ => bail!("the server said: {msg}"),
    }
}

fn albums_match(r: &Value, artist: &str, album: &str) -> bool {
    let same = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    r["searchResult3"]["album"].as_array().is_some_and(|list| {
        list.iter().any(|a| {
            a["name"].as_str().is_some_and(|n| same(n, album)) && (artist.is_empty() || a["artist"].as_str().is_some_and(|n| same(n, artist)))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_is_md5_of_password_and_salt() {
        // The example in the Subsonic API documentation.
        assert_eq!(token("sesame", "c19b2d"), "26719a1196d2a940705a59634eb18eab");
    }

    #[test]
    fn replies() {
        let ok = json!({"subsonic-response": {"status": "ok", "version": "1.16.1"}});
        assert!(parse(ok).is_ok());
        let bad = json!({"subsonic-response": {"status": "failed", "error": {"code": 40, "message": "Wrong username or password"}}});
        assert!(parse(bad).unwrap_err().to_string().contains("refused the login"));
        assert!(parse(json!({"x": 1})).is_err());
    }

    #[test]
    fn server_status_reply() {
        let v = json!({
            "version": "0.1.0", "uptimeSecs": 3600, "songs": 9000, "albums": 700, "scanning": false,
            "lastScan": "2026-11-19T10:00:00Z", "fastfetchError": "fastfetch isn't installed on the server",
            "folders": [{"name": "FLAC", "kind": "library", "path": "/srv/music/FLAC", "files": 8000, "bytes": 90000000000u64}],
            "disks": [{"mount": "/srv/music", "total": 800000000000u64, "used": 96000000000u64, "available": 705000000000u64, "holds": ["FLAC"]}],
            "opus": {"made": 8990, "needed": 9000},
            "somethingNew": 1
        });
        let s: ServerStatus = serde_json::from_value(v).unwrap();
        assert_eq!(s.uptime_secs, 3600);
        assert!(s.fastfetch.is_none() && s.fastfetch_error.is_some());
        assert_eq!(s.folders[0].bytes, 90_000_000_000);
        assert_eq!(s.disks[0].holds, ["FLAC"]);
        assert_eq!(s.opus.made, 8990);
        // An older reply without some fields still reads.
        let s: ServerStatus = serde_json::from_value(json!({"songs": 3})).unwrap();
        assert!(s.disks.is_empty() && s.opus.needed == 0);
    }

    #[test]
    fn album_search() {
        let r = json!({"searchResult3": {"album": [
            {"name": "OK Computer OKNOTOK", "artist": "Radiohead"},
            {"name": "OK Computer", "artist": "Radiohead"}
        ]}});
        assert!(albums_match(&r, "radiohead", "ok computer"));
        assert!(albums_match(&r, "", "OK Computer"));
        assert!(!albums_match(&r, "Someone Else", "OK Computer"));
        assert!(!albums_match(&json!({"searchResult3": {}}), "", "OK Computer"));
    }
}
