//! Album covers for the library: where an album's picture is (an image file
//! in its folder, else a picture embedded in a song), small JPEG thumbnails
//! cached in `<cache>/covers/`, and an accent colour taken from the cover.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;

use crate::library::{Album, Song};

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    /// The picture inside this song.
    Embedded(PathBuf),
}

const IMAGE_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// Picks the album's cover: the best-named image in its folders ("cover",
/// "folder", "front"; iTunes' small "AlbumArt" last; larger wins on a tie),
/// else the first song with an embedded picture.
pub fn find(album: &Album, songs: &[Song]) -> Option<Source> {
    let mut best: Option<(u8, u64, PathBuf)> = None;
    for dir in &album.dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
            if !IMAGE_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let name = p.file_stem().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if name.starts_with('.') || name.contains("back") || name.contains("inlay") || name.contains("tray") {
                continue;
            }
            let rank = if name.starts_with("cover") || name.starts_with("folder") || name.starts_with("front") {
                3
            } else if name.starts_with("albumart") {
                1
            } else {
                2
            };
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if best.as_ref().is_none_or(|(r, s, _)| (rank, size) > (*r, *s)) {
                best = Some((rank, size, p));
            }
        }
    }
    if let Some((_, _, p)) = best {
        return Some(Source::File(p));
    }
    album.songs.iter().map(|&i| &songs[i]).find(|s| s.picture).map(|s| Source::Embedded(s.path.clone()))
}

/// The image bytes of a cover.
pub fn bytes(src: &Source) -> Result<Vec<u8>> {
    match src {
        Source::File(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display())),
        Source::Embedded(p) => {
            let tf = lofty::read_from_path(p).with_context(|| format!("reading {}", p.display()))?;
            let pics: Vec<_> = tf.tags().iter().flat_map(|t| t.pictures().iter()).collect();
            let pic = pics
                .iter()
                .find(|p| p.pic_type() == PictureType::CoverFront)
                .or(pics.first())
                .context("no picture in the file")?;
            Ok(pic.data().to_vec())
        }
    }
}

fn source_path(src: &Source) -> &Path {
    match src {
        Source::File(p) | Source::Embedded(p) => p,
    }
}

/// Path of the album's `px`-pixel thumbnail, made (or remade, when the
/// cover is newer) if needed. None if the album has no cover.
pub fn thumbnail(album: &Album, songs: &[Song], cache_dir: &Path, px: u32) -> Result<Option<PathBuf>> {
    let Some(src) = find(album, songs) else { return Ok(None) };
    let dir = cache_dir.join("covers");
    let out = dir.join(format!("{:08x}-{px}.jpg", crc32fast::hash(album.key.as_bytes())));
    let newer = |a: &Path, b: &Path| -> bool {
        let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (m(a), m(b)) {
            (Some(x), Some(y)) => x > y,
            _ => true,
        }
    };
    if out.exists() && !newer(source_path(&src), &out) {
        return Ok(Some(out));
    }
    let data = bytes(&src)?;
    let img = image::load_from_memory(&data).context("decoding the cover")?;
    let thumb = img.resize_to_fill(px, px, image::imageops::FilterType::Lanczos3).to_rgb8();
    std::fs::create_dir_all(&dir)?;
    let tmp = out.with_extension("jpg.part");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut f, 88).encode_image(&thumb)?;
    drop(f);
    std::fs::rename(&tmp, &out)?;
    Ok(Some(out))
}

/// A colour that stands for the cover: the average of its more colourful
/// pixels, kept dark enough to sit behind white text. RGBA input.
pub fn accent_color(rgba: &[u8]) -> [u8; 3] {
    let (mut sum, mut weight) = ([0f64; 3], 0f64);
    for px in rgba.as_chunks::<4>().0.iter().step_by(7) {
        let (r, g, b) = (px[0] as f64, px[1] as f64, px[2] as f64);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        // Colourful, not too dark or blown out, counts most.
        let w = 0.05 + sat * sat * (max / 255.0) * (1.0 - (max / 255.0 - 0.6).abs());
        sum[0] += r * w;
        sum[1] += g * w;
        sum[2] += b * w;
        weight += w;
    }
    if weight == 0.0 {
        return [40, 44, 70];
    }
    let mut c = sum.map(|v| v / weight);
    // Cap the brightness (luma about 110 of 255).
    let luma = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
    if luma > 110.0 {
        c = c.map(|v| v * 110.0 / luma);
    }
    c.map(|v| v.round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Origin, ScanStats};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("localbeat-covers-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn album_in(dir: &Path) -> (Album, Vec<Song>) {
        let song = Song {
            path: dir.join("01.flac"),
            size: 1,
            mtime: 1,
            ctime: 0,
            ctime_nsec: 0,
            tag_schema: 1,
            added: 1,
            title: "x".into(),
            artist: "A".into(),
            album: "B".into(),
            album_artist: "A".into(),
            track: Some(1),
            disc: None,
            year: None,
            genre: None,
            genres: Vec::new(),
            moods: Vec::new(),
            ai_enriched: None,
            seconds: 1.0,
            codec: "FLAC".into(),
            sample_rate: None,
            bits: None,
            bitrate: None,
            drm: false,
            origin: Origin::Flac,
            picture: false,
            server_path: None,
        };
        let lib = crate::library::Library::build(vec![song], &mut ScanStats::default());
        (lib.albums[0].clone(), lib.songs)
    }

    #[test]
    fn prefers_cover_named_files_and_makes_a_thumbnail() {
        let d = tmpdir("find");
        let img = image::RgbImage::from_fn(600, 400, |x, _| image::Rgb([(x % 255) as u8, 30, 200]));
        img.save(d.join("AlbumArtSmall.jpg")).unwrap();
        img.save(d.join("Front.jpg")).unwrap();
        img.save(d.join("back.jpg")).unwrap();
        let (album, songs) = album_in(&d);
        assert_eq!(find(&album, &songs), Some(Source::File(d.join("Front.jpg"))));
        let cache = d.join("cache");
        let t = thumbnail(&album, &songs, &cache, 128).unwrap().unwrap();
        let back = image::open(&t).unwrap();
        assert_eq!((back.width(), back.height()), (128, 128));
        // Second call reuses it.
        let m1 = std::fs::metadata(&t).unwrap().modified().unwrap();
        thumbnail(&album, &songs, &cache, 128).unwrap();
        assert_eq!(std::fs::metadata(&t).unwrap().modified().unwrap(), m1);
    }

    #[test]
    fn no_cover_is_none() {
        let d = tmpdir("none");
        let (album, songs) = album_in(&d);
        assert_eq!(find(&album, &songs), None);
        assert!(thumbnail(&album, &songs, &d, 64).unwrap().is_none());
    }

    #[test]
    fn accent_follows_the_colourful_part_and_stays_dark() {
        // Mostly grey, a quarter strong red.
        let mut px = Vec::new();
        for i in 0..1000 {
            px.extend_from_slice(if i % 4 == 0 { &[230, 20, 20, 255] } else { &[128, 128, 128, 255] });
        }
        let c = accent_color(&px);
        assert!(c[0] > c[1] + 40 && c[0] > c[2] + 40, "{c:?}");
        assert!(0.299 * c[0] as f64 + 0.587 * c[1] as f64 + 0.114 * c[2] as f64 <= 111.0);
    }
}
