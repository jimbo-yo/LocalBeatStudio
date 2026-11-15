//! Shared HTTP client, and MusicBrainz's API rules (1 request/s, retry 503).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ureq::Agent;

pub const USER_AGENT: &str = concat!("LocalBeatStudio/", env!("CARGO_PKG_VERSION"));

pub fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into()
    })
}

const MB_API: &str = "https://musicbrainz.org/ws/2/";

/// GET a MusicBrainz JSON resource (`path` relative to /ws/2/, without
/// fmt=json). At most one request per 1.1 s across threads; 503/429 (rate
/// limited) and network errors are retried a few times. `Ok(None)` on 404.
pub fn musicbrainz(path: &str) -> Result<Option<serde_json::Value>> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let sep = if path.contains('?') { '&' } else { '?' };
    let url = format!("{MB_API}{path}{sep}fmt=json");
    let mut attempt = 0;
    loop {
        {
            let mut last = LAST.lock().unwrap();
            if let Some(t) = *last {
                let wait = Duration::from_millis(1100).saturating_sub(t.elapsed());
                std::thread::sleep(wait);
            }
            *last = Some(Instant::now());
        }
        attempt += 1;
        match agent().get(&url).call() {
            Ok(mut resp) => {
                return resp.body_mut().read_json().map(Some).with_context(|| format!("reading {url}"));
            }
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(ureq::Error::StatusCode(503 | 429)) if attempt < 5 => {
                std::thread::sleep(Duration::from_secs(2 * attempt));
            }
            Err(ureq::Error::StatusCode(code)) => anyhow::bail!("MusicBrainz returned HTTP {code} for {url}"),
            Err(_) if attempt < 5 => std::thread::sleep(Duration::from_secs(3)),
            Err(e) => return Err(e).with_context(|| format!("requesting {url}")),
        }
    }
}
