//! Reading audio files other than our FLACs: AAC/ALAC (.m4a), MP3, and anything
//! else Symphonia handles, for phone copies of the Miscellaneous library.
//! FLAC goes through [`crate::flacfile`] (the reference decoder we verify with).
//!
//! DRM-protected iTunes files (.m4p) can't be decoded: [`is_protected`].

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTagKey, Value};
use symphonia::core::probe::{Hint, ProbeResult};

/// Tags (Vorbis comment names), front cover and bitrate of a file.
#[derive(Debug, Default, Clone)]
pub struct SourceInfo {
    pub tags: Vec<(String, String)>,
    pub cover: Option<Vec<u8>>,
    /// Average bitrate, for not making a lossy file bigger than it was.
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
    /// Length in seconds, if the file says.
    pub seconds: Option<f64>,
}

/// Audio file extensions the phone sync takes from the Miscellaneous library.
pub const AUDIO_EXTENSIONS: [&str; 6] = ["flac", "m4a", "mp3", "aac", "ogg", "opus"];

/// iTunes Store purchases from before 2009 (FairPlay DRM): unplayable outside
/// Apple's software, so they're listed, never converted.
pub fn is_protected(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m4p"))
}

fn probe(path: &Path) -> Result<ProbeResult> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .with_context(|| format!("reading {}", path.display()))
}

pub fn read_info(path: &Path) -> Result<SourceInfo> {
    let mut probed = probe(path)?;
    let mut info = SourceInfo::default();
    // ID3 tags come with the probe; MP4 (iTunes) tags with the format reader.
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        absorb(&mut info, rev);
    }
    if let Some(rev) = probed.format.metadata().current() {
        absorb(&mut info, rev);
    }
    let track = probed.format.default_track().ok_or_else(|| anyhow!("no audio track"))?;
    let p = &track.codec_params;
    info.lossless = matches!(
        p.codec,
        symphonia::core::codecs::CODEC_TYPE_ALAC | symphonia::core::codecs::CODEC_TYPE_FLAC
    ) || p.codec.to_string().starts_with("pcm");
    if let (Some(frames), Some(rate)) = (p.n_frames, p.sample_rate) {
        let secs = frames as f64 / rate as f64;
        info.seconds = Some(secs);
        if secs > 1.0 {
            let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            info.bitrate_kbps = Some((bytes as f64 * 8.0 / secs / 1000.0).round() as u32);
        }
    }
    Ok(info)
}

fn absorb(info: &mut SourceInfo, rev: &MetadataRevision) {
    for tag in rev.tags() {
        let value = match &tag.value {
            Value::String(s) => s.trim().to_string(),
            Value::UnsignedInt(n) => n.to_string(),
            Value::SignedInt(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Boolean(b) => b.to_string(),
            _ => continue,
        };
        if value.is_empty() {
            continue;
        }
        if tag.std_key.is_none() {
            if tag.key.eq_ignore_ascii_case("MOOD") || tag.key.eq_ignore_ascii_case(crate::ai_enrich::MP4_MOOD_KEY) {
                push_multi(info, "MOOD", &value);
            } else if tag.key.eq_ignore_ascii_case(crate::ai_enrich::AI_ENRICHED_KEY)
                || tag.key.eq_ignore_ascii_case(crate::ai_enrich::MP4_AI_ENRICHED_KEY)
            {
                set(info, crate::ai_enrich::AI_ENRICHED_KEY, &value);
            }
            continue;
        }
        let Some(key) = tag.std_key else { continue };
        // "3/12" style track and disc numbers (iTunes, ID3).
        let split = |n: &str, total: &str, info: &mut SourceInfo| {
            let (a, b) = value.split_once('/').unwrap_or((value.as_str(), ""));
            set(info, n, a.trim());
            set(info, total, b.trim());
        };
        match key {
            StandardTagKey::TrackTitle => set(info, "TITLE", &value),
            StandardTagKey::Artist => set(info, "ARTIST", &value),
            StandardTagKey::Album => set(info, "ALBUM", &value),
            StandardTagKey::AlbumArtist => set(info, "ALBUMARTIST", &value),
            StandardTagKey::TrackNumber => split("TRACKNUMBER", "TRACKTOTAL", info),
            StandardTagKey::TrackTotal => set(info, "TRACKTOTAL", &value),
            StandardTagKey::DiscNumber => split("DISCNUMBER", "DISCTOTAL", info),
            StandardTagKey::DiscTotal => set(info, "DISCTOTAL", &value),
            StandardTagKey::Date | StandardTagKey::ReleaseDate => set(info, "DATE", &value),
            StandardTagKey::OriginalDate => set(info, "ORIGINALDATE", &value),
            StandardTagKey::Genre => push_multi(info, "GENRE", &value),
            StandardTagKey::Mood => push_multi(info, "MOOD", &value),
            StandardTagKey::Composer => set(info, "COMPOSER", &value),
            StandardTagKey::Comment => set(info, "COMMENT", &value),
            StandardTagKey::MusicBrainzAlbumId => set(info, "MUSICBRAINZ_ALBUMID", &value),
            StandardTagKey::MusicBrainzTrackId => set(info, "MUSICBRAINZ_TRACKID", &value),
            StandardTagKey::MusicBrainzArtistId => set(info, "MUSICBRAINZ_ARTISTID", &value),
            _ => {}
        }
    }
    if info.cover.is_none() {
        let visuals = rev.visuals();
        let front = visuals
            .iter()
            .find(|v| v.usage == Some(symphonia::core::meta::StandardVisualKey::FrontCover))
            .or(visuals.first());
        info.cover = front.map(|v| v.data.to_vec());
    }
}

/// Sets a tag once (the first value wins; a "0" track total is ignored).
fn set(info: &mut SourceInfo, key: &str, value: &str) {
    if value.is_empty() || (key.ends_with("TOTAL") && value == "0") {
        return;
    }
    if !info.tags.iter().any(|(k, _)| k == key) {
        info.tags.push((key.to_string(), value.to_string()));
    }
}

/// Appends each distinct `;` / `\0`-separated value (for multi-value GENRE / MOOD).
fn push_multi(info: &mut SourceInfo, key: &str, value: &str) {
    for part in value.split([';', '\0']) {
        let p = part.trim();
        if !p.is_empty() && !info.tags.iter().any(|(k, v)| k == key && v.eq_ignore_ascii_case(p)) {
            info.tags.push((key.to_string(), p.to_string()));
        }
    }
}

/// Decodes to interleaved f32, calling `f(samples, rate, channels)` per packet.
pub fn decode_float(path: &Path, mut f: impl FnMut(&[f32], u32, u32) -> Result<()>) -> Result<()> {
    let probed = probe(path)?;
    let mut format: Box<dyn FormatReader> = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| format!("no decoder for {}", path.display()))?;
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut bad_packets = 0;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if packet.track_id() != id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            // A damaged frame in a lossy file: skip it, like players do.
            Err(SymError::DecodeError(_)) => {
                bad_packets += 1;
                if bad_packets > 100 {
                    bail!("too many damaged frames in {}", path.display());
                }
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("decoding {}", path.display())),
        };
        let spec = *audio.spec();
        let needed = audio.capacity() * spec.channels.count();
        if buf.as_ref().is_none_or(|b| b.capacity() < needed) {
            buf = Some(SampleBuffer::new(audio.capacity() as u64, spec));
        }
        let b = buf.as_mut().unwrap();
        b.copy_interleaved_ref(audio);
        if !b.samples().is_empty() {
            f(b.samples(), spec.rate, spec.channels.count() as u32)?;
        }
    }
    Ok(())
}
