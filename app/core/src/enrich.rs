//! Tags and cover art for the Miscellaneous library (old .m4a/.mp3 albums
//! with thin tags and no embedded art), found on MusicBrainz by name.
//!
//! For each album folder: read its tracks (title, length, tags), search
//! MusicBrainz for the album, and score each candidate release by how well
//! its tracks line up with the folder's (titles and lengths; the folder may
//! hold only some of the album). A **sure** match is written into the files
//! (tags + embedded front cover, plus `cover.jpg`), after saving the old tags
//! so it can be undone; anything less waits for review or a fingerprint
//! match. Folders whose tracks come from several albums are left for
//! per-track matching.
//!
//! State and undo backups live in `<config>/rip-cd/enrich/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::Tag;
use serde_json::{Value, json};

use crate::{audio_source, http, musicbrainz};

// ---------------------------------------------------------------- local side

#[derive(Debug, Clone)]
pub struct LocalTrack {
    pub path: PathBuf,
    /// TITLE tag, else the file name without its number.
    pub title: String,
    pub seconds: Option<f64>,
    pub album: String,
    pub artist: String,
    pub album_artist: String,
    pub has_cover: bool,
}

#[derive(Debug, Clone)]
pub struct LocalAlbum {
    pub dir: PathBuf,
    /// Folder relative to the Miscellaneous library.
    pub rel: String,
    pub tracks: Vec<LocalTrack>,
    /// What to search for.
    pub album: String,
    pub artist: String,
    /// Tracks tagged with more than one album name: a folder of loose songs.
    pub mixed: bool,
}

/// "03 - Song.m4a" / "1-03 Song.mp3" / "03. Artist - Song.flac" -> "Song".
fn title_from_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut s = stem.trim_start_matches(|c: char| c.is_ascii_digit() || " -._".contains(c)).to_string();
    if let Some((_, b)) = s.rsplit_once(" - ") {
        s = b.to_string();
    }
    if s.is_empty() { stem } else { s }
}

fn most_common(values: impl Iterator<Item = String>) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for v in values.filter(|v| !v.trim().is_empty()) {
        *counts.entry(v.trim().to_string()).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(v, _)| v).unwrap_or_default()
}

pub fn read_album(dir: &Path, misc_root: &Path) -> Result<LocalAlbum> {
    let rel = dir.strip_prefix(misc_root).unwrap_or(dir).to_string_lossy().replace('\\', "/");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            p.is_file() && ["m4a", "mp3", "aac", "ogg", "flac"].contains(&ext.as_str())
        })
        .collect();
    paths.sort();
    let mut tracks = Vec::new();
    for path in paths {
        let info = audio_source::read_info(&path).unwrap_or_default();
        let tag = |k: &str| info.tags.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let title = Some(tag("TITLE")).filter(|t| !t.is_empty()).unwrap_or_else(|| title_from_name(&path));
        tracks.push(LocalTrack {
            title,
            seconds: info.seconds,
            album: tag("ALBUM"),
            artist: tag("ARTIST"),
            album_artist: tag("ALBUMARTIST"),
            has_cover: info.cover.is_some(),
            path,
        });
    }
    let (folder_artist, folder_album) = crate::phone_sync::split_album(&rel);
    let albums: std::collections::BTreeSet<String> =
        tracks.iter().map(|t| norm(&t.album)).filter(|a| !a.is_empty()).collect();
    let album = Some(most_common(tracks.iter().map(|t| t.album.clone()))).filter(|a| !a.is_empty()).unwrap_or(folder_album);
    let artist = Some(most_common(tracks.iter().map(|t| if t.album_artist.is_empty() { t.artist.clone() } else { t.album_artist.clone() })))
        .filter(|a| !a.is_empty())
        .unwrap_or(folder_artist);
    Ok(LocalAlbum { dir: dir.to_path_buf(), rel, tracks, album, artist, mixed: albums.len() > 1 })
}

// ---------------------------------------------------------------- comparing names

/// Lower case, letters and digits only, single spaces; "&" = "and".
pub fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace('&', " and ").replace(['’', '‘'], "'");
    let s: String = s.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Without "(Remastered 2009)", "[Live]" and the like.
fn strip_brackets(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// 0..1: 1 = same title.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (norm(a), norm(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let (sa, sb) = (norm(&strip_brackets(a)), norm(&strip_brackets(b)));
    if !sa.is_empty() && sa == sb {
        return 0.95;
    }
    let wa: std::collections::BTreeSet<&str> = na.split(' ').collect();
    let wb: std::collections::BTreeSet<&str> = nb.split(' ').collect();
    let inter = wa.intersection(&wb).count() as f64;
    let jaccard = inter / (wa.len() + wb.len()) as f64 * 2.0; // Dice coefficient
    if na.contains(&nb) || nb.contains(&na) { jaccard.max(0.8) } else { jaccard }
}

// ---------------------------------------------------------------- MusicBrainz

#[derive(Debug, Clone)]
pub struct CandTrack {
    pub disc: usize,
    pub position: usize,
    pub disc_tracks: usize,
    pub title: String,
    pub artist: String,
    pub artist_ids: Vec<String>,
    pub seconds: Option<f64>,
    pub recording_id: String,
    pub track_id: String,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub release_id: String,
    pub release_group_id: String,
    pub title: String,
    pub artist: String,
    pub artist_ids: Vec<String>,
    pub date: String,
    pub original_date: String,
    pub country: String,
    pub status: String,
    pub discs: usize,
    pub tracks: Vec<CandTrack>,
}

impl Candidate {
    pub fn summary(&self) -> String {
        let mut bits: Vec<&str> = vec![&self.country, &self.date];
        bits.retain(|b| !b.is_empty());
        format!("{} — {} ({}, {} tracks)", self.title, self.artist, bits.join(" "), self.tracks.len())
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A Lucene phrase for MusicBrainz search.
fn phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', " ").replace('"', " "))
}

fn is_various(artist: &str) -> bool {
    let a = norm(artist);
    a.is_empty() || a == "various artists" || a == "various" || a == "compilations" || a == "soundtrack" || a == "va"
}

/// Release IDs MusicBrainz finds for this album name (best first).
pub fn search(album: &str, artist: &str) -> Result<Vec<String>> {
    let title = strip_brackets(album);
    let title = if title.trim().is_empty() { album.to_string() } else { title };
    let mut q = format!("release:{}", phrase(title.trim()));
    if !is_various(artist) {
        q.push_str(&format!(" AND artist:{}", phrase(artist)));
    }
    let Some(v) = http::musicbrainz(&format!("release?query={}&limit=10", url_encode(&q)))? else { return Ok(Vec::new()) };
    Ok(v["releases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["score"].as_i64().unwrap_or(0) >= 70)
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect())
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn credit(v: &Value) -> (String, Vec<String>) {
    let mut name = String::new();
    let mut ids = Vec::new();
    for c in v.as_array().into_iter().flatten() {
        name.push_str(c["name"].as_str().unwrap_or(""));
        name.push_str(c["joinphrase"].as_str().unwrap_or(""));
        if let Some(id) = c["artist"]["id"].as_str() {
            ids.push(id.to_string());
        }
    }
    (name, ids)
}

pub fn parse_candidate(r: &Value) -> Option<Candidate> {
    let (artist, artist_ids) = credit(&r["artist-credit"]);
    let mut tracks = Vec::new();
    let media: Vec<&Value> = r["media"]
        .as_array()?
        .iter()
        .filter(|m| musicbrainz::is_audio_cd(m["format"].as_str().unwrap_or("")) || m["format"].is_null())
        .collect();
    for (i, m) in media.iter().enumerate() {
        let list = m["tracks"].as_array().cloned().unwrap_or_default();
        for t in &list {
            let (tartist, tids) = credit(&t["artist-credit"]);
            tracks.push(CandTrack {
                disc: i + 1,
                position: t["position"].as_u64().unwrap_or(0) as usize,
                disc_tracks: list.len(),
                title: s(&t["title"]),
                artist: if tartist.is_empty() { artist.clone() } else { tartist },
                artist_ids: if tids.is_empty() { artist_ids.clone() } else { tids },
                seconds: t["length"].as_u64().or(t["recording"]["length"].as_u64()).map(|ms| ms as f64 / 1000.0),
                recording_id: s(&t["recording"]["id"]),
                track_id: s(&t["id"]),
            });
        }
    }
    Some(Candidate {
        release_id: s(&r["id"]),
        release_group_id: s(&r["release-group"]["id"]),
        title: s(&r["title"]),
        artist,
        artist_ids,
        date: s(&r["date"]),
        original_date: s(&r["release-group"]["first-release-date"]),
        country: s(&r["country"]),
        status: s(&r["status"]),
        discs: media.len().max(1),
        tracks,
    })
}

pub fn fetch_candidate(release_id: &str) -> Result<Option<Candidate>> {
    Ok(http::musicbrainz(&format!("release/{release_id}?inc=recordings+artist-credits+release-groups"))?
        .and_then(|v| parse_candidate(&v)))
}

// ---------------------------------------------------------------- scoring

#[derive(Debug, Clone)]
pub struct Match {
    /// Index into the candidate's tracks for each local track.
    pub mapping: Vec<Option<usize>>,
    /// Mean per-track score (0..1) over the local tracks.
    pub score: f64,
    pub album_similarity: f64,
    pub artist_similarity: f64,
    /// Safe to write without asking.
    pub sure: bool,
    pub why_not_sure: String,
}

const SURE_TITLE: f64 = 0.9;
const SURE_SECONDS: f64 = 5.0;

pub fn score(local: &LocalAlbum, c: &Candidate) -> Match {
    let mut used = vec![false; c.tracks.len()];
    let mut mapping = Vec::new();
    let mut total = 0.0;
    let mut problems = Vec::new();
    for t in &local.tracks {
        let mut best: Option<(usize, f64, f64, Option<f64>)> = None;
        for (i, ct) in c.tracks.iter().enumerate() {
            if used[i] {
                continue;
            }
            let title = similarity(&t.title, &ct.title);
            let diff = match (t.seconds, ct.seconds) {
                (Some(a), Some(b)) => Some((a - b).abs()),
                _ => None,
            };
            let len_factor = match diff {
                Some(d) if d <= 3.0 => 1.0,
                Some(d) if d <= 10.0 => 0.85,
                Some(_) => 0.3,
                None => 0.8,
            };
            let s = title * len_factor;
            if best.as_ref().is_none_or(|b| s > b.1) {
                best = Some((i, s, title, diff));
            }
        }
        match best {
            Some((i, s, title, diff)) if s >= 0.5 => {
                used[i] = true;
                mapping.push(Some(i));
                total += s;
                if title < SURE_TITLE {
                    problems.push(format!("\"{}\" vs \"{}\"", t.title, c.tracks[i].title));
                }
                if diff.is_some_and(|d| d > SURE_SECONDS) || diff.is_none() && title < 1.0 {
                    problems.push(format!("length of \"{}\"", t.title));
                }
            }
            _ => {
                mapping.push(None);
                problems.push(format!("no match for \"{}\"", t.title));
            }
        }
    }
    let n = local.tracks.len().max(1) as f64;
    let album_similarity = similarity(&local.album, &c.title).max(similarity(&strip_brackets(&local.album), &c.title));
    let artist_similarity =
        if is_various(&local.artist) && (is_various(&c.artist) || c.artist.is_empty()) { 1.0 } else { similarity(&local.artist, &c.artist) };
    if album_similarity < 0.8 {
        problems.push(format!("album \"{}\" vs \"{}\"", local.album, c.title));
    }
    if artist_similarity < 0.8 && !is_various(&c.artist) {
        problems.push(format!("artist \"{}\" vs \"{}\"", local.artist, c.artist));
    }
    if local.tracks.len() == 1 {
        problems.push("only one track: too little to be sure".into());
    }
    Match { mapping, score: total / n, album_similarity, artist_similarity, sure: problems.is_empty(), why_not_sure: problems.join("; ") }
}

/// The best candidate for an album: highest score; among equals, the one
/// whose track count fits, then the preferred country, official, earliest.
pub fn best<'a>(local: &LocalAlbum, cands: &'a [Candidate], country: Option<&str>) -> Option<(&'a Candidate, Match)> {
    let mut scored: Vec<(&Candidate, Match)> = cands.iter().map(|c| (c, score(local, c))).collect();
    scored.sort_by(|(ca, ma), (cb, mb)| {
        let key = |c: &Candidate, m: &Match| {
            (
                (m.sure as u8),
                (m.score * 1000.0) as i64,
                (c.tracks.len() == local.tracks.len()) as u8,
                country.is_some_and(|k| c.country.eq_ignore_ascii_case(k)) as u8,
                (c.status == "Official") as u8,
            )
        };
        key(cb, mb).cmp(&key(ca, ma)).then_with(|| {
            let d = |c: &Candidate| if c.date.is_empty() { "9999".to_string() } else { c.date.clone() };
            d(ca).cmp(&d(cb))
        })
    });
    scored.into_iter().next()
}

// ---------------------------------------------------------------- writing

/// The tags a match sets on one file (old values are saved before writing).
const TEXT_KEYS: [ItemKey; 11] = [
    ItemKey::TrackTitle,
    ItemKey::TrackArtist,
    ItemKey::AlbumTitle,
    ItemKey::AlbumArtist,
    ItemKey::RecordingDate,
    ItemKey::OriginalReleaseDate,
    ItemKey::Genre,
    ItemKey::MusicBrainzReleaseId,
    ItemKey::MusicBrainzRecordingId,
    ItemKey::MusicBrainzReleaseGroupId,
    ItemKey::MusicBrainzArtistId,
];

fn key_name(k: &ItemKey) -> String {
    format!("{k:?}")
}

/// Writes the match into the files; returns the undo record (old values).
pub fn apply(local: &LocalAlbum, c: &Candidate, m: &Match, genres: &[String], cover: Option<&[u8]>) -> Result<Value> {
    let mut undo = serde_json::Map::new();
    for (t, idx) in local.tracks.iter().zip(&m.mapping) {
        let Some(i) = idx else { continue };
        let ct = &c.tracks[*i];
        let mut tf = lofty::read_from_path(&t.path).with_context(|| format!("reading tags of {}", t.path.display()))?;
        let tt = tf.primary_tag_type();
        if tf.tag_mut(tt).is_none() {
            tf.insert_tag(Tag::new(tt));
        }
        let tag = tf.tag_mut(tt).ok_or_else(|| anyhow!("no tag in {}", t.path.display()))?;

        // Old values first.
        let mut old = serde_json::Map::new();
        for k in TEXT_KEYS {
            old.insert(key_name(&k), tag.get_string(k.clone()).map(|v| json!(v)).unwrap_or(Value::Null));
        }
        old.insert("track".into(), json!(tag.track()));
        old.insert("track_total".into(), json!(tag.track_total()));
        old.insert("disk".into(), json!(tag.disk()));
        old.insert("disk_total".into(), json!(tag.disk_total()));
        // Any picture counts: MP4 (iTunes) pictures have no type, so a front
        // cover added here reads back as "other".
        let had_front = !tag.pictures().is_empty();
        old.insert("pictures_before".into(), json!(tag.pictures().len()));

        let set = |tag: &mut Tag, k: ItemKey, v: &str| {
            if !v.is_empty() {
                tag.insert_text(k, v.to_string());
            }
        };
        set(tag, ItemKey::TrackTitle, &ct.title);
        set(tag, ItemKey::TrackArtist, &ct.artist);
        set(tag, ItemKey::AlbumTitle, &c.title);
        set(tag, ItemKey::AlbumArtist, &c.artist);
        set(tag, ItemKey::RecordingDate, &c.date);
        set(tag, ItemKey::OriginalReleaseDate, &c.original_date);
        if let Some(g) = genres.first() {
            set(tag, ItemKey::Genre, g);
        }
        set(tag, ItemKey::MusicBrainzReleaseId, &c.release_id);
        set(tag, ItemKey::MusicBrainzRecordingId, &ct.recording_id);
        set(tag, ItemKey::MusicBrainzReleaseGroupId, &c.release_group_id);
        if let Some(a) = ct.artist_ids.first() {
            set(tag, ItemKey::MusicBrainzArtistId, a);
        }
        tag.set_track(ct.position as u32);
        tag.set_track_total(ct.disc_tracks as u32);
        tag.set_disk(ct.disc as u32);
        tag.set_disk_total(c.discs as u32);
        if let (false, Some(img)) = (had_front, cover) {
            let mime = if img.starts_with(&[0x89, b'P', b'N', b'G']) { MimeType::Png } else { MimeType::Jpeg };
            tag.push_picture(Picture::unchecked(img.to_vec()).pic_type(PictureType::CoverFront).mime_type(mime).build());
        }
        tag.save_to_path(&t.path, WriteOptions::default()).with_context(|| format!("writing tags to {}", t.path.display()))?;
        undo.insert(t.path.to_string_lossy().into_owned(), Value::Object(old));
    }
    if let Some(img) = cover {
        let name = if img.starts_with(&[0x89, b'P', b'N', b'G']) { "cover.png" } else { "cover.jpg" };
        let p = local.dir.join(name);
        if !local.dir.join("cover.jpg").exists() && !local.dir.join("cover.png").exists() {
            std::fs::write(&p, img).with_context(|| format!("writing {}", p.display()))?;
            undo.insert("_cover_file".into(), json!(p.to_string_lossy()));
        }
    }
    Ok(Value::Object(undo))
}

/// Puts back what [`apply`] changed.
pub fn undo(record: &Value) -> Result<usize> {
    let mut n = 0;
    for (path, old) in record.as_object().into_iter().flatten() {
        if path == "_cover_file" {
            if let Some(p) = old.as_str() {
                let _ = std::fs::remove_file(p);
            }
            continue;
        }
        let mut tf = lofty::read_from_path(path).with_context(|| format!("reading {path}"))?;
        let tt = tf.primary_tag_type();
        let Some(tag) = tf.tag_mut(tt) else { continue };
        for k in TEXT_KEYS {
            match old[key_name(&k)].as_str() {
                Some(v) => {
                    tag.insert_text(k, v.to_string());
                }
                None => tag.remove_key(k),
            }
        }
        match old["track"].as_u64() {
            Some(v) => tag.set_track(v as u32),
            None => tag.remove_track(),
        }
        match old["track_total"].as_u64() {
            Some(v) => tag.set_track_total(v as u32),
            None => tag.remove_track_total(),
        }
        match old["disk"].as_u64() {
            Some(v) => tag.set_disk(v as u32),
            None => tag.remove_disk(),
        }
        match old["disk_total"].as_u64() {
            Some(v) => tag.set_disk_total(v as u32),
            None => tag.remove_disk_total(),
        }
        // Remove the pictures added (they're appended after the old ones).
        let before = old["pictures_before"].as_u64().unwrap_or(u64::MAX) as usize;
        while tag.pictures().len() > before {
            tag.remove_picture(tag.pictures().len() - 1);
        }
        tag.save_to_path(path, WriteOptions::default()).with_context(|| format!("writing {path}"))?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------- state

/// What happened to each album folder, kept between runs.
pub struct State {
    file: PathBuf,
    pub albums: serde_json::Map<String, Value>,
}

impl State {
    pub fn load(config_dir: &Path) -> Self {
        let file = config_dir.join("enrich").join("state.json");
        let albums = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["albums"].as_object().cloned())
            .unwrap_or_default();
        State { file, albums }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(self.file.parent().unwrap())?;
        let tmp = self.file.with_extension("new");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&json!({ "version": 1, "albums": self.albums }))?)?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    pub fn status(&self, rel: &str) -> Option<&str> {
        self.albums.get(rel).and_then(|a| a["status"].as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(album: &str, artist: &str, tracks: &[(&str, f64)]) -> LocalAlbum {
        LocalAlbum {
            dir: PathBuf::new(),
            rel: format!("{artist}/{album}"),
            album: album.into(),
            artist: artist.into(),
            mixed: false,
            tracks: tracks
                .iter()
                .map(|(t, s)| LocalTrack {
                    path: PathBuf::new(),
                    title: t.to_string(),
                    seconds: Some(*s),
                    album: album.into(),
                    artist: artist.into(),
                    album_artist: String::new(),
                    has_cover: false,
                })
                .collect(),
        }
    }

    fn cand(title: &str, artist: &str, tracks: &[(&str, f64)]) -> Candidate {
        Candidate {
            release_id: "r".into(),
            release_group_id: "g".into(),
            title: title.into(),
            artist: artist.into(),
            artist_ids: vec![],
            date: "1992".into(),
            original_date: "1992".into(),
            country: "US".into(),
            status: "Official".into(),
            discs: 1,
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (t, s))| CandTrack {
                    disc: 1,
                    position: i + 1,
                    disc_tracks: tracks.len(),
                    title: t.to_string(),
                    artist: artist.into(),
                    artist_ids: vec![],
                    seconds: Some(*s),
                    recording_id: format!("rec{i}"),
                    track_id: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn names() {
        assert_eq!(title_from_name(Path::new("03 - Dancing Queen.m4a")), "Dancing Queen");
        assert_eq!(title_from_name(Path::new("1-03 Waterloo.mp3")), "Waterloo");
        assert_eq!(title_from_name(Path::new("03. ABBA - S.O.S.flac")), "S.O.S");
        assert_eq!(similarity("Gimme! Gimme! Gimme!", "Gimme Gimme Gimme"), 1.0);
        assert_eq!(similarity("Waterloo (Remastered 2001)", "Waterloo"), 0.95);
        assert!(similarity("Dancing Queen", "Money, Money, Money") < 0.3);
        assert_eq!(similarity("Simon & Garfunkel", "Simon and Garfunkel"), 1.0);
    }

    #[test]
    fn partial_album_is_sure_wrong_one_is_not() {
        let rel = [("Dancing Queen", 231.0), ("Knowing Me, Knowing You", 242.0), ("Take a Chance on Me", 245.0), ("Mamma Mia", 213.0)];
        let gold = cand("Gold: Greatest Hits", "ABBA", &rel);
        // Two of four tracks, iTunes-style titles, lengths a second off.
        let mine = local("Gold: Greatest Hits", "ABBA", &[("Mamma Mia", 214.0), ("Dancing Queen", 231.5)]);
        let m = score(&mine, &gold);
        assert!(m.sure, "{}", m.why_not_sure);
        assert_eq!(m.mapping, vec![Some(3), Some(0)]);

        let other = cand("ABBA Live", "ABBA", &[("Dancing Queen", 260.0), ("Mamma Mia", 180.0)]);
        let m = score(&mine, &other);
        assert!(!m.sure, "different album name and lengths");

        let one = local("Gold: Greatest Hits", "ABBA", &[("Mamma Mia", 213.0)]);
        assert!(!score(&one, &gold).sure, "one track is too little");
    }

    #[test]
    fn picks_the_fitting_release() {
        let mine = local("Gold", "ABBA", &[("Waterloo", 165.0), ("SOS", 200.0)]);
        let a = cand("Gold", "ABBA", &[("Waterloo", 165.0), ("SOS", 200.0), ("Fernando", 250.0)]);
        let b = cand("Gold", "ABBA", &[("Waterloo", 165.0), ("SOS", 200.0)]);
        let cands = [a, b];
        let (best_c, m) = best(&mine, &cands, Some("US")).unwrap();
        assert!(m.sure);
        assert_eq!(best_c.tracks.len(), 2, "track count that fits wins a tie");
    }
}
