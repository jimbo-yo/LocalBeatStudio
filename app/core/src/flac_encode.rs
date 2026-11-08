//! FLAC encoding with the reference libFLAC (built from source by
//! libflac-sys), in verify mode: libFLAC decodes every block it writes and
//! compares it with the input, so a bad encode fails instead of reaching the
//! archive.
//!
//! Output goes through Rust's `File` (libFLAC's stream API), so Unicode paths
//! work on Windows too.

use std::ffi::{CString, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use libflac_sys as ffi;

/// Front cover to embed (PICTURE block, type 3).
pub struct Picture {
    pub mime: String,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Picture({}, {}x{}, {} bytes)", self.mime, self.width, self.height, self.data.len())
    }
}

/// Room left after the tags so later edits (mb-enrich, re-tagging) don't
/// rewrite the whole file. metaflac's default.
const PADDING_BYTES: u32 = 8192;

/// Encodes CD frames (see `accuraterip`) to `path`. `compression`: 0-8, 5 is
/// flac's default. Overwrites `path`: callers write to a temporary name and
/// rename.
pub fn write_flac(
    path: &Path,
    frames: &[u32],
    tags: &[(String, String)],
    cover: Option<&Picture>,
    compression: u32,
) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut sink = Sink { out: BufWriter::new(file), error: None };
    let result = encode(&mut sink, frames, tags, cover, compression);
    let flushed = sink.out.flush();
    let result = result.and_then(|()| flushed.map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result.with_context(|| format!("encoding {}", path.display()))
}

struct Sink {
    out: BufWriter<File>,
    error: Option<io::Error>,
}

/// Owns libFLAC objects so every exit path frees them.
struct Owned {
    encoder: *mut ffi::FLAC__StreamEncoder,
    blocks: Vec<*mut ffi::FLAC__StreamMetadata>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                ffi::FLAC__stream_encoder_delete(self.encoder);
            }
            for &b in &self.blocks {
                ffi::FLAC__metadata_object_delete(b);
            }
        }
    }
}

fn check(ok: ffi::FLAC__bool, what: &str) -> Result<()> {
    if ok == 0 { Err(anyhow!("libFLAC: {what} failed")) } else { Ok(()) }
}

fn encode(
    sink: &mut Sink,
    frames: &[u32],
    tags: &[(String, String)],
    cover: Option<&Picture>,
    compression: u32,
) -> Result<()> {
    unsafe {
        let mut o = Owned { encoder: ffi::FLAC__stream_encoder_new(), blocks: Vec::new() };
        if o.encoder.is_null() {
            bail!("libFLAC: out of memory");
        }
        let e = o.encoder;

        // Metadata: tags, cover, padding.
        let vc = ffi::FLAC__metadata_object_new(ffi::FLAC__METADATA_TYPE_VORBIS_COMMENT);
        if vc.is_null() {
            bail!("libFLAC: out of memory");
        }
        o.blocks.push(vc);
        for (name, value) in tags {
            let n = CString::new(name.as_str()).map_err(|_| anyhow!("tag name {name:?} contains NUL"))?;
            let v = CString::new(value.as_str()).map_err(|_| anyhow!("tag {name} value contains NUL"))?;
            let mut entry = ffi::FLAC__StreamMetadata_VorbisComment_Entry { length: 0, entry: std::ptr::null_mut() };
            if ffi::FLAC__metadata_object_vorbiscomment_entry_from_name_value_pair(&mut entry, n.as_ptr(), v.as_ptr())
                == 0
            {
                bail!("invalid tag name {name:?}");
            }
            // copy = false: the block takes ownership of the entry.
            check(ffi::FLAC__metadata_object_vorbiscomment_append_comment(vc, entry, 0), "adding a tag")?;
        }

        if let Some(p) = cover {
            let pic = ffi::FLAC__metadata_object_new(ffi::FLAC__METADATA_TYPE_PICTURE);
            if pic.is_null() {
                bail!("libFLAC: out of memory");
            }
            o.blocks.push(pic);
            let mime = CString::new(p.mime.as_str())?;
            check(ffi::FLAC__metadata_object_picture_set_mime_type(pic, mime.as_ptr() as *mut _, 1), "picture type")?;
            let len = u32::try_from(p.data.len()).map_err(|_| anyhow!("cover image too large"))?;
            check(ffi::FLAC__metadata_object_picture_set_data(pic, p.data.as_ptr() as *mut _, len, 1), "picture data")?;
            let pd = &mut (*pic).data.picture;
            pd.type_ = ffi::FLAC__STREAM_METADATA_PICTURE_TYPE_FRONT_COVER;
            pd.width = p.width;
            pd.height = p.height;
            pd.depth = 24;
        }

        let pad = ffi::FLAC__metadata_object_new(ffi::FLAC__METADATA_TYPE_PADDING);
        if pad.is_null() {
            bail!("libFLAC: out of memory");
        }
        o.blocks.push(pad);
        (*pad).length = PADDING_BYTES;

        check(ffi::FLAC__stream_encoder_set_channels(e, 2), "set channels")?;
        check(ffi::FLAC__stream_encoder_set_bits_per_sample(e, 16), "set bits")?;
        check(ffi::FLAC__stream_encoder_set_sample_rate(e, 44_100), "set rate")?;
        check(ffi::FLAC__stream_encoder_set_compression_level(e, compression), "set compression")?;
        check(ffi::FLAC__stream_encoder_set_verify(e, 1), "set verify")?;
        check(ffi::FLAC__stream_encoder_set_total_samples_estimate(e, frames.len() as u64), "set length")?;
        check(ffi::FLAC__stream_encoder_set_metadata(e, o.blocks.as_mut_ptr(), o.blocks.len() as u32), "set metadata")?;

        let status = ffi::FLAC__stream_encoder_init_stream(
            e,
            Some(write_cb),
            Some(seek_cb),
            Some(tell_cb),
            None,
            sink as *mut Sink as *mut c_void,
        );
        if status != ffi::FLAC__STREAM_ENCODER_INIT_STATUS_OK {
            bail!("libFLAC: encoder init failed (status {status})");
        }

        let mut buf: Vec<i32> = Vec::with_capacity(2 * 4096);
        for chunk in frames.chunks(4096) {
            buf.clear();
            for &f in chunk {
                buf.push(f as u16 as i16 as i32);
                buf.push((f >> 16) as u16 as i16 as i32);
            }
            if ffi::FLAC__stream_encoder_process_interleaved(e, buf.as_ptr(), chunk.len() as u32) == 0 {
                return Err(encoder_error(e, sink));
            }
        }
        if ffi::FLAC__stream_encoder_finish(e) == 0 {
            return Err(encoder_error(e, sink));
        }
        Ok(())
    }
}

unsafe fn encoder_error(e: *mut ffi::FLAC__StreamEncoder, sink: &mut Sink) -> anyhow::Error {
    if let Some(io) = sink.error.take() {
        return anyhow!(io).context("writing the file");
    }
    let state = unsafe { ffi::FLAC__stream_encoder_get_state(e) };
    if state == ffi::FLAC__STREAM_ENCODER_VERIFY_MISMATCH_IN_AUDIO_DATA {
        anyhow!("libFLAC verify failed: decoded audio differs from the input")
    } else {
        anyhow!("libFLAC encoder error (state {state})")
    }
}

unsafe extern "C" fn write_cb(
    _e: *const ffi::FLAC__StreamEncoder,
    buffer: *const ffi::FLAC__byte,
    bytes: usize,
    _samples: u32,
    _frame: u32,
    client: *mut c_void,
) -> ffi::FLAC__StreamEncoderWriteStatus {
    let sink = unsafe { &mut *(client as *mut Sink) };
    let data = unsafe { std::slice::from_raw_parts(buffer, bytes) };
    match sink.out.write_all(data) {
        Ok(()) => ffi::FLAC__STREAM_ENCODER_WRITE_STATUS_OK,
        Err(err) => {
            sink.error = Some(err);
            ffi::FLAC__STREAM_ENCODER_WRITE_STATUS_FATAL_ERROR
        }
    }
}

unsafe extern "C" fn seek_cb(
    _e: *const ffi::FLAC__StreamEncoder,
    offset: ffi::FLAC__uint64,
    client: *mut c_void,
) -> ffi::FLAC__StreamEncoderSeekStatus {
    let sink = unsafe { &mut *(client as *mut Sink) };
    match sink.out.seek(SeekFrom::Start(offset)) {
        Ok(_) => ffi::FLAC__STREAM_ENCODER_SEEK_STATUS_OK,
        Err(err) => {
            sink.error = Some(err);
            ffi::FLAC__STREAM_ENCODER_SEEK_STATUS_ERROR
        }
    }
}

unsafe extern "C" fn tell_cb(
    _e: *const ffi::FLAC__StreamEncoder,
    offset: *mut ffi::FLAC__uint64,
    client: *mut c_void,
) -> ffi::FLAC__StreamEncoderTellStatus {
    let sink = unsafe { &mut *(client as *mut Sink) };
    match sink.out.stream_position() {
        Ok(p) => {
            unsafe { *offset = p };
            ffi::FLAC__STREAM_ENCODER_TELL_STATUS_OK
        }
        Err(err) => {
            sink.error = Some(err);
            ffi::FLAC__STREAM_ENCODER_TELL_STATUS_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flacfile;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ripshelf-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn music_like(n: usize) -> Vec<u32> {
        // Two sine-ish tones plus a little noise: compressible but not trivial.
        let mut x: u32 = 0x9E37_79B9;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let t = i as f64 / 44_100.0;
                let l = (8000.0 * (t * 440.0 * std::f64::consts::TAU).sin()) as i16 ^ (x & 0x7) as i16;
                let r = (6000.0 * (t * 660.0 * std::f64::consts::TAU).sin()) as i16 ^ ((x >> 8) & 0x7) as i16;
                crate::accuraterip::pack_frame(l, r)
            })
            .collect()
    }

    #[test]
    fn round_trip_audio_tags_and_unicode_path() {
        let dir = temp_dir("flac");
        let path = dir.join("07. Björk - Who？ “Live”.flac");
        let frames = music_like(44_100 * 3 + 123);
        let tags = vec![
            ("TITLE".to_string(), "Sittin’ on Top of the World".to_string()),
            ("ARTIST".to_string(), "Björk".to_string()),
            ("TRACKNUMBER".to_string(), "7".to_string()),
        ];
        let cover = Picture { mime: "image/jpeg".into(), data: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3], width: 1, height: 1 };
        write_flac(&path, &frames, &tags, Some(&cover), 5).unwrap();

        let (f, decoded) = flacfile::read_cd_audio(&path).unwrap();
        assert_eq!(decoded, frames);
        assert_eq!(f.tag("title"), Some("Sittin’ on Top of the World"));
        assert_eq!(f.track_number(), Some(7));
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.windows(10).any(|w| w == b"image/jpeg"));
        assert!(raw.len() < frames.len() * 4, "should compress");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_bad_tag_name() {
        let dir = temp_dir("badtag");
        let path = dir.join("x.flac");
        let err = write_flac(&path, &[0; 588], &[("BAD=NAME".into(), "v".into())], None, 5).unwrap_err();
        assert!(format!("{err:#}").contains("invalid tag name"));
        assert!(!path.exists(), "failed encode leaves no file");
        std::fs::remove_dir_all(&dir).ok();
    }
}
