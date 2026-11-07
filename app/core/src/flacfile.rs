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
