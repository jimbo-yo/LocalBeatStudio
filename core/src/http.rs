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
/// fmt=json). At most one request per 1.1 s across threads; network errors
/// are retried a few times. `Ok(None)` on 404.
pub fn musicbrainz(path: &str) -> Result<Option<serde_json::Value>> {
    musicbrainz_with(path, &|_| {})
}

/// How long a busy MusicBrainz (503/429) is waited out per request: it
/// answers "currently busy" for minutes at a time when it's overloaded
/// (2026-11-17, during a two-drive rip), and giving up fails the disc.
const MB_BUSY_PATIENCE: Duration = Duration::from_secs(180);

/// [`musicbrainz`], telling `waiting` how many seconds it waits each time
/// MusicBrainz says it's busy (so a rip can say so instead of looking stuck).
pub fn musicbrainz_with(path: &str, waiting: &dyn Fn(u64)) -> Result<Option<serde_json::Value>> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let sep = if path.contains('?') { '&' } else { '?' };
    let url = format!("{MB_API}{path}{sep}fmt=json");
    let started = Instant::now();
    let (mut busy, mut failures) = (0u32, 0u32);
    loop {
        {
            let mut last = LAST.lock().unwrap();
            if let Some(t) = *last {
                let wait = Duration::from_millis(1100).saturating_sub(t.elapsed());
                std::thread::sleep(wait);
            }
            *last = Some(Instant::now());
        }
        let resp = agent().get(&url).config().http_status_as_error(false).build().call();
        match resp {
            Ok(mut resp) => match resp.status().as_u16() {
                200..=299 => return resp.body_mut().read_json().map(Some).with_context(|| format!("reading {url}")),
                404 => return Ok(None),
                code @ (503 | 429) => {
                    busy += 1;
                    let hint = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    let secs = busy_wait(busy, hint);
                    if started.elapsed() + Duration::from_secs(secs) > MB_BUSY_PATIENCE {
                        anyhow::bail!(
                            "MusicBrainz is busy (HTTP {code}) and stayed busy for {} s; try the disc again later",
                            started.elapsed().as_secs()
                        );
                    }
                    waiting(secs);
                    std::thread::sleep(Duration::from_secs(secs));
                }
                code => anyhow::bail!("MusicBrainz returned HTTP {code} for {url}"),
            },
            Err(_) if failures < 4 => {
                failures += 1;
                std::thread::sleep(Duration::from_secs(3));
            }
            Err(e) => return Err(e).with_context(|| format!("requesting {url}")),
        }
    }
}

/// Seconds to wait after the `n`th busy answer: MusicBrainz's Retry-After
/// if it gave one (1-60 s), else 2, 4, 8, 16, then 30.
fn busy_wait(n: u32, retry_after: Option<u64>) -> u64 {
    match retry_after {
        Some(s) => s.clamp(1, 60),
        None => (1u64 << n.min(5)).min(30),
    }
}

#[cfg(test)]
mod tests {
    use super::busy_wait;

    #[test]
    fn busy_backoff() {
        let waits: Vec<u64> = (1..=7).map(|n| busy_wait(n, None)).collect();
        assert_eq!(waits, [2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(busy_wait(1, Some(0)), 1);
        assert_eq!(busy_wait(3, Some(5)), 5);
        assert_eq!(busy_wait(1, Some(600)), 60);
    }
}
