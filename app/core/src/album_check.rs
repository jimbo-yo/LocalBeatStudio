//! Checks ripped FLAC files against AccurateRip: decodes each track, computes
//! its checksums and looks them up. Used to validate this crate's checksum
//! code on known-good rips, and to audit the library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use cdtoc::Toc;

use crate::accuraterip::{self, Checksums, Verdict};
use crate::flacfile;

pub struct TrackReport {
    pub path: PathBuf,
    pub track: usize,
    pub sums: Checksums,
    pub verdict: Verdict,
}

pub struct DiscInfo {
    /// Where the TOC came from, for display ("CDTOC tag", "MusicBrainz disc ID ...").
    pub toc_source: String,
    pub accuraterip_id: String,
    pub track_count: usize,
    pub in_database: bool,
}

/// Progress callbacks, so a caller can print as it goes (decoding is slow).
pub trait Observer {
    fn disc(&mut self, info: &DiscInfo);
    fn track(&mut self, report: &TrackReport);
    fn skipped(&mut self, path: &Path, why: &str);
}

/// Checks every .flac in `dir` (not recursive). Files are grouped by disc, so
/// a folder holding several discs works.
pub fn check_dir(dir: &Path, obs: &mut dyn Observer) -> Result<()> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .flac files in {}", dir.display());
    }

    // Group by disc identity (TOC source), keeping track order.
    let mut discs: BTreeMap<String, Vec<(usize, PathBuf)>> = BTreeMap::new();
    for path in files {
        let f = flacfile::read_tags(&path)?;
        let Some(track) = f.track_number() else {
            obs.skipped(&path, "no TRACKNUMBER tag");
            continue;
        };
        let Some(key) = disc_key(&f) else {
            obs.skipped(&path, "no CDTOC or MUSICBRAINZ_DISCID tag");
            continue;
        };
        discs.entry(key).or_default().push((track, path));
    }

    for (key, mut tracks) in discs {
        tracks.sort();
        let (toc, toc_source) = toc_from_key(&key)?;
        let entries = accuraterip::fetch(&toc)?;
        let track_count = toc.audio_len();
        obs.disc(&DiscInfo {
            toc_source,
            accuraterip_id: toc.accuraterip_id().to_string(),
            track_count,
            in_database: entries.is_some(),
        });
        let entries = entries.unwrap_or_default();

        for (track, path) in tracks {
            if track == 0 || track > track_count {
                obs.skipped(&path, &format!("track {track} is not on this disc's TOC"));
                continue;
            }
            let (_, frames) = flacfile::read_cd_audio(&path)?;
            let sums = accuraterip::checksums(&frames, track == 1, track == track_count);
            let verdict = entries.verdict(track, sums);
            obs.track(&TrackReport { path, track, sums, verdict });
        }
    }
    Ok(())
}

/// "cdtoc:<value>" or "mbid:<disc id>".
fn disc_key(f: &flacfile::FlacFile) -> Option<String> {
    if let Some(t) = f.tag("CDTOC") {
        return Some(format!("cdtoc:{t}"));
    }
    f.tag("MUSICBRAINZ_DISCID").map(|id| format!("mbid:{id}"))
}

fn toc_from_key(key: &str) -> Result<(Toc, String)> {
    if let Some(v) = key.strip_prefix("cdtoc:") {
        let toc = Toc::from_cdtoc(v).map_err(|e| anyhow!("bad CDTOC tag {v:?}: {e}"))?;
        return Ok((toc, "CDTOC tag".into()));
    }
    let id = key.strip_prefix("mbid:").expect("disc key");
    let toc = toc_from_musicbrainz(id)?;
    Ok((toc, format!("MusicBrainz disc ID {id}")))
}

/// Looks up a MusicBrainz disc ID and rebuilds the TOC from its track offsets.
pub fn toc_from_musicbrainz(disc_id: &str) -> Result<Toc> {
    let url = format!("https://musicbrainz.org/ws/2/discid/{disc_id}?fmt=json&inc=");
    let v: serde_json::Value = crate::http::agent()
        .get(&url)
        .call()
        .with_context(|| format!("looking up disc ID {disc_id} on MusicBrainz"))?
        .body_mut()
        .read_json()
        .context("reading MusicBrainz reply")?;
    let offsets: Vec<u32> = v["offsets"]
        .as_array()
        .ok_or_else(|| anyhow!("MusicBrainz reply has no offsets (disc ID not found?)"))?
        .iter()
        .filter_map(|o| o.as_u64().map(|n| n as u32))
        .collect();
    let leadout = v["sectors"].as_u64().ok_or_else(|| anyhow!("MusicBrainz reply has no sectors"))? as u32;
    Toc::from_parts(offsets, None, leadout).map_err(|e| anyhow!("bad TOC from MusicBrainz: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the internet: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn online_lookups() {
        // TOC from the cdtoc crate's docs.
        let toc = Toc::from_cdtoc("4+96+2D2B+6256+B327+D84A").unwrap();
        let entries = accuraterip::fetch(&toc).expect("AccurateRip request");
        println!("AccurateRip {}: {:?}", toc.accuraterip_id(), entries.map(|e| e.tracks));
        let id = toc.musicbrainz_id().to_string();
        match toc_from_musicbrainz(&id) {
            Ok(t) => assert_eq!(t, toc),
            Err(e) => println!("MusicBrainz {id}: {e:#}"),
        }
    }
}
