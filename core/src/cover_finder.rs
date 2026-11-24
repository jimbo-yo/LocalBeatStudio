//! Missing cover art discovery, preview download, AI identification fallback,
//! and native format-specific embedding (FLAC/Ogg/Opus/MP3/M4A) while
//! preserving file modification time (`mtime`) and custom tags.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lofty::TextEncoding;
use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::FileType;
use lofty::id3::v2::{AttachedPictureFrame, Frame, Id3v2Tag};
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::covers;
use crate::http;
use crate::library::{Album, Library, Origin, Song, norm};

const MAX_COVER_DIM: u32 = 1200;
const MAX_COVER_BYTES: usize = 15 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverCandidate {
    /// "iTunes" or "Cover Art Archive"
    pub source: String,
    pub artist: String,
    pub title: String,
    pub year: Option<String>,
    pub thumb_url: String,
    pub full_url: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct CoverImage {
    pub data: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiCoverHint {
    pub id: String,
    pub search_artist: String,
    pub search_album: String,
}

/// True if an album already has cover art (either embedded in a song, reported
/// by the server via `coverArt`, or a cover image file in its folder).
pub fn album_has_cover(album: &Album, songs: &[Song]) -> bool {
    if album.songs.iter().any(|&i| songs[i].picture) {
        return true;
    }
    if album.origin != Origin::Server && covers::find(album, songs).is_some() {
        return true;
    }
    false
}

/// All albums in `lib` that currently have no cover art, sorted by artist then title.
pub fn missing_cover_albums(lib: &Library) -> Vec<&Album> {
    let mut out: Vec<&Album> = lib
        .albums
        .iter()
        .filter(|a| !album_has_cover(a, &lib.songs))
        .collect();
    out.sort_by(|a, b| norm(&a.artist).cmp(&norm(&b.artist)).then_with(|| norm(&a.title).cmp(&norm(&b.title))));
    out
}

/// Stable short ID for an album key (matches `covers::thumbnail` prefix).
pub fn key_short_id(key: &str) -> String {
    format!("{:08x}", crc32fast::hash(key.as_bytes()))
}

/// Stable short ID for an album (matches `ai_enrich::album_short_id`).
pub fn album_short_id(album: &Album) -> String {
    key_short_id(&album.key)
}

// ---------------------------------------------------------------- searching

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn token_overlap(a: &str, b: &str) -> f32 {
    let na = norm(a);
    let nb = norm(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    if na.contains(&nb) || nb.contains(&na) {
        return 0.85;
    }
    let wa: Vec<&str> = na.split_whitespace().collect();
    let wb: Vec<&str> = nb.split_whitespace().collect();
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let hits = wa.iter().filter(|w| wb.contains(w)).count();
    (2.0 * hits as f32) / (wa.len() + wb.len()) as f32
}

fn score_match(want_artist: &str, want_album: &str, got_artist: &str, got_album: &str) -> f32 {
    let a_score = token_overlap(want_artist, got_artist);
    let t_score = token_overlap(want_album, got_album);
    if want_artist.trim().is_empty() || want_artist.eq_ignore_ascii_case("various artists") {
        t_score
    } else {
        0.4 * a_score + 0.6 * t_score
    }
}

/// Searches Apple iTunes Search API + MusicBrainz / Cover Art Archive for
/// candidate album covers, sorted best match first.
pub fn search_covers(artist: &str, album: &str) -> Vec<CoverCandidate> {
    let mut out = Vec::new();
    if let Ok(mut itunes) = search_itunes(artist, album) {
        out.append(&mut itunes);
    }
    // Also check MusicBrainz / Cover Art Archive if iTunes didn't find a strong match
    // (or returned fewer than 2 options).
    let strong = out.iter().filter(|c| c.score >= 0.72).count();
    if strong < 2 {
        if let Ok(mut caa) = search_cover_art_archive(artist, album) {
            out.append(&mut caa);
        }
    }
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(8);
    out
}

fn search_itunes(artist: &str, album: &str) -> Result<Vec<CoverCandidate>> {
    let clean_artist = if artist.eq_ignore_ascii_case("various artists") { "" } else { artist.trim() };
    let term = format!("{clean_artist} {}", album.trim()).trim().to_string();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=album&limit=8",
        url_encode(&term)
    );
    let mut resp = http::agent()
        .get(&url)
        .call()
        .with_context(|| format!("searching iTunes for {term}"))?;
    let val: Value = resp.body_mut().read_json()?;
    let Some(results) = val["results"].as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in results {
        let got_artist = item["artistName"].as_str().unwrap_or("").trim().to_string();
        let got_title = item["collectionName"].as_str().unwrap_or("").trim().to_string();
        let art100 = item["artworkUrl100"].as_str().unwrap_or("").trim();
        if art100.is_empty() {
            continue;
        }
        let thumb_url = art100.replace("100x100bb", "300x300bb");
        let full_url = art100.replace("100x100bb", "1000x1000bb");
        let year = item["releaseDate"]
            .as_str()
            .and_then(|d| d.get(..4))
            .map(str::to_string);
        let score = score_match(artist, album, &got_artist, &got_title);
        out.push(CoverCandidate {
            source: "iTunes".into(),
            artist: got_artist,
            title: got_title,
            year,
            thumb_url,
            full_url,
            score,
        });
    }
    Ok(out)
}

fn search_cover_art_archive(artist: &str, album: &str) -> Result<Vec<CoverCandidate>> {
    let q = if artist.trim().is_empty() || artist.eq_ignore_ascii_case("various artists") {
        format!("releasegroup:\"{}\"", album.replace('"', ""))
    } else {
        format!(
            "releasegroup:\"{}\" AND artist:\"{}\"",
            album.replace('"', ""),
            artist.replace('"', "")
        )
    };
    let url = format!(
        "https://musicbrainz.org/ws/2/release-group/?query={}&limit=4&fmt=json",
        url_encode(&q)
    );
    let mut resp = http::agent()
        .get(&url)
        .call()
        .with_context(|| format!("searching MusicBrainz for {artist} - {album}"))?;
    let val: Value = resp.body_mut().read_json()?;
    let Some(rgs) = val["release-groups"].as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for rg in rgs {
        let Some(id) = rg["id"].as_str() else { continue };
        let got_title = rg["title"].as_str().unwrap_or("").to_string();
        let got_artist = rg["artist-credit"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|c| c["name"].as_str())
            .unwrap_or("")
            .to_string();
        let year = rg["first-release-date"].as_str().and_then(|d| d.get(..4)).map(str::to_string);
        let thumb_url = format!("https://coverartarchive.org/release-group/{id}/front-250");
        let full_url = format!("https://coverartarchive.org/release-group/{id}/front-1200");
        // Verify Cover Art Archive actually has front art for this release-group.
        match http::agent().head(&thumb_url).call() {
            Ok(_) => {
                let score = score_match(artist, album, &got_artist, &got_title);
                out.push(CoverCandidate {
                    source: "Cover Art Archive".into(),
                    artist: got_artist,
                    title: got_title,
                    year,
                    thumb_url,
                    full_url,
                    score,
                });
            }
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// Downloads a cover image URL, validates it, and normalizes oversized images
/// to <= 1200x1200 JPEG so embedded art stays crisp and compact.
pub fn download_cover(url: &str) -> Result<CoverImage> {
    let mut resp = http::agent()
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let raw = resp
        .body_mut()
        .with_config()
        .limit(MAX_COVER_BYTES as u64)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))?;
    normalize_cover_bytes(&raw)
}

/// Loads and validates a local image file chosen by the user.
pub fn load_local_cover(path: &Path) -> Result<CoverImage> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.len() > MAX_COVER_BYTES {
        bail!("cover file exceeds {} MiB limit", MAX_COVER_BYTES / (1024 * 1024));
    }
    normalize_cover_bytes(&raw)
}

pub fn normalize_cover_bytes(raw: &[u8]) -> Result<CoverImage> {
    let img = image::load_from_memory(raw).context("decoding cover image")?;
    let (w, h) = (img.width(), img.height());
    // If already <= 1200px and JPEG or PNG and <= 1.5 MiB, keep original bytes untouched.
    if w <= MAX_COVER_DIM && h <= MAX_COVER_DIM && raw.len() <= 1_500_000 {
        if raw.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Ok(CoverImage { data: raw.to_vec(), mime: "image/jpeg".into(), width: w, height: h });
        }
        if raw.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Ok(CoverImage { data: raw.to_vec(), mime: "image/png".into(), width: w, height: h });
        }
    }
    let resized = if w > MAX_COVER_DIM || h > MAX_COVER_DIM {
        img.resize(MAX_COVER_DIM, MAX_COVER_DIM, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let rgb = resized.to_rgb8();
    let (rw, rh) = (rgb.width(), rgb.height());
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&rgb)
        .context("encoding cover JPEG")?;
    Ok(CoverImage { data: out, mime: "image/jpeg".into(), width: rw, height: rh })
}

/// Standard base64 encoder (no external dependency needed).
pub fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------- AI identify helper

/// Exports unmatched/garbled albums as a concise prompt for any AI to identify
/// the canonical artist + album title so Studio can fetch the real cover art.
pub fn export_ai_identify_prompt(lib: &Library, albums: &[&Album]) -> String {
    let rows: Vec<Value> = albums
        .iter()
        .map(|a| {
            let tracks: Vec<&str> = a.songs.iter().take(12).map(|&i| lib.songs[i].title.as_str()).collect();
            json!({
                "id": album_short_id(a),
                "artist": a.artist,
                "album": a.title,
                "year": a.year,
                "tracks": tracks,
            })
        })
        .collect();
    let payload = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into());
    format!(
        "I am trying to find album cover art for the following albums whose folder/tag names didn't match automatically.\n\
         For each entry, identify the exact canonical Artist name and Album title (fixing typos, removing disc suffixes or extra junk) \
         so my music manager can look up the official cover on iTunes / Cover Art Archive.\n\n\
         Return ONLY a JSON array with this schema:\n\
         ```json\n\
         [\n\
           {{\"id\": \"<id>\", \"search_artist\": \"Canonical Artist\", \"search_album\": \"Canonical Album Title\"}}\n\
         ]\n\
         ```\n\n\
         Albums:\n\
         ```json\n\
         {payload}\n\
         ```\n"
    )
}

/// Parses the AI's JSON response containing cleaned-up search queries.
pub fn import_ai_identify_response(raw: &str) -> Result<Vec<AiCoverHint>> {
    let trimmed = raw.trim();
    let json_text = if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            &trimmed[start..=end]
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    let items: Vec<AiCoverHint> = serde_json::from_str(json_text).context("parsing AI cover-identify JSON array")?;
    let mut by_id: BTreeMap<String, AiCoverHint> = BTreeMap::new();
    for mut h in items {
        h.id = h.id.trim().to_string();
        h.search_artist = h.search_artist.trim().to_string();
        h.search_album = h.search_album.trim().to_string();
        if !h.id.is_empty() && !h.search_album.is_empty() {
            by_id.insert(h.id.clone(), h);
        }
    }
    Ok(by_id.into_values().collect())
}

// ---------------------------------------------------------------- writing covers

fn parse_relaxed() -> ParseOptions {
    ParseOptions::new().parsing_mode(ParsingMode::Relaxed)
}

fn mime_type_from_str(mime: &str) -> MimeType {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => MimeType::Png,
        "image/gif" => MimeType::Gif,
        "image/jpeg" | "image/jpg" => MimeType::Jpeg,
        other => MimeType::Unknown(other.to_string()),
    }
}

fn make_front_picture(bytes: &[u8], mime: &str) -> Picture {
    Picture::unchecked(bytes.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime_type_from_str(mime))
        .build()
}

fn fix_id3_languages_in_place(id3: &mut Id3v2Tag) {
    let valid = |l: &[u8; 3]| l.iter().all(|b| b.is_ascii_alphabetic());
    id3.retain_mut(|frame| {
        match frame {
            Frame::Comment(c) if !valid(&c.language) => c.language = lofty::tag::items::UNKNOWN_LANGUAGE,
            Frame::UnsynchronizedText(u) if !valid(&u.language) => u.language = lofty::tag::items::UNKNOWN_LANGUAGE,
            _ => {}
        }
        true
    });
}

/// Embeds front cover art directly into `path` (FLAC/Vorbis/Opus/MP3/M4A),
/// preserving `mtime` and all existing custom tags.
pub fn embed_cover_file(path: &Path, data: &[u8], mime: &str) -> Result<()> {
    let orig_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let probe = lofty::probe::Probe::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .guess_file_type()
        .with_context(|| format!("detecting format of {}", path.display()))?;
    let ft = probe.file_type().ok_or_else(|| anyhow!("unsupported audio format: {}", path.display()))?;
    let pic = make_front_picture(data, mime);

    match ft {
        FileType::Flac => {
            let mut flac = {
                let mut f = std::fs::File::open(path)?;
                lofty::flac::FlacFile::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading FLAC {}", path.display()))?
            };
            let _ = flac.remove_picture_type(PictureType::CoverFront);
            flac.insert_picture(pic, None)?;
            flac.save_to_path(path, WriteOptions::default())?;
        }
        FileType::Vorbis => {
            let mut v = {
                let mut f = std::fs::File::open(path)?;
                VorbisFile::read_from(&mut f, parse_relaxed())?
            };
            let _ = v.vorbis_comments_mut().remove_picture_type(PictureType::CoverFront);
            v.vorbis_comments_mut().insert_picture(pic, None)?;
            v.save_to_path(path, WriteOptions::default())?;
        }
        FileType::Opus => {
            let mut o = {
                let mut f = std::fs::File::open(path)?;
                OpusFile::read_from(&mut f, parse_relaxed())?
            };
            let _ = o.vorbis_comments_mut().remove_picture_type(PictureType::CoverFront);
            o.vorbis_comments_mut().insert_picture(pic, None)?;
            o.save_to_path(path, WriteOptions::default())?;
        }
        FileType::Mpeg => {
            let mut mpeg = {
                let mut f = std::fs::File::open(path)?;
                MpegFile::read_from(&mut f, parse_relaxed())?
            };
            if mpeg.id3v2().is_none() {
                mpeg.set_id3v2(Id3v2Tag::new());
            }
            let id3 = mpeg.id3v2_mut().unwrap();
            fix_id3_languages_in_place(id3);
            id3.retain(|f| !matches!(f, Frame::Picture(p) if p.picture.pic_type() == PictureType::CoverFront));
            id3.insert(Frame::Picture(AttachedPictureFrame::new(TextEncoding::UTF8, pic)));
            crate::ai_enrich::save_id3_mp3(id3, path)?;
        }
        FileType::Mp4 => {
            let mut mp4 = {
                let mut f = std::fs::File::open(path)?;
                Mp4File::read_from(&mut f, parse_relaxed())?
            };
            if mp4.ilst().is_none() {
                mp4.set_ilst(Ilst::new());
            }
            let ilst = mp4.ilst_mut().unwrap();
            let covr = AtomIdent::Fourcc(*b"covr");
            let _ = ilst.remove(&covr);
            ilst.replace_atom(Atom::new(covr, AtomData::Picture(pic)));
            mp4.ilst().unwrap().save_to_path(path, WriteOptions::default())?;
        }
        other => bail!("embedding cover art in {other:?} is not supported ({})", path.display()),
    }

    if let Some(mtime) = orig_mtime {
        let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(mtime));
    }
    Ok(())
}

/// Writes `cover.jpg` or `cover.png` atomically in `dir`.
pub fn write_folder_cover(dir: &Path, data: &[u8], mime: &str) -> Result<PathBuf> {
    let name = if mime.eq_ignore_ascii_case("image/png") { "cover.png" } else { "cover.jpg" };
    let dest = dir.join(name);
    let tmp = dir.join(format!(".{name}.part"));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).with_context(|| format!("renaming {}", dest.display()))?;
    let alt = if name == "cover.png" { "cover.jpg" } else { "cover.png" };
    let _ = std::fs::remove_file(dir.join(alt));
    Ok(dest)
}

#[allow(dead_code)]
fn _keep_cow_import() -> Cow<'static, str> {
    Cow::Borrowed("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encoding() {
        assert_eq!(encode_base64(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]), "/9j/4AAQ");
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn scoring_prefers_exact_match() {
        let exact = score_match("Guns N' Roses", "Appetite for Destruction", "Guns N' Roses", "Appetite for Destruction");
        let loose = score_match("Guns N' Roses", "Appetite for Destruction", "Guns N' Roses", "Greatest Hits");
        assert!(exact > 0.99);
        assert!(exact > loose);
    }

    #[test]
    fn ai_identify_round_trip() {
        let raw = "Here you go:\n```json\n[{\"id\":\"abcd1234\",\"search_artist\":\"Billy Idol\",\"search_album\":\"Greatest Hits\"}]\n```";
        let hints = import_ai_identify_response(raw).unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].search_artist, "Billy Idol");
        assert_eq!(hints[0].search_album, "Greatest Hits");
    }
}
