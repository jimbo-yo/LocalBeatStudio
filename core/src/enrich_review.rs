//! Settling the albums `localbeat enrich` left for review: accept the proposed
//! (or another) MusicBrainz release, or reject it so runs leave the album be.
//! Shared by the CLI and the GUI's Review tab.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::Settings;
use crate::enrich::{self, State};
use crate::musicbrainz::{self, Release};

/// The undo record file of an album folder.
pub fn backup_file(config_dir: &Path, rel: &str) -> PathBuf {
    config_dir.join("enrich").join("backups").join(format!("{}.json", rel.replace('/', "__")))
}

/// Merges into the album's undo record: the first saved old values of a
/// file are its original ones, so they're never replaced.
pub fn merge_backup(config_dir: &Path, rel: &str, new: &Value) -> Result<()> {
    let f = backup_file(config_dir, rel);
    let mut record = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in new.as_object().cloned().unwrap_or_default() {
        record.entry(k).or_insert(v);
    }
    if !record.is_empty() {
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(&f, serde_json::to_vec_pretty(&record)?)?;
    }
    Ok(())
}

/// Files an undo record covers (keys starting with `_` are extras).
pub fn written_count(record: &Value) -> usize {
    record.as_object().map(|o| o.keys().filter(|k| !k.starts_with('_')).count()).unwrap_or(0)
}

/// Seconds since 1970, to order runs.
pub fn now_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

/// An album waiting for a decision.
#[derive(Debug, Clone)]
pub struct ReviewItem {
    pub rel: String,
    pub release_id: String,
    pub summary: String,
    pub why: String,
    pub score: f64,
    pub by_sound: bool,
}

/// Albums in review, by folder name.
pub fn list(s: &Settings) -> Vec<ReviewItem> {
    let state = State::load(&s.config_dir);
    state
        .albums
        .iter()
        .filter(|(_, v)| v["status"] == "review")
        .map(|(rel, v)| ReviewItem {
            rel: rel.clone(),
            release_id: v["release"].as_str().unwrap_or("").into(),
            summary: v["summary"].as_str().unwrap_or("").into(),
            why: v["why"].as_str().unwrap_or("").into(),
            score: v["score"].as_f64().unwrap_or(0.0),
            by_sound: v["by"] == "sound",
        })
        .collect()
}

/// Accepts `release_id` (an MBID, or a MusicBrainz release URL) for the
/// album: each track goes to the release track whose title and length fit
/// best; tracks that fit none are left alone. Returns a one-line report.
pub fn accept(s: &Settings, rel: &str, release_id: &str) -> Result<String> {
    let id = release_id.trim().trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    if id.len() != 36 {
        bail!("not a MusicBrainz release ID: {release_id}");
    }
    let local = enrich::read_album(&s.misc_dir.join(rel), &s.misc_dir)?;
    let Some(c) = enrich::fetch_candidate(&id)? else { bail!("MusicBrainz has no release {id}") };
    let m = enrich::score(&local, &c);
    let fits = m.mapping.iter().filter(|i| i.is_some()).count();
    if fits == 0 {
        bail!("none of the {} songs fits a track on {}", local.tracks.len(), c.summary());
    }
    let release = Release {
        id: c.release_id.clone(),
        release_group_id: c.release_group_id.clone(),
        artist_ids: c.artist_ids.clone(),
        ..Default::default()
    };
    let genres = musicbrainz::genres(&release).unwrap_or_default();
    let cover = if local.tracks.iter().all(|t| t.has_cover) {
        None
    } else {
        crate::cover::fetch_front(&release).ok().flatten().map(|p| p.data)
    };
    let done = enrich::apply(&local, &c, &m, &genres, cover.as_deref(), true);
    merge_backup(&s.config_dir, rel, &done.record)?;
    let written = written_count(&done.record);
    let mut state = State::load(&s.config_dir);
    let status = if !done.failed.is_empty() { "partial" } else { "applied" };
    state.albums.insert(
        rel.to_string(),
        json!({"status": status, "by": "review", "release": c.release_id, "summary": c.summary(), "score": m.score,
               "failed": done.failed.iter().map(|(f, e)| json!({"file": f, "error": e})).collect::<Vec<_>>(),
               "when": now_stamp()}),
    );
    state.save()?;
    let mut report = format!("{rel}: {written} of {} files written as {}", local.tracks.len(), c.summary());
    if fits < local.tracks.len() {
        report += &format!(" ({} didn't fit a track)", local.tracks.len() - fits);
    }
    if !done.failed.is_empty() {
        report += &format!("; {} couldn't be written", done.failed.len());
    }
    Ok(report)
}

/// Leaves the album as it is: later runs skip it (`--retry` too).
pub fn reject(s: &Settings, rel: &str) -> Result<()> {
    let mut state = State::load(&s.config_dir);
    let Some(v) = state.albums.get_mut(rel) else { bail!("{rel} isn't in the enrich state") };
    v["status"] = json!("rejected");
    v["when"] = json!(now_stamp());
    state.save()
}
