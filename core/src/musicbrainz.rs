//! MusicBrainz: find the releases for a disc, pick one, and build the tags.
//!
//! Tag names follow MusicBrainz Picard's Vorbis comment mapping (what music
//! players, mb-enrich.py and LocalBeat expect), plus ORIGINALDATE/GENRE the
//! way mb-enrich.py sets them.

use anyhow::{Result, anyhow};
use cdtoc::Toc;
use serde_json::Value;

use crate::http;

#[derive(Debug, Clone, Default)]
pub struct Release {
    pub id: String,
    pub title: String,
    /// Credited album artist ("Gershwin"), as on the cover.
    pub artist: String,
    pub artist_ids: Vec<String>,
    pub artist_sort: String,
    pub date: String,
    pub country: String,
    pub status: String,
    pub barcode: String,
    pub disambiguation: String,
    pub labels: Vec<String>,
    pub catalog_numbers: Vec<String>,
    pub release_group_id: String,
    /// "album", "single", ... plus secondary types ("compilation").
    pub release_types: Vec<String>,
    pub original_date: String,
    /// Audio CDs in the release (a bonus DVD isn't counted, so a CD + DVD
    /// edition is filed like a single CD). Used for DISCTOTAL and disc prefixes.
    pub media_count: usize,
    pub has_front_cover: bool,
    /// The medium (disc of the release) this CD is.
    pub medium: Medium,
    /// Matched by TOC similarity only (disc ID not in MusicBrainz).
    pub fuzzy: bool,
    /// Not our disc ID, but a disc of this release has our layout to within
    /// a few sectors (see [`NEAR_SECTORS`]): as good as exact.
    pub near: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Medium {
    /// Which audio CD of the release this is (1-based, other media skipped).
    pub position: usize,
    pub title: String,
    pub format: String,
    pub tracks: Vec<TrackMeta>,
}

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub position: usize,
    pub title: String,
    pub artist: String,
    pub artist_ids: Vec<String>,
    pub artist_sort: String,
    pub recording_id: String,
    pub track_id: String,
    pub isrcs: Vec<String>,
    pub length_ms: u64,
}

impl Release {
    /// One line for lists: "Greatest Hits — Gershwin (US 1991, CD, RCA Victor 60834-2-RG)".
    pub fn summary(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        let place_date = format!("{} {}", self.country, self.date).trim().to_string();
        if !place_date.is_empty() {
            bits.push(place_date);
        }
        if !self.medium.format.is_empty() {
            bits.push(self.medium.format.clone());
        }
        let label = format!(
            "{} {}",
            self.labels.first().map(String::as_str).unwrap_or(""),
            self.catalog_numbers.first().map(String::as_str).unwrap_or("")
        );
        if !label.trim().is_empty() {
            bits.push(label.trim().to_string());
        }
        if !self.disambiguation.is_empty() {
            bits.push(self.disambiguation.clone());
        }
        if self.status != "Official" && !self.status.is_empty() {
            bits.push(self.status.clone());
        }
        let disc = if self.media_count > 1 { format!(", disc {} of {}", self.medium.position, self.media_count) } else { String::new() };
        format!("{} — {} ({}){disc}", self.title, self.artist, bits.join(", "))
    }
}

/// MusicBrainz's TOC string: "1 N leadout offset1 ... offsetN" (sectors,
/// including the 150-sector pregap; audio session only).
pub fn toc_query(toc: &Toc) -> String {
    let mut s = format!("1 {} {}", toc.audio_len(), toc.audio_leadout());
    for o in toc.audio_sectors() {
        s.push_str(&format!(" {o}"));
    }
    s
}

const INC: &str = "artist-credits+recordings+release-groups+labels+isrcs";

/// Releases containing this disc: exact disc ID matches, or if the disc ID
/// isn't in MusicBrainz, releases with a similar TOC (`fuzzy`).
pub fn lookup(toc: &Toc) -> Result<Vec<Release>> {
    lookup_with(toc, &|_| {})
}

/// [`lookup`], telling `waiting` the seconds of each wait while MusicBrainz
/// is busy (see [`http::musicbrainz_with`]).
pub fn lookup_with(toc: &Toc, waiting: &dyn Fn(u64)) -> Result<Vec<Release>> {
    let id = toc.musicbrainz_id().to_string();
    let path = format!("discid/{id}?toc={}&inc={INC}", toc_query(toc).replace(' ', "+"));
    let mut rs = match http::musicbrainz_with(&path, waiting)? {
        Some(v) => parse_lookup(&v, &id, toc.audio_len()),
        None => Vec::new(),
    };
    if rs.iter().all(|r| r.fuzzy) {
        // A failed side lookup leaves the answer as it was.
        if let Ok(near) = near_lookup(toc, waiting) {
            rs.retain(|r| !near.iter().any(|n| n.id == r.id));
            rs.splice(0..0, near);
        }
    }
    Ok(rs)
}

/// How far (sectors) each track start and the lead-out may be from a
/// MusicBrainz disc's for it to count as the same pressing. Drives report
/// a TOC a sector or so differently, which changes the disc ID (seen: Sinatra
/// "Swing Along With Me", Reprise 1002-2, off by 1 on 9 of 13 positions), and
/// MusicBrainz's own similar-TOC search didn't find it.
pub const NEAR_SECTORS: u32 = 3;

/// Whether a MusicBrainz disc (`discs[]` item: offsets + sectors) has the
/// same layout as ours, give or take [`NEAR_SECTORS`].
pub fn near_disc(d: &Value, starts: &[u32], leadout: u32) -> bool {
    let Some(offs) = d["offsets"].as_array() else { return false };
    let close = |a: u64, b: u32| a.abs_diff(b as u64) <= NEAR_SECTORS as u64;
    offs.len() == starts.len()
        && offs.iter().zip(starts).all(|(o, &s)| o.as_u64().is_some_and(|o| close(o, s)))
        && d["sectors"].as_u64().is_some_and(|l| close(l, leadout))
}

/// Releases with a disc laid out like ours (see [`NEAR_SECTORS`]), found
/// through the album names the CUETools database knows for the layout
/// (from MusicBrainz, Discogs, freedb), then a MusicBrainz search. Counted
/// as exact matches: the track lengths agree to a sector.
fn near_lookup(toc: &Toc, waiting: &dyn Fn(u64)) -> Result<Vec<Release>> {
    let (starts, leadout, n) = (toc.audio_sectors(), toc.audio_leadout(), toc.audio_len());
    let names = crate::ctdb::names(starts, leadout)?;
    let mut ids: Vec<String> = names.iter().filter_map(|h| h.mbid.clone()).collect();
    for h in names.iter().take(4) {
        let q = format!(
            "release:{} AND artist:{} AND tracksmedium:{n}",
            crate::enrich::phrase(&h.album),
            crate::enrich::phrase(&h.artist)
        );
        let Some(v) = http::musicbrainz_with(&format!("release?query={}&limit=25", crate::enrich::url_encode(&q)), waiting)? else { continue };
        for r in v["releases"].as_array().into_iter().flatten() {
            let cd = r["media"].as_array().into_iter().flatten().any(|m| {
                is_audio_cd(m["format"].as_str().unwrap_or("")) && m["track-count"].as_u64() == Some(n as u64)
            });
            if let Some(id) = r["id"].as_str().filter(|_| cd)
                && !ids.iter().any(|x| x == id)
            {
                ids.push(id.to_string());
            }
        }
    }
    let mut out = Vec::new();
    for id in ids.iter().take(12) {
        let Some(v) = http::musicbrainz_with(&format!("release/{id}?inc={INC}+discids"), waiting)? else { continue };
        let near = |d: &Value| near_disc(d, starts, leadout);
        if let Some(r) = parse_release_with(&v, &near, n) {
            out.push(Release { near: !r.fuzzy, ..r });
        }
    }
    Ok(out)
}

/// One release by MBID (for a user's explicit choice).
pub fn release(id: &str, toc: &Toc) -> Result<Release> {
    let v = http::musicbrainz(&format!("release/{id}?inc={INC}+discids"))?
        .ok_or_else(|| anyhow!("MusicBrainz has no release {id}"))?;
    parse_release(&v, &toc.musicbrainz_id().to_string(), toc.audio_len())
        .ok_or_else(|| anyhow!("release {id} has no {}-track medium matching this disc", toc.audio_len()))
}

pub fn parse_lookup(v: &Value, disc_id: &str, track_count: usize) -> Vec<Release> {
    v["releases"]
        .as_array()
        .map(|rs| rs.iter().filter_map(|r| parse_release(r, disc_id, track_count)).collect())
        .unwrap_or_default()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// "Name1, Name2 & Name3" from an artist-credit list, plus IDs and sort name.
fn credit(v: &Value) -> (String, Vec<String>, String) {
    let mut name = String::new();
    let mut sort = String::new();
    let mut ids = Vec::new();
    for c in v.as_array().into_iter().flatten() {
        name.push_str(c["name"].as_str().unwrap_or(""));
        name.push_str(c["joinphrase"].as_str().unwrap_or(""));
        sort.push_str(c["artist"]["sort-name"].as_str().unwrap_or(""));
        sort.push_str(c["joinphrase"].as_str().unwrap_or(""));
        if let Some(id) = c["artist"]["id"].as_str() {
            ids.push(id.to_string());
        }
    }
    (name, ids, sort)
}

fn parse_release(r: &Value, disc_id: &str, track_count: usize) -> Option<Release> {
    parse_release_with(r, &|d: &Value| d["id"].as_str() == Some(disc_id), track_count)
}

/// `is_ours` picks our disc among a medium's `discs`.
fn parse_release_with(r: &Value, is_ours: &dyn Fn(&Value) -> bool, track_count: usize) -> Option<Release> {
    let media = r["media"].as_array()?;
    // The medium with our disc; else (fuzzy match) the first CD medium
    // with the right number of tracks.
    let exact = media.iter().find(|m| m["discs"].as_array().into_iter().flatten().any(is_ours));
    let fuzzy = exact.is_none();
    let m = exact.or_else(|| {
        media.iter().find(|m| m["track-count"].as_u64() == Some(track_count as u64) && m["tracks"].is_array())
    })?;
    let cds: Vec<&Value> = media.iter().filter(|x| is_audio_cd(x["format"].as_str().unwrap_or(""))).collect();
    let cd_position = cds.iter().position(|x| std::ptr::eq(*x, m)).map(|i| i + 1);

    let tracks: Vec<TrackMeta> = m["tracks"]
        .as_array()?
        .iter()
        .map(|t| {
            let (artist, artist_ids, artist_sort) = credit(&t["artist-credit"]);
            let rec = &t["recording"];
            TrackMeta {
                position: t["position"].as_u64().unwrap_or(0) as usize,
                title: s(&t["title"]),
                artist,
                artist_ids,
                artist_sort,
                recording_id: s(&rec["id"]),
                track_id: s(&t["id"]),
                isrcs: rec["isrcs"].as_array().into_iter().flatten().map(s).collect(),
                length_ms: t["length"].as_u64().or(rec["length"].as_u64()).unwrap_or(0),
            }
        })
        .collect();
    if tracks.len() != track_count {
        return None;
    }

    let rg = &r["release-group"];
    let (artist, artist_ids, artist_sort) = credit(&r["artist-credit"]);
    let mut release_types = Vec::new();
    if let Some(p) = rg["primary-type"].as_str() {
        release_types.push(p.to_lowercase());
    }
    release_types.extend(rg["secondary-types"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(str::to_lowercase));
    let labels_info = r["label-info"].as_array().cloned().unwrap_or_default();

    Some(Release {
        id: s(&r["id"]),
        title: s(&r["title"]),
        artist,
        artist_ids,
        artist_sort,
        date: s(&r["date"]),
        country: s(&r["country"]),
        status: s(&r["status"]),
        barcode: s(&r["barcode"]),
        disambiguation: s(&r["disambiguation"]),
        labels: labels_info.iter().filter_map(|l| l["label"]["name"].as_str().map(String::from)).collect(),
        catalog_numbers: labels_info.iter().filter_map(|l| l["catalog-number"].as_str().map(String::from)).collect(),
        release_group_id: s(&rg["id"]),
        release_types,
        original_date: s(&rg["first-release-date"]),
        media_count: cds.len().max(1),
        has_front_cover: r["cover-art-archive"]["front"].as_bool().unwrap_or(false),
        medium: Medium {
            position: cd_position.unwrap_or_else(|| m["position"].as_u64().unwrap_or(1) as usize),
            title: s(&m["title"]),
            format: s(&m["format"]),
            tracks,
        },
        fuzzy,
        near: false,
    })
}

/// Whether a MusicBrainz medium format is an audio CD (CD, Enhanced CD,
/// Copy Control CD, HDCD, SHM-CD, hybrid SACD, DualDisc...), not a DVD,
/// Blu-ray, video CD or data disc. An unknown format counts as a CD.
pub fn is_audio_cd(format: &str) -> bool {
    let f = format.to_ascii_uppercase();
    if f.is_empty() || f == "DUALDISC" {
        return true;
    }
    f.contains("CD") && !f.contains("VCD") && !f.contains("CD-ROM") && !f.contains("DVD")
}

/// Sorts releases best first: exact disc ID match, preferred country (e.g.
/// "US"), official, not a record-club edition, CD, then earliest date.
/// MusicBrainz's order breaks ties.
pub fn rank(releases: &mut [Release], country: Option<&str>) {
    releases.sort_by_key(|r| {
        let club = {
            let d = r.disambiguation.to_lowercase();
            d.contains("club") || d.contains("bmg music service") || d.contains("columbia house")
        };
        (
            r.fuzzy,
            country.is_some_and(|c| !r.country.eq_ignore_ascii_case(c)),
            r.status != "Official",
            club,
            !r.medium.format.contains("CD"),
            r.date.is_empty(),
            r.date.clone(),
        )
    });
}

/// Up to 3 genres, most votes first, Title Case: the release group's, else
/// the album artist's (same as mb-enrich.py).
pub fn genres(release: &Release) -> Result<Vec<String>> {
    if !release.release_group_id.is_empty() && let Some(v) = http::musicbrainz(&format!("release-group/{}?inc=genres", release.release_group_id))? {
        let g = top_genres(&v);
        if !g.is_empty() {
            return Ok(g);
        }
    }
    if let Some(artist) = release.artist_ids.first() && let Some(v) = http::musicbrainz(&format!("artist/{artist}?inc=genres"))? {
        return Ok(top_genres(&v));
    }
    Ok(Vec::new())
}

const MAX_GENRES: usize = 3;

pub fn top_genres(entity: &Value) -> Vec<String> {
    let mut g: Vec<(i64, String)> = entity["genres"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| Some((g["count"].as_i64().unwrap_or(0), g["name"].as_str()?.to_string())))
        .filter(|(c, _)| *c > 0)
        .collect();
    g.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    g.into_iter().take(MAX_GENRES).map(|(_, n)| title_case(&n)).collect()
}

/// "rock and roll" → "Rock and Roll", "proto-punk" → "Proto-Punk".
pub fn title_case(genre: &str) -> String {
    const SMALL: [&str; 11] = ["a", "an", "and", "of", "the", "n", "in", "on", "or", "for", "to"];
    genre
        .split(' ')
        .enumerate()
        .map(|(i, w)| {
            if i > 0 && SMALL.contains(&w) {
                w.to_string()
            } else {
                w.split('-')
                    .map(|p| {
                        let mut c = p.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                    })
                    .collect::<Vec<String>>()
                    .join("-")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Vorbis comments for track `position` (1-based, on this medium).
pub fn track_tags(r: &Release, position: usize, genres: &[String], toc: &Toc) -> Vec<(String, String)> {
    let mut t: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: &str| {
        if !v.is_empty() {
            t.push((k.to_string(), v.to_string()));
        }
    };
    let tr = r.medium.tracks.iter().find(|x| x.position == position);

    add("TITLE", tr.map(|x| x.title.as_str()).unwrap_or(""));
    add("ARTIST", tr.map(|x| x.artist.as_str()).unwrap_or(&r.artist));
    add("ARTISTSORT", tr.map(|x| x.artist_sort.as_str()).unwrap_or(""));
    add("ALBUM", &r.title);
    add("ALBUMARTIST", &r.artist);
    add("ALBUMARTISTSORT", &r.artist_sort);
    add("TRACKNUMBER", &position.to_string());
    add("TRACKTOTAL", &r.medium.tracks.len().to_string());
    add("DISCNUMBER", &r.medium.position.to_string());
    add("DISCTOTAL", &r.media_count.to_string());
    add("DISCSUBTITLE", &r.medium.title);
    add("DATE", &r.date);
    add("ORIGINALDATE", &r.original_date);
    add("ORIGINALYEAR", r.original_date.get(..4).unwrap_or(""));
    for g in genres {
        add("GENRE", g);
    }
    for ty in &r.release_types {
        add("RELEASETYPE", ty);
    }
    add("RELEASESTATUS", &r.status.to_lowercase());
    add("RELEASECOUNTRY", &r.country);
    add("MEDIA", &r.medium.format);
    for l in &r.labels {
        add("LABEL", l);
    }
    for c in &r.catalog_numbers {
        add("CATALOGNUMBER", c);
    }
    add("BARCODE", &r.barcode);
    if let Some(x) = tr {
        for i in &x.isrcs {
            add("ISRC", i);
        }
        add("MUSICBRAINZ_TRACKID", &x.recording_id);
        add("MUSICBRAINZ_RELEASETRACKID", &x.track_id);
        for a in &x.artist_ids {
            add("MUSICBRAINZ_ARTISTID", a);
        }
    }
    add("MUSICBRAINZ_ALBUMID", &r.id);
    add("MUSICBRAINZ_RELEASEGROUPID", &r.release_group_id);
    for a in &r.artist_ids {
        add("MUSICBRAINZ_ALBUMARTISTID", a);
    }
    add("MUSICBRAINZ_DISCID", &toc.musicbrainz_id().to_string());
    add("CDTOC", &toc.to_string());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const GERSHWIN: &str = include_str!("../tests/data/discid-rQRpBjAQ.json");
    const GERSHWIN_TOC: &str = "8+96+11FCE+245F9+2FF76+36E64+3A014+3D2B6+42F7A+4BBF7";

    fn gershwin() -> (Toc, Vec<Release>) {
        let toc = Toc::from_cdtoc(GERSHWIN_TOC).unwrap();
        let v: Value = serde_json::from_str(GERSHWIN).unwrap();
        let rs = parse_lookup(&v, &toc.musicbrainz_id().to_string(), toc.audio_len());
        (toc, rs)
    }

    #[test]
    fn audio_cd_formats() {
        for f in ["CD", "Enhanced CD", "Copy Control CD", "HDCD", "SHM-CD", "Hybrid SACD", "DualDisc", ""] {
            assert!(is_audio_cd(f), "{f}");
        }
        for f in ["DVD-Video", "DVD-Audio", "VCD", "SVCD", "CD-ROM", "Blu-ray", "Digital Media", "Vinyl"] {
            assert!(!is_audio_cd(f), "{f}");
        }
    }

    #[test]
        fn toc_query_format() {
        let toc = Toc::from_cdtoc(GERSHWIN_TOC).unwrap();
        // Same offsets as MusicBrainz lists for disc rQRpBjAQ...
        assert_eq!(toc_query(&toc), "1 8 310263 150 73678 148985 196470 224868 237588 250550 274298");
        assert_eq!(toc.musicbrainz_id().to_string(), "rQRpBjAQwux5k2jpEUJ3yDA6OJA-");
    }

    #[test]
    fn parses_release() {
        let (_, rs) = gershwin();
        assert_eq!(rs.len(), 1);
        let r = &rs[0];
        assert!(!r.fuzzy);
        assert_eq!(r.title, "Greatest Hits");
        assert_eq!(r.artist, "Gershwin");
        assert_eq!(r.country, "US");
        assert_eq!(r.labels, ["RCA Victor"]);
        assert_eq!(r.catalog_numbers, ["60834-2-RG"]);
        assert_eq!(r.release_types, ["album", "compilation"]);
        assert_eq!(r.medium.tracks.len(), 8);
        assert_eq!(r.medium.tracks[0].title, "Rhapsody in Blue");
        assert_eq!(r.summary(), "Greatest Hits — Gershwin (US 1991, CD, RCA Victor 60834-2-RG)");
    }

    #[test]
    fn tags_for_track() {
        let (toc, rs) = gershwin();
        let tags = track_tags(&rs[0], 1, &["Classical".into()], &toc);
        let get = |k: &str| tags.iter().filter(|(n, _)| n == k).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        assert_eq!(get("TITLE"), ["Rhapsody in Blue"]);
        assert_eq!(get("ALBUMARTIST"), ["Gershwin"]);
        assert_eq!(get("TRACKNUMBER"), ["1"]);
        assert_eq!(get("TRACKTOTAL"), ["8"]);
        assert_eq!(get("ORIGINALYEAR"), ["1991"]);
        assert_eq!(get("GENRE"), ["Classical"]);
        assert_eq!(get("ISRC").len(), 3);
        assert_eq!(get("MUSICBRAINZ_ALBUMID"), ["2661c677-3181-4be0-b645-7421554ae5ee"]);
        assert_eq!(get("CDTOC"), [GERSHWIN_TOC]);
        assert!(get("DISCSUBTITLE").is_empty(), "empty values are left out");
    }

    fn rel(id: &str, country: &str, status: &str, dis: &str, date: &str) -> Release {
        Release {
            id: id.into(),
            country: country.into(),
            status: status.into(),
            disambiguation: dis.into(),
            date: date.into(),
            medium: Medium { format: "CD".into(), ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn near_layout() {
        // Sinatra, Swing Along With Me (Reprise 1002-2): MusicBrainz's disc
        // kNcoTsJ52sHNE5R7c8wX1X1HdQI- against the HP drive's TOC.
        let d = serde_json::json!({
            "id": "kNcoTsJ52sHNE5R7c8wX1X1HdQI-",
            "offsets": [183, 8865, 18688, 33010, 46625, 57433, 70588, 81008, 93443, 108795, 118980, 130783],
            "sectors": 149673
        });
        let ours = [182, 8865, 18687, 33010, 46625, 57432, 70587, 81007, 93442, 108795, 118980, 130782];
        assert!(near_disc(&d, &ours, 149672));
        assert!(!near_disc(&d, &ours, 149690), "lead-out too far");
        assert!(!near_disc(&d, &ours[..11], 149672), "track count differs");
        let mut far = ours;
        far[5] += 75;
        assert!(!near_disc(&d, &far, 149672), "a track a second off");
    }

    #[test]
    fn ranking() {
        let mut rs = vec![
            rel("gb", "GB", "Official", "", "1989"),
            rel("club", "US", "Official", "BMG Direct club edition", "1990"),
            rel("us-late", "US", "Official", "", "1999"),
            rel("us", "US", "Official", "", "1989"),
            rel("promo", "US", "Promotion", "", "1988"),
        ];
        rank(&mut rs, Some("US"));
        let order: Vec<&str> = rs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(order, ["us", "us-late", "club", "promo", "gb"]);
        rank(&mut rs, None);
        assert_eq!(rs[0].id, "us");
    }

    #[test]
    fn genre_names() {
        assert_eq!(title_case("rock and roll"), "Rock and Roll");
        assert_eq!(title_case("proto-punk"), "Proto-Punk");
        let v: Value = serde_json::json!({"genres": [
            {"name": "pop", "count": 2}, {"name": "rock", "count": 5},
            {"name": "blues rock", "count": 5}, {"name": "jazz", "count": 1}, {"name": "zero", "count": 0}
        ]});
        assert_eq!(top_genres(&v), ["Blues Rock", "Rock", "Pop"]);
    }

    /// Needs the internet: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn online_lookup_and_genres() {
        let toc = Toc::from_cdtoc(GERSHWIN_TOC).unwrap();
        let rs = lookup(&toc).unwrap();
        assert!(rs.iter().any(|r| r.id == "2661c677-3181-4be0-b645-7421554ae5ee"));
        println!("genres: {:?}", genres(&rs[0]).unwrap());
    }
}
