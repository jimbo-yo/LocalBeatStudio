//! A small Subsonic API client for the LocalBeat server: the calls Studio
//! needs after sending an album (scan, and is it in the catalogue yet).
//! Playing from the server (step 4 of docs/server-library.md) adds more.
//!
//! Login is the salted token (`t = md5(password + salt)`), so the secret
//! itself never goes over the wire (the server accepts both).

use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Calls `method` and returns the `subsonic-response` object, or the
    /// server's error message.
    fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}/rest/{method}", self.base);
        let salt = salt();
        let mut req = crate::http::agent()
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
        let mut resp = req.call().with_context(|| format!("asking the server ({url})"))?;
        let v: Value = resp.body_mut().read_json().with_context(|| format!("reading the reply from {url}"))?;
        parse(v)
    }

    pub fn ping(&self) -> Result<()> {
        self.call("ping", &[]).map(|_| ())
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
