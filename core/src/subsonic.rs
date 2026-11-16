//! A small Subsonic API client for the LocalBeat server: the calls Studio
//! needs after sending an album (scan, and is it in the catalogue yet).
//! Playing from the server (step 4 of docs/server-library.md) adds more.
//!
//! Login is the salted token (`t = md5(password + salt)`), so the secret
//! itself never goes over the wire (the server accepts both).

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
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
    pub fn new(p: &ServerPrefs) -> Option<Self> {
        (!p.url.trim().is_empty() && !p.login.trim().is_empty() && !p.secret.is_empty()).then(|| Subsonic {
            base: p.url.trim().trim_end_matches('/').to_string(),
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
