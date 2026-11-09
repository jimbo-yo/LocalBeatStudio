//! Phone copies: mirrors a FLAC folder tree as Opus (flac2phone.sh's job).
//!
//! - only new or changed FLACs are converted (copy newer than its FLAC = done);
//! - tags are copied except ReplayGain/R128 (opusenc once turned those into a
//!   +99.6 dB header gain), and the front cover is embedded;
//! - cover.jpg/folder.jpg (and .png) are copied alongside;
//! - written to `<name>.part` then renamed; all CPU cores.
//!
//! Encoding uses the reference libopusenc (libopus), which resamples
//! 44.1 kHz to Opus's 48 kHz itself.

use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use libopusenc_static_sys as ope;
// libopusenc calls into libopus; linking this crate pulls libopus in.
use opus_static_sys as _;

use crate::{audio_source, flacfile};

pub const DEFAULT_BITRATE_KBPS: u32 = 160;

/// opus_defines.h: OPUS_SET_BITRATE_REQUEST.
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;

const COVER_FILES: [&str; 4] = ["cover.jpg", "cover.png", "folder.jpg", "folder.png"];

#[derive(Debug, Default)]
pub struct Stats {
    pub converted: Vec<PathBuf>,
    pub up_to_date: usize,
    pub failed: Vec<(PathBuf, String)>,
    pub covers_copied: usize,
    /// DRM-protected iTunes files (.m4p): skipped, can't be converted.
    pub protected: Vec<PathBuf>,
}

/// Converts every audio file under `src` (FLAC, AAC/ALAC .m4a, MP3, Ogg
/// Vorbis; not files that are already Opus) to `dest/<same relative path>.opus`.
/// `on_file(relative path, result)` is called as each one finishes
/// (from worker threads).
pub fn convert_tree(
    src: &Path,
    dest: &Path,
    bitrate_kbps: u32,
    on_file: &(dyn Fn(&Path, &Result<()>) + Sync),
) -> Result<Stats> {
    let src = src.canonicalize().with_context(|| format!("source folder {}", src.display()))?;
    let dest_abs = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());

    let mut flacs = Vec::new();
    let mut covers = Vec::new();
    let mut protected = Vec::new();
    // Skip the destination if it's inside the source.
    let walker = walkdir::WalkDir::new(&src).into_iter().filter_entry(|e| {
        e.path().canonicalize().map(|p| p != dest_abs && !dest_abs.starts_with(&p) || p == src).unwrap_or(true)
    });
    for e in walker.filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let name = e.file_name().to_string_lossy().to_lowercase();
        let ext = name.rsplit_once('.').map(|(_, x)| x).unwrap_or("");
        if ext != "opus" && audio_source::AUDIO_EXTENSIONS.contains(&ext) {
            flacs.push(e.into_path());
        } else if audio_source::is_protected(e.path()) {
            protected.push(e.path().strip_prefix(&src).unwrap_or(e.path()).to_path_buf());
        } else if COVER_FILES.contains(&name.as_str()) {
            covers.push(e.into_path());
        }
    }
    flacs.sort();

    let mut stats = Stats { protected, ..Default::default() };
    stats.protected.sort();
    let mut todo = Vec::new();
    for f in &flacs {
        let rel = f.strip_prefix(&src).expect("under src");
        let out = dest.join(rel).with_extension("opus");
        if is_up_to_date(f, &out) {
            stats.up_to_date += 1;
        } else {
            todo.push((f.clone(), out, rel.to_path_buf()));
        }
    }

    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(todo.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((input, out, rel)) = todo.get(i) else { break };
                    let r = convert_file(input, out, bitrate_kbps);
                    on_file(rel, &r);
                    results.lock().unwrap().push((rel.clone(), r));
                }
            });
        }
    });
    for (rel, r) in results.into_inner().unwrap() {
        match r {
            Ok(()) => stats.converted.push(rel),
            Err(e) => stats.failed.push((rel, format!("{e:#}"))),
        }
    }
    stats.converted.sort();

    for c in covers {
        let rel = c.strip_prefix(&src).expect("under src");
        let out = dest.join(rel);
        if !is_up_to_date(&c, &out) {
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&c, &out).with_context(|| format!("copying {}", c.display()))?;
            stats.covers_copied += 1;
        }
    }
    Ok(stats)
}

fn is_up_to_date(input: &Path, out: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(input), mtime(out)) {
        (Some(i), Some(o)) => o >= i,
        _ => false,
    }
}

/// Tags not copied to the phone copy.
fn skip_tag(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.starts_with("REPLAYGAIN_") || n.starts_with("R128_") || n == "METADATA_BLOCK_PICTURE"
}

fn is_flac(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("flac"))
}

/// Lowest bitrate used for a lossy source.
const MIN_LOSSY_KBPS: u32 = 64;

/// Converts one audio file to Opus at `out` (via `out.part`). FLAC goes
/// through the reference decoder; AAC, MP3 and others through Symphonia, and
/// a lossy source is encoded at no more than its own bitrate (a 128 kbps MP3
/// doesn't get bigger), but at least 64 kbps.
pub fn convert_file(input: &Path, out: &Path, bitrate_kbps: u32) -> Result<()> {
    let (tags, embedded, kbps) = if is_flac(input) {
        let pictures = flacfile::read_pictures(input).unwrap_or_default();
        let cover = pictures.iter().find(|p| p.kind == 3).or(pictures.first()).map(|p| p.data.clone());
        (flacfile::read_tags(input)?.tags, cover, bitrate_kbps)
    } else {
        if audio_source::is_protected(input) {
            bail!("{} is DRM-protected (iTunes .m4p) and can't be converted", input.display());
        }
        let info = audio_source::read_info(input)?;
        let kbps = match info.bitrate_kbps {
            Some(b) if !info.lossless => b.clamp(MIN_LOSSY_KBPS, bitrate_kbps.max(MIN_LOSSY_KBPS)),
            _ => bitrate_kbps,
        };
        (info.tags, info.cover, kbps)
    };
    let cover: Option<Vec<u8>> = embedded
        .or_else(|| {
            let dir = input.parent()?;
            COVER_FILES.iter().find_map(|n| std::fs::read(dir.join(n)).ok())
        });

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let name = out.file_name().ok_or_else(|| anyhow!("bad output path"))?.to_string_lossy();
    let tmp = out.with_file_name(format!("{name}.part"));
    let result = encode(input, &tmp, &tags, cover.as_deref(), kbps);
    match result {
        Ok(()) => std::fs::rename(&tmp, out).with_context(|| format!("renaming to {}", out.display())),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("converting {}", input.display()))
        }
    }
}

struct Sink {
    out: BufWriter<File>,
    error: Option<std::io::Error>,
}

unsafe extern "C" fn write_cb(user: *mut c_void, ptr: *const u8, len: i32) -> c_int {
    let sink = unsafe { &mut *(user as *mut Sink) };
    let data = if ptr.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(ptr, len as usize) } };
    match sink.out.write_all(data) {
        Ok(()) => 0,
        Err(e) => {
            sink.error = Some(e);
            1
        }
    }
}

unsafe extern "C" fn close_cb(user: *mut c_void) -> c_int {
    let sink = unsafe { &mut *(user as *mut Sink) };
    match sink.out.flush() {
        Ok(()) => 0,
        Err(e) => {
            sink.error = Some(e);
            1
        }
    }
}

struct Comments(*mut ope::OggOpusComments);
impl Drop for Comments {
    fn drop(&mut self) {
        unsafe { ope::ope_comments_destroy(self.0) }
    }
}

struct Encoder(*mut ope::OggOpusEnc);
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ope::ope_encoder_destroy(self.0) }
    }
}

fn ope_check(code: c_int, what: &str) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    let msg = unsafe { std::ffi::CStr::from_ptr(ope::ope_strerror(code)) }.to_string_lossy().into_owned();
    bail!("libopusenc: {what}: {msg}")
}

fn encode(input: &Path, tmp: &Path, tags: &[(String, String)], cover: Option<&[u8]>, kbps: u32) -> Result<()> {
    let file = File::create(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    // Boxed so its address stays put while libopusenc holds it.
    let mut sink = Box::new(Sink { out: BufWriter::new(file), error: None });

    unsafe {
        let comments = Comments(ope::ope_comments_create());
        if comments.0.is_null() {
            bail!("libopusenc: out of memory");
        }
        for (k, v) in tags.iter().filter(|(k, _)| !skip_tag(k)) {
            let (Ok(k), Ok(v)) = (CString::new(k.as_str()), CString::new(v.as_str())) else { continue };
            ope_check(ope::ope_comments_add(comments.0, k.as_ptr(), v.as_ptr()), "adding a tag")?;
        }
        if let Some(img) = cover {
            let desc = CString::new("").unwrap();
            // 3 = front cover. A picture libopusenc can't parse is skipped.
            let _ = ope::ope_comments_add_picture_from_memory(
                comments.0,
                img.as_ptr() as *const c_char,
                img.len(),
                3,
                desc.as_ptr(),
            );
        }

        let callbacks = ope::OpusEncCallbacks { write: Some(write_cb), close: Some(close_cb) };
        let mut enc: Option<Encoder> = None;
        let sink_ptr = &mut *sink as *mut Sink as *mut c_void;

        let on_block = |pcm: &[f32], rate: u32, channels: u32| -> Result<()> {
            if channels == 0 || channels > 2 {
                bail!("{channels} channels: only mono/stereo supported");
            }
            if enc.is_none() {
                let mut err: c_int = 0;
                let e = ope::ope_encoder_create_callbacks(
                    &callbacks,
                    sink_ptr,
                    comments.0,
                    rate as i32,
                    channels as c_int,
                    0,
                    &mut err,
                );
                if e.is_null() {
                    ope_check(err, "creating encoder")?;
                    bail!("libopusenc: creating encoder failed");
                }
                ope_check(ope::ope_encoder_ctl(e, OPUS_SET_BITRATE_REQUEST, (kbps * 1000) as i32), "setting bitrate")?;
                enc = Some(Encoder(e));
            }
            let e = enc.as_ref().unwrap().0;
            ope_check(ope::ope_encoder_write_float(e, pcm.as_ptr(), (pcm.len() / channels as usize) as c_int), "encoding")
        };
        if is_flac(input) { flacfile::decode_float(input, on_block)? } else { audio_source::decode_float(input, on_block)? }

        let enc = enc.ok_or_else(|| anyhow!("no audio in {}", input.display()))?;
        ope_check(ope::ope_encoder_drain(enc.0), "finishing")?;
        drop(enc);
    }
    if let Some(e) = sink.error.take() {
        return Err(e).context("writing the Opus file");
    }
    sink.out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flac_encode::{self, Picture};

    #[test]
    fn mirrors_tree_skips_gain_tags_and_up_to_date_files() {
        let root = std::env::temp_dir().join(format!("ripshelf-phone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let album = root.join("FLAC").join("Björk").join("Who？");
        std::fs::create_dir_all(&album).unwrap();
        let frames: Vec<u32> = (0..44_100 * 2)
            .map(|i| {
                let v = ((i as f64 / 44_100.0 * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                crate::accuraterip::pack_frame(v, v)
            })
            .collect();
        let tags = vec![
            ("TITLE".to_string(), "Sittin’ on Top".to_string()),
            ("REPLAYGAIN_TRACK_GAIN".to_string(), "-7.5 dB".to_string()),
        ];
        // A minimal valid 1x1 PNG.
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 0x90, 0x77, 0x53, 0xDE, 0, 0, 0, 12, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF,
            0xC0, 0, 0, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ];
        let pic = Picture { mime: "image/png".into(), data: png.clone(), width: 1, height: 1 };
        flac_encode::write_flac(&album.join("01. Björk - Sittin’.flac"), &frames, &tags, Some(&pic), 0).unwrap();
        std::fs::write(album.join("cover.png"), &png).unwrap();

        let phone = root.join("phone");
        let stats = convert_tree(&root.join("FLAC"), &phone, 160, &|_, _| {}).unwrap();
        assert!(stats.failed.is_empty(), "{:?}", stats.failed);
        assert_eq!(stats.converted.len(), 1);
        assert_eq!(stats.covers_copied, 1);
        let out = phone.join("Björk").join("Who？").join("01. Björk - Sittin’.opus");
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"OggS"));
        let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(has("TITLE=Sittin’ on Top".as_bytes()));
        assert!(!has(b"REPLAYGAIN"), "gain tags must not be copied");
        assert!(has(b"METADATA_BLOCK_PICTURE"), "cover embedded");
        assert!(phone.join("Björk").join("Who？").join("cover.png").exists());

        // Second run: nothing to do.
        let stats = convert_tree(&root.join("FLAC"), &phone, 160, &|_, _| {}).unwrap();
        assert_eq!((stats.converted.len(), stats.up_to_date, stats.covers_copied), (0, 1, 0));
        std::fs::remove_dir_all(&root).ok();
    }
}
