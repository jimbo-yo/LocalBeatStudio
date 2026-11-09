//! Reading FLAC files: tags and CD audio frames.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::accuraterip::pack_frame;

pub struct FlacFile {
    /// Vorbis comments in file order; names as written (compare case-insensitively).
    pub tags: Vec<(String, String)>,
}

impl FlacFile {
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Track number from TRACKNUMBER ("7" or "7/12").
    pub fn track_number(&self) -> Option<usize> {
        self.tag("TRACKNUMBER")?.split('/').next()?.trim().parse().ok()
    }
}

/// Reads only the tags.
pub fn read_tags(path: &Path) -> Result<FlacFile> {
    let reader = claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(FlacFile { tags: collect_tags(&reader) })
}

/// Length in seconds, from STREAMINFO (None if the file doesn't say).
pub fn seconds(path: &Path) -> Option<f64> {
    let info = claxon::FlacReader::open(path).ok()?.streaminfo();
    Some(info.samples? as f64 / info.sample_rate.max(1) as f64)
}

/// Reads tags and decodes the audio as CD frames (44.1 kHz, 16-bit stereo
/// only).
pub fn read_cd_audio(path: &Path) -> Result<(FlacFile, Vec<u32>)> {
    let mut reader = claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let info = reader.streaminfo();
    if info.channels != 2 || info.bits_per_sample != 16 || info.sample_rate != 44_100 {
        bail!(
            "{}: not CD audio ({} Hz, {} bit, {} channels)",
            path.display(),
            info.sample_rate,
            info.bits_per_sample,
            info.channels
        );
    }
    let tags = collect_tags(&reader);

    let mut frames = Vec::with_capacity(info.samples.unwrap_or(0) as usize);
    let mut blocks = reader.blocks();
    let mut buf = Vec::new();
    while let Some(block) = blocks
        .read_next_or_eof(buf)
        .with_context(|| format!("decoding {}", path.display()))?
    {
        for i in 0..block.duration() {
            frames.push(pack_frame(block.sample(0, i) as i16, block.sample(1, i) as i16));
        }
        buf = block.into_buffer();
    }
    Ok((FlacFile { tags }, frames))
}

fn collect_tags<R: std::io::Read>(reader: &claxon::FlacReader<R>) -> Vec<(String, String)> {
    reader.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// A picture block from a FLAC file.
pub struct FlacPicture {
    /// 3 = front cover.
    pub kind: u32,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Reads the PICTURE metadata blocks (claxon skips them). Stops at the
/// first audio frame, so only the header is read.
pub fn read_pictures(path: &Path) -> Result<Vec<FlacPicture>> {
    use std::io::Read;
    let mut f = std::io::BufReader::new(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"fLaC" {
        bail!("{}: not a FLAC file", path.display());
    }
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 4];
        f.read_exact(&mut h)?;
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7F;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let mut block = vec![0u8; len];
        f.read_exact(&mut block)?;
        if kind == 6 {
            if let Some(p) = parse_picture(&block) {
                out.push(p);
            }
        }
        if last {
            break;
        }
    }
    Ok(out)
}

fn parse_picture(b: &[u8]) -> Option<FlacPicture> {
    let mut pos = 0usize;
    let u32_at = |pos: &mut usize| -> Option<u32> {
        let v = u32::from_be_bytes(b.get(*pos..*pos + 4)?.try_into().ok()?);
        *pos += 4;
        Some(v)
    };
    let kind = u32_at(&mut pos)?;
    let mime_len = u32_at(&mut pos)? as usize;
    let mime = String::from_utf8_lossy(b.get(pos..pos + mime_len)?).into_owned();
    pos += mime_len;
    let desc_len = u32_at(&mut pos)? as usize;
    pos += desc_len;
    pos += 16; // width, height, depth, colours
    let data_len = u32_at(&mut pos)? as usize;
    let data = b.get(pos..pos + data_len)?.to_vec();
    Some(FlacPicture { kind, mime, data })
}

/// Streams the audio as interleaved floats in -1..1, in blocks, with the
/// stream's sample rate and channel count.
pub fn decode_float(
    path: &Path,
    mut on_block: impl FnMut(&[f32], u32, u32) -> Result<()>,
) -> Result<()> {
    let mut reader = claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let info = reader.streaminfo();
    let (rate, ch) = (info.sample_rate, info.channels);
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let mut blocks = reader.blocks();
    let mut buf = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buf).with_context(|| format!("decoding {}", path.display()))? {
        pcm.clear();
        for i in 0..block.duration() {
            for c in 0..ch {
                pcm.push(block.sample(c, i) as f32 * scale);
            }
        }
        on_block(&pcm, rate, ch)?;
        buf = block.into_buffer();
    }
    Ok(())
}
