//! Identifying songs by sound: Chromaprint fingerprints (rusty-chromaprint,
//! the algorithm AcoustID uses) looked up on AcoustID, which returns the
//! MusicBrainz recordings and the releases they're on. Needs an application
//! key (free: https://acoustid.org/new-application).

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde_json::Value;

use crate::{audio_source, flacfile, http};

/// AcoustID fingerprints the first two minutes.
const SECONDS: f64 = 120.0;

/// Base64 (URL-safe, no padding), as AcoustID wants fingerprints.
fn base64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..chunk.len() + 1 {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// (compressed fingerprint, length in seconds) of a file.
pub fn fingerprint(path: &Path) -> Result<(String, u32)> {
    let config = Configuration::preset_test2();
    let mut fp = Fingerprinter::new(&config);
    let mut started = false;
    let mut frames = 0f64;
    let mut rate = 0u32;
    let mut pcm16: Vec<i16> = Vec::new();
    let on_block = |pcm: &[f32], r: u32, channels: u32| -> Result<()> {
        if !started {
            fp.start(r, channels).map_err(|e| anyhow!("fingerprinter: {e:?}"))?;
            started = true;
            rate = r;
        }
        if frames / r as f64 >= SECONDS {
            bail!("enough");
        }
        pcm16.clear();
        pcm16.extend(pcm.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        fp.consume(&pcm16);
        frames += (pcm.len() / channels.max(1) as usize) as f64;
        Ok(())
    };
    let is_flac = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("flac"));
    let r = if is_flac { flacfile::decode_float(path, on_block) } else { audio_source::decode_float(path, on_block) };
    match r {
        Err(e) if e.to_string() != "enough" && !e.chain().any(|c| c.to_string() == "enough") => return Err(e),
        _ => {}
    }
    if !started {
        bail!("no audio in {}", path.display());
    }
    fp.finish();
    // The whole song's length (AcoustID checks it), not just what we decoded.
    let known = if is_flac { flacfile::seconds(path) } else { audio_source::read_info(path).ok().and_then(|i| i.seconds) };
    let secs = known.unwrap_or(frames / rate.max(1) as f64);
    let compressed = FingerprintCompressor::from(&config).compress(fp.fingerprint());
    Ok((base64url(&compressed), secs.round() as u32))
}

/// A recording AcoustID matched, with the releases it's on.
#[derive(Debug, Clone)]
pub struct Hit {
    pub score: f64,
    pub recording_id: String,
    pub title: String,
    pub releases: Vec<HitRelease>,
}

#[derive(Debug, Clone)]
pub struct HitRelease {
    pub id: String,
    pub title: String,
    pub group_title: String,
    /// "Album", "Single", ... and secondary types ("Compilation", "Live").
    pub group_types: Vec<String>,
    pub country: String,
    pub year: u32,
}

impl HitRelease {
    /// Studio album (not a compilation, live album, soundtrack...).
    pub fn is_plain_album(&self) -> bool {
        self.group_types.first().map(String::as_str) == Some("Album") && self.group_types.len() == 1
    }
}

/// Looks up a fingerprint (best matches first). At most 3 requests a second.
pub fn lookup(key: &str, fingerprint: &str, seconds: u32) -> Result<Vec<Hit>> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if let Some(t) = *last {
            std::thread::sleep(Duration::from_millis(350).saturating_sub(t.elapsed()));
        }
        *last = Some(Instant::now());
    }
    let body = format!(
        "client={key}&format=json&duration={seconds}&meta=recordings+releasegroups+releases&fingerprint={fingerprint}"
    );
    // AcoustID explains errors (bad key, bad fingerprint) in a JSON body
    // with a 4xx status: read it rather than stopping at the status.
    let mut resp = http::agent()
        .post("https://api.acoustid.org/v2/lookup")
        .config()
        .http_status_as_error(false)
        .build()
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .context("asking AcoustID")?;
    let v: Value = resp.body_mut().read_json().context("reading AcoustID's answer")?;
    if v["status"] != "ok" {
        bail!("AcoustID: {}", v["error"]["message"].as_str().unwrap_or("error"));
    }
    let mut hits = Vec::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let score = r["score"].as_f64().unwrap_or(0.0);
        for rec in r["recordings"].as_array().into_iter().flatten() {
            let mut releases = Vec::new();
            for g in rec["releasegroups"].as_array().into_iter().flatten() {
                let mut types: Vec<String> = g["type"].as_str().map(|t| vec![t.to_string()]).unwrap_or_default();
                types.extend(g["secondarytypes"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)));
                for rel in g["releases"].as_array().into_iter().flatten() {
                    releases.push(HitRelease {
                        id: rel["id"].as_str().unwrap_or("").into(),
                        title: rel["title"].as_str().or(g["title"].as_str()).unwrap_or("").into(),
                        group_title: g["title"].as_str().unwrap_or("").into(),
                        group_types: types.clone(),
                        country: rel["country"].as_str().unwrap_or("").into(),
                        year: rel["date"]["year"].as_u64().unwrap_or(9999) as u32,
                    });
                }
            }
            if let Some(id) = rec["id"].as_str() {
                hits.push(Hit { score, recording_id: id.into(), title: rec["title"].as_str().unwrap_or("").into(), releases });
            }
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64url(b"Man"), "TWFu");
        assert_eq!(base64url(b"Ma"), "TWE");
        assert_eq!(base64url(b"M"), "TQ");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}
