//! One file being decoded: Symphonia for every format (FLAC, MP3, AAC/ALAC
//! in .m4a, Ogg Vorbis, WAV, AIFF), always handed on as stereo f32 frames.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// What's playing, for the "FLAC 16/44.1" badge.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// Short codec name: "FLAC", "MP3", "AAC", "ALAC", "Vorbis", "PCM".
    pub codec: String,
    pub sample_rate: u32,
    pub bits: Option<u32>,
    pub channels: u16,
    pub lossless: bool,
}

pub struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    pub info: Format,
    /// Length in seconds, if the file says.
    pub duration: Option<f64>,
    /// After an accurate seek: frames still to drop before the target.
    skip_frames: u64,
    /// Whether timestamps count frames (time base 1/sample_rate), so a seek
    /// can be trimmed to the exact frame.
    ts_is_frames: bool,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext == "m4p" {
            bail!("protected by iTunes DRM: can't be played");
        }
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(Growing::new(file, path)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(&ext);
        let opts = FormatOptions { enable_gapless: true, ..Default::default() };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &opts, &MetadataOptions::default())
            .map_err(|e| anyhow!("not a playable audio file: {e}"))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track"))?;
        let p = &track.codec_params;
        let decoder = symphonia::default::get_codecs()
            .make(p, &DecoderOptions::default())
            .map_err(|e| anyhow!("can't decode this format: {e}"))?;
        let sample_rate = p.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
        let codec = symphonia::default::get_codecs()
            .get_codec(p.codec)
            .map(|d| d.short_name.to_string())
            .unwrap_or_else(|| "audio".into());
        let codec = match codec.as_str() {
            "flac" => "FLAC".to_string(),
            "mp3" => "MP3".to_string(),
            "aac" => "AAC".to_string(),
            "alac" => "ALAC".to_string(),
            "vorbis" => "Vorbis".to_string(),
            c if c.starts_with("pcm") => "PCM".to_string(),
            c => c.to_uppercase(),
        };
        let lossless = matches!(codec.as_str(), "FLAC" | "ALAC" | "PCM");
        let info = Format {
            lossless,
            bits: p.bits_per_sample.or(p.bits_per_coded_sample).filter(|_| lossless),
            channels: p.channels.map(|c| c.count() as u16).unwrap_or(2),
            sample_rate,
            codec,
        };
        let duration = p.n_frames.map(|n| n as f64 / sample_rate as f64);
        let ts_is_frames = p.time_base.is_none_or(|tb| tb.numer == 1 && tb.denom == sample_rate);
        let track_id = track.id;
        Ok(Source { format, decoder, track_id, info, duration, skip_frames: 0, ts_is_frames })
    }

    /// Moves to `secs`; returns where it really landed (seconds).
    pub fn seek(&mut self, secs: f64) -> Result<f64> {
        let secs = secs.max(0.0);
        let time = Time { seconds: secs.trunc() as u64, frac: secs.fract() };
        let to = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(|e| anyhow!("seek failed: {e}"))?;
        self.decoder.reset();
        // The reader stops at or before the target; the frames in between are
        // dropped as they're decoded.
        self.skip_frames = if self.ts_is_frames { to.required_ts.saturating_sub(to.actual_ts) } else { 0 };
        let rate = self.info.sample_rate as f64;
        Ok(if self.ts_is_frames { to.required_ts as f64 / rate } else { secs })
    }

    /// Decodes the next packet into `out` as interleaved stereo. Ok(false) at
    /// the end of the file.
    pub fn next(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(anyhow!("read error: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                // A damaged packet: skip it rather than stop the song.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(anyhow!("decode error: {e}")),
            };
            let spec = *decoded.spec();
            let ch = spec.channels.count().max(1);
            let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();
            let frames = samples.len() / ch;
            let skip = (self.skip_frames as usize).min(frames);
            self.skip_frames -= skip as u64;
            out.reserve((frames - skip) * 2);
            for f in skip..frames {
                let s = &samples[f * ch..f * ch + ch];
                let (l, r) = if ch == 1 { (s[0], s[0]) } else { (s[0], s[1]) };
                out.push(l);
                out.push(r);
            }
            if frames > skip {
                return Ok(true);
            }
        }
    }
}

/// How long to wait for more of a song that's still downloading.
const GROW_WAIT: Duration = Duration::from_secs(60);

/// A file that may still be arriving: while `<file>.downloading` exists
/// (LocalBeat Studio's server song cache), reaching the end means "wait for
/// more", not "the song ends". An ordinary file reads as usual.
struct Growing {
    file: File,
    marker: PathBuf,
}

impl Growing {
    fn new(file: File, path: &Path) -> Self {
        let mut m = path.as_os_str().to_owned();
        m.push(".downloading");
        Growing { file, marker: PathBuf::from(m) }
    }

    fn growing(&self) -> bool {
        self.marker.exists()
    }
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = Instant::now();
        loop {
            let n = self.file.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            if !self.growing() {
                // Finished: the last piece may have landed just before the
                // marker went.
                return self.file.read(buf);
            }
            if start.elapsed() > GROW_WAIT {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the song stopped arriving"));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Seek for Growing {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

impl MediaSource for Growing {
    fn is_seekable(&self) -> bool {
        true
    }

    /// Unknown until it's all here.
    fn byte_len(&self) -> Option<u64> {
        if self.growing() { None } else { self.file.metadata().ok().map(|m| m.len()) }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes a 16-bit PCM WAV whose sample n (per channel) is `f(n)`.
    pub fn write_wav(path: &Path, rate: u32, channels: u16, frames: usize, f: impl Fn(usize) -> i16) {
        let mut data = Vec::with_capacity(frames * channels as usize * 2);
        for n in 0..frames {
            for _ in 0..channels {
                data.extend_from_slice(&f(n).to_le_bytes());
            }
        }
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        std::fs::write(path, b).unwrap();
    }

    pub fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("localbeat-player-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn decodes_mono_wav_as_stereo() {
        let p = tmp("mono.wav");
        write_wav(&p, 8000, 1, 1000, |n| n as i16);
        let mut s = Source::open(&p).unwrap();
        assert_eq!(s.info.codec, "PCM");
        assert_eq!(s.info.sample_rate, 8000);
        assert!((s.duration.unwrap() - 0.125).abs() < 1e-9);
        let mut out = Vec::new();
        while s.next(&mut out).unwrap() {}
        assert_eq!(out.len(), 2000);
        assert_eq!(out[2], out[3]);
        assert!((out[20] * 32768.0 - 10.0).abs() < 0.01);
    }

    #[test]
    fn seek_lands_on_the_exact_frame() {
        let p = tmp("seek.wav");
        write_wav(&p, 8000, 2, 24000, |n| (n % 30000) as i16);
        let mut s = Source::open(&p).unwrap();
        let at = s.seek(1.0).unwrap();
        assert!((at - 1.0).abs() < 1e-9);
        let mut out = Vec::new();
        assert!(s.next(&mut out).unwrap());
        assert!((out[0] * 32768.0 - 8000.0).abs() < 0.01, "first sample {}", out[0] * 32768.0);
    }

    #[test]
    fn a_song_still_arriving_plays_to_its_end() {
        let whole = tmp("whole.wav");
        write_wav(&whole, 8000, 2, 16000, |n| (n % 1000) as i16);
        let bytes = std::fs::read(&whole).unwrap();
        let p = tmp("arriving.wav");
        let marker = tmp("arriving.wav.downloading");
        std::fs::write(&marker, b"").unwrap();
        std::fs::write(&p, &bytes[..2000]).unwrap();
        let (p2, m2) = (p.clone(), marker.clone());
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::File::options().append(true).open(&p2).unwrap();
            for chunk in bytes[2000..].chunks(7000) {
                std::thread::sleep(Duration::from_millis(30));
                f.write_all(chunk).unwrap();
                f.flush().unwrap();
            }
            std::fs::remove_file(&m2).unwrap();
        });
        let mut s = Source::open(&p).unwrap();
        let mut out = Vec::new();
        while s.next(&mut out).unwrap() {}
        writer.join().unwrap();
        assert_eq!(out.len(), 32000, "every frame, none cut off at the first end of file");
        assert!((out[2 * 999] * 32768.0 - 999.0).abs() < 0.01);
    }

    #[test]
    fn drm_files_are_refused() {
        let p = tmp("song.m4p");
        std::fs::write(&p, b"x").unwrap();
        let e = Source::open(&p).err().unwrap().to_string();
        assert!(e.contains("DRM"));
    }
}
