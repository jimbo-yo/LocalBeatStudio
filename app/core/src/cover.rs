//! Front covers from the Cover Art Archive.

use anyhow::{Context, Result};

use crate::flac_encode::Picture;
use crate::http;
use crate::musicbrainz::Release;

/// The release's front cover (1200 px version), else its release group's.
/// `Ok(None)` if neither has one.
pub fn fetch_front(release: &Release) -> Result<Option<Picture>> {
    let mut urls = vec![format!("https://coverartarchive.org/release/{}/front-1200", release.id)];
    if !release.release_group_id.is_empty() {
        urls.push(format!("https://coverartarchive.org/release-group/{}/front-1200", release.release_group_id));
    }
    for url in urls {
        match http::agent().get(&url).call() {
            Ok(mut resp) => {
                let data = resp
                    .body_mut()
                    .with_config()
                    .limit(30 * 1024 * 1024)
                    .read_to_vec()
                    .with_context(|| format!("downloading {url}"))?;
                return Ok(Some(picture(data)?));
            }
            Err(ureq::Error::StatusCode(404)) => continue,
            Err(e) => return Err(e).with_context(|| format!("fetching {url}")),
        }
    }
    Ok(None)
}

/// Wraps image bytes, reading the type and size from the data itself.
pub fn picture(data: Vec<u8>) -> Result<Picture> {
    let size = imagesize::blob_size(&data).context("cover image isn't a readable image")?;
    let mime = match imagesize::image_type(&data) {
        Ok(imagesize::ImageType::Png) => "image/png",
        Ok(imagesize::ImageType::Webp) => "image/webp",
        Ok(imagesize::ImageType::Gif) => "image/gif",
        _ => "image/jpeg",
    };
    Ok(Picture { mime: mime.into(), width: size.width as u32, height: size.height as u32, data })
}

/// File name for the cover saved next to the tracks.
pub fn file_name(p: &Picture) -> &'static str {
    match p.mime.as_str() {
        "image/png" => "cover.png",
        "image/webp" => "cover.webp",
        "image/gif" => "cover.gif",
        _ => "cover.jpg",
    }
}
