//! The CUETools database (CTDB): per-disc checksums and parity, for checking
//! tracks AccurateRip can't vouch for and (later) repairing damaged ones.
//! See docs/ctdb-repair.md. Reference: CUETools (LGPL),
//! CUETools.AccurateRip/AccurateRip.cs `CTDBCRC` and CDRepair.cs.
//!
//! Layout: the disc's audio as 16-bit words (L, R, L, R...) from the start
//! of track 1 to the lead-out, cut into rows of [`STRIDE`] words. The
//! database calls the stride "5880": that's stereo samples (10 sectors);
//! in 16-bit words it's 11760. The first row (lead-in) and the last row,
//! `laststride` words long, are left out of the checksums.

use anyhow::{Context, Result, anyhow};

/// Row length in 16-bit words (5880 stereo samples, 10 CD sectors).
pub const STRIDE: usize = 11760;

/// Row geometry for a disc of `samples` stereo samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub samples: u64,
    /// Words in the last row: one stride plus whatever doesn't fill a row.
    pub laststride: usize,
    /// Rows between the lead-in and lead-out rows (the parity's codeword length).
    pub stridecount: usize,
}

impl Layout {
    pub fn new(samples: u64) -> Self {
        let words = samples * 2;
        Layout {
            samples,
            laststride: STRIDE + (words % STRIDE as u64) as usize,
            stridecount: (words / STRIDE as u64).saturating_sub(2) as usize,
        }
    }

    /// Stereo samples left out at the start of track 1 and the end of the last track.
    pub fn prefix_samples(&self) -> usize {
        STRIDE / 2
    }
    pub fn suffix_samples(&self) -> usize {
        self.laststride / 2
    }
}

fn crc_frames(h: &mut crc32fast::Hasher, frames: &[u32]) {
    let mut buf = Vec::with_capacity(4096 * 4);
    for chunk in frames.chunks(4096) {
        buf.clear();
        for f in chunk {
            buf.extend_from_slice(&f.to_le_bytes());
        }
        h.update(&buf);
    }
}

/// CTDB's per-track CRC32s (`TrackCRC`): each track's audio, but track 1
/// without its first `stride/2` samples and the last track without its last
/// `laststride/2`. `tracks` = every track of the disc, in order, as stereo
/// frames (`u32`, little-endian L then R, as in `secure_read`).
pub fn track_crcs(tracks: &[&[u32]]) -> Vec<u32> {
    let samples: u64 = tracks.iter().map(|t| t.len() as u64).sum();
    let l = Layout::new(samples);
    let n = tracks.len();
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let a = if i == 0 { l.prefix_samples().min(t.len()) } else { 0 };
            let b = if i + 1 == n { t.len().saturating_sub(l.suffix_samples()) } else { t.len() };
            let mut h = crc32fast::Hasher::new();
            crc_frames(&mut h, &t[a..b.max(a)]);
            h.finalize()
        })
        .collect()
}

/// CTDB's whole-disc CRC32 (`CTDBCRC(0)`): all audio without the first
/// `stride/2` and the last `laststride/2` samples.
pub fn disc_crc(tracks: &[&[u32]]) -> u32 {
    let samples: u64 = tracks.iter().map(|t| t.len() as u64).sum();
    let l = Layout::new(samples);
    let (skip, keep) = (l.prefix_samples() as u64, samples.saturating_sub(l.suffix_samples() as u64));
    let mut h = crc32fast::Hasher::new();
    let mut pos = 0u64;
    for t in tracks {
        let (s, e) = (pos, pos + t.len() as u64);
        let a = skip.clamp(s, e) - s;
        let b = keep.clamp(s, e) - s;
        if b > a {
            crc_frames(&mut h, &t[a as usize..b as usize]);
        }
        pos = e;
    }
    h.finalize()
}

/// The TOC as CTDB wants it: track starts relative to track 1, then the
/// lead-out, in sectors, `:`-separated ("0:25655:...:233017").
pub fn toc_string(starts: &[u32], leadout: u32) -> String {
    let first = starts.first().copied().unwrap_or(0);
    starts.iter().chain(std::iter::once(&leadout)).map(|s| (s - first).to_string()).collect::<Vec<_>>().join(":")
}

/// TOC string for back-to-back tracks of these lengths (stereo samples):
/// a whole disc ripped by LocalBeat Studio, no data tracks.
pub fn toc_from_lengths(lengths: &[usize]) -> Result<String> {
    let mut starts = Vec::new();
    let mut pos = 0u32;
    for &n in lengths {
        if n % 588 != 0 {
            return Err(anyhow!("a track isn't a whole number of CD sectors: not a straight CD rip"));
        }
        starts.push(pos);
        pos += (n / 588) as u32;
    }
    Ok(toc_string(&starts, pos))
}

/// One pressing (submission group) in the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: u64,
    /// How many rips agreed.
    pub confidence: u32,
    /// Whole-disc CRC32 (see [`disc_crc`]).
    pub crc32: u32,
    /// Parity symbols per column (8 or 16): fixes up to npar/2 bad words per
    /// column, npar if their positions are known.
    pub npar: u32,
    /// Where its parity is, if it has any.
    pub parity_url: Option<String>,
    pub track_crcs: Vec<u32>,
}

/// Looks the TOC up (exact layout only).
pub fn lookup(toc: &str) -> Result<Vec<Entry>> {
    let url = format!("http://db.cuetools.net/lookup2.php?version=3&ctdb=1&fuzzy=0&metadata=none&toc={toc}");
    let body = crate::http::agent()
        .get(&url)
        .call()
        .context("CUETools database lookup")?
        .body_mut()
        .read_to_string()
        .context("reading the CUETools database's answer")?;
    Ok(parse_entries(&body))
}

/// The `<entry .../>` elements of a lookup2.php answer.
pub fn parse_entries(xml: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for part in xml.split("<entry ").skip(1) {
        let end = part.find('>').unwrap_or(part.len());
        let attrs = &part[..end];
        let get = |name: &str| -> Option<String> {
            let key = format!("{name}=\"");
            let i = attrs.find(&key)? + key.len();
            let j = attrs[i..].find('"')? + i;
            Some(attrs[i..j].to_string())
        };
        let hex = |s: &str| u32::from_str_radix(s.trim(), 16).ok();
        let Some(id) = get("id").and_then(|s| s.parse().ok()) else { continue };
        out.push(Entry {
            id,
            confidence: get("confidence").and_then(|s| s.parse().ok()).unwrap_or(0),
            crc32: get("crc32").as_deref().and_then(hex).unwrap_or(0),
            npar: get("npar").and_then(|s| s.parse().ok()).unwrap_or(0),
            parity_url: get("hasparity").filter(|s| !s.is_empty()),
            track_crcs: get("trackcrcs").map(|s| s.split_whitespace().filter_map(hex).collect()).unwrap_or_default(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_of_grace() {
        // Jeff Buckley, Grace: lead-out at 233017 sectors.
        let l = Layout::new(233017 * 588);
        assert_eq!(l.laststride, 19992);
        assert_eq!(l.prefix_samples(), 5880);
        assert_eq!(l.suffix_samples(), 9996);
    }

    #[test]
    fn edge_tracks_leave_out_the_lead_in_and_lead_out_rows() {
        // Three tracks of 3 sectors each: frame n holds n.
        let all: Vec<u32> = (0..9 * 588).collect();
        let t: Vec<&[u32]> = all.chunks(3 * 588).collect();
        let l = Layout::new(all.len() as u64);
        let crc = |r: &[u32]| {
            let mut h = crc32fast::Hasher::new();
            crc_frames(&mut h, r);
            h.finalize()
        };
        let got = track_crcs(&t);
        let skip = l.prefix_samples().min(t[0].len());
        assert_eq!(got[0], crc(&t[0][skip..]));
        assert_eq!(got[1], crc(t[1]));
        let keep = t[2].len().saturating_sub(l.suffix_samples());
        assert_eq!(got[2], crc(&t[2][..keep]));
        let disc_keep = all.len().saturating_sub(l.suffix_samples());
        assert_eq!(disc_crc(&t), crc(&all[l.prefix_samples().min(disc_keep)..disc_keep]));
    }

    #[test]
    fn toc_strings() {
        assert_eq!(toc_string(&[150, 1150], 3150), "0:1000:3000");
        assert_eq!(toc_from_lengths(&[588 * 1000, 588 * 2000]).unwrap(), "0:1000:3000");
        assert!(toc_from_lengths(&[100]).is_err());
    }

    #[test]
    fn parses_a_lookup_answer() {
        let xml = r#"<?xml version="1.0"?><ctdb><entry confidence="999" crc32="1521c397" hasparity="http://p.cuetools.net/11118" id="11118" npar="16" stride="5880" syndrome="AAA=" toc="0:1:2" trackcrcs="b6bf4480 22339c25"/><entry confidence="1" crc32="2" id="5" npar="16" trackcrcs=""/></ctdb>"#;
        let e = parse_entries(xml);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, 11118);
        assert_eq!(e[0].confidence, 999);
        assert_eq!(e[0].crc32, 0x1521c397);
        assert_eq!(e[0].npar, 16);
        assert_eq!(e[0].parity_url.as_deref(), Some("http://p.cuetools.net/11118"));
        assert_eq!(e[0].track_crcs, vec![0xb6bf4480, 0x22339c25]);
        assert_eq!(e[1].parity_url, None);
        assert!(e[1].track_crcs.is_empty());
    }
}
