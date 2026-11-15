//! AccurateRip: per-track checksums computed from audio, and lookups in the
//! AccurateRip database.
//!
//! Audio is passed as one `u32` per stereo frame, the way it comes off the
//! disc read as little-endian words: left sample in the low 16 bits, right in
//! the high 16 bits. Samples must already be corrected for the drive's read
//! offset.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use cdtoc::Toc;

/// Stereo frames per CD sector (2352 bytes / 4).
pub const FRAMES_PER_SECTOR: usize = 588;

/// AccurateRip ignores the first and last five sectors of a disc (drives
/// can't all read them), minus one frame at the start.
const SKIP_FRAMES: usize = 5 * FRAMES_PER_SECTOR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checksums {
    pub v1: u32,
    pub v2: u32,
}

/// Packs a stereo frame of signed 16-bit samples into the disc word layout.
pub fn pack_frame(left: i16, right: i16) -> u32 {
    (left as u16 as u32) | ((right as u16 as u32) << 16)
}

/// AccurateRip v1 and v2 checksums of one track.
///
/// `first`/`last`: whether this is the disc's first/last audio track, which
/// changes which frames are counted.
pub fn checksums(frames: &[u32], first: bool, last: bool) -> Checksums {
    let len = frames.len();
    // Multipliers are 1-based frame positions; same bounds as the reference
    // implementation used by EAC, whipper and cyanrip.
    let start = if first { SKIP_FRAMES - 1 } else { 0 };
    let end = if last { len.saturating_sub(SKIP_FRAMES) } else { len };

    let mut v1: u32 = 0;
    let mut v2: u32 = 0;
    for (i, &frame) in frames.iter().enumerate() {
        let mult = i + 1;
        if mult < start || mult > end {
            continue;
        }
        let product = frame as u64 * mult as u64;
        v1 = v1.wrapping_add(product as u32);
        v2 = v2.wrapping_add(product as u32).wrapping_add((product >> 32) as u32);
    }
    Checksums { v1, v2 }
}

/// Database entries for one disc: per track (index = track number - 1), the
/// known checksums and how many submitters matched each one.
#[derive(Debug, Clone, Default)]
pub struct DiscEntries {
    pub tracks: Vec<BTreeMap<u32, u8>>,
    /// Per track: the v1 checksum of the sector 450 sectors into the track,
    /// one per pressing in the database (-> confidence). Lets a read be
    /// lined up with a pressing whose audio sits a few hundred frames
    /// earlier or later (same TOC, so the same database entry).
    pub frame450: Vec<BTreeMap<u32, u8>>,
}

/// Sectors into a track of the sector AccurateRip's frame-450 checksum covers.
pub const FRAME450_SECTOR: usize = 450;

/// How far (in frames) a pressing may be shifted: ±10 sectors. Pressings seen
/// so far were ~390 and ~1250 frames off.
pub const MAX_PRESSING_SHIFT: i64 = 10 * FRAMES_PER_SECTOR as i64;

/// v1 checksum of one sector (588 frames), as in the database's frame-450 field.
fn sector_v1(frames: &[u32]) -> u32 {
    frames.iter().enumerate().fold(0u32, |acc, (j, &f)| acc.wrapping_add(f.wrapping_mul(j as u32 + 1)))
}

impl DiscEntries {
    /// Looks for a pressing of `track` whose audio is `shift` frames later
    /// (negative: earlier) than in `frames` (an offset-corrected read of the
    /// track): the frame-450 sector at that shift matches the database.
    /// Nearest shifts first; 0 is skipped (a plain mismatch). Only a lead:
    /// the whole track still has to match at that shift.
    /// Whether the frame-450 sector of `frames` (unshifted) matches a pressing.
    pub fn frame450_matches(&self, track: usize, frames: &[u32]) -> bool {
        let base = FRAME450_SECTOR * FRAMES_PER_SECTOR;
        let Some(known) = track.checked_sub(1).and_then(|i| self.frame450.get(i)) else { return false };
        frames.len() >= base + FRAMES_PER_SECTOR && {
            let sum = sector_v1(&frames[base..base + FRAMES_PER_SECTOR]);
            sum != 0 && known.contains_key(&sum)
        }
    }

    pub fn find_pressing_shift(&self, track: usize, frames: &[u32]) -> Option<i64> {
        let known = track.checked_sub(1).and_then(|i| self.frame450.get(i))?;
        if known.is_empty() {
            return None;
        }
        let base = (FRAME450_SECTOR * FRAMES_PER_SECTOR) as i64;
        for d in 1..=MAX_PRESSING_SHIFT {
            for shift in [d, -d] {
                let start = base + shift;
                let end = start + FRAMES_PER_SECTOR as i64;
                if start < 0 || end > frames.len() as i64 {
                    continue;
                }
                let sum = sector_v1(&frames[start as usize..end as usize]);
                if sum != 0 && known.contains_key(&sum) {
                    return Some(shift);
                }
            }
        }
        None
    }
}

/// Reads the frame-450 checksums from the database file: chunks of a header
/// (audio track count, disc ID 1, disc ID 2, CDDB ID, little-endian) and per
/// track (confidence u8, checksum u32, frame-450 checksum u32). Chunks for
/// another disc are skipped.
fn parse_frame450(bin: &[u8], ids: (u8, u32, u32, u32)) -> Vec<BTreeMap<u32, u8>> {
    let le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let n = ids.0 as usize;
    let mut out = vec![BTreeMap::new(); n];
    let mut pos = 0;
    while pos + 13 <= bin.len() {
        let count = bin[pos] as usize;
        let header = (bin[pos], le(&bin[pos + 1..]), le(&bin[pos + 5..]), le(&bin[pos + 9..]));
        pos += 13;
        let size = count * 9;
        if pos + size > bin.len() {
            break;
        }
        if header == ids {
            for t in 0..n.min(count) {
                let e = &bin[pos + t * 9..];
                let (confidence, crc450) = (e[0], le(&e[5..]));
                let c = out[t].entry(crc450).or_insert(0u8);
                *c = c.saturating_add(confidence);
            }
        }
        pos += size;
    }
    out
}

/// (track count, disc ID 1, disc ID 2, CDDB ID) from cdtoc's "013-001802ed-00f8ee31-b611560e".
fn ar_ids(toc: &Toc) -> Option<(u8, u32, u32, u32)> {
    let id = toc.accuraterip_id().to_string();
    let mut p = id.split('-');
    let n = p.next()?.parse::<u8>().ok()?;
    let h = |x: Option<&str>| x.and_then(|x| u32::from_str_radix(x, 16).ok());
    Some((n, h(p.next())?, h(p.next())?, h(p.next())?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Matches a database entry; `confidence` = number of matching submissions.
    Match { version: u8, confidence: u8 },
    /// The disc is in the database but this checksum isn't.
    NoMatch,
    /// The database has no entries for this track (or the disc).
    NotInDatabase,
}

impl DiscEntries {
    pub fn verdict(&self, track: usize, sums: Checksums) -> Verdict {
        let Some(known) = track.checked_sub(1).and_then(|i| self.tracks.get(i)) else {
            return Verdict::NotInDatabase;
        };
        if known.is_empty() {
            return Verdict::NotInDatabase;
        }
        if let Some(&c) = known.get(&sums.v2) {
            return Verdict::Match { version: 2, confidence: c };
        }
        if let Some(&c) = known.get(&sums.v1) {
            return Verdict::Match { version: 1, confidence: c };
        }
        Verdict::NoMatch
    }
}

/// Downloads the disc's AccurateRip entries. `Ok(None)` if the disc isn't in
/// the database.
pub fn fetch(toc: &Toc) -> Result<Option<DiscEntries>> {
    let url = toc.accuraterip_checksum_url();
    let mut resp = match crate::http::agent().get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("fetching {url}")),
    };
    let bin = resp.body_mut().read_to_vec().context("reading AccurateRip data")?;
    match toc.accuraterip_parse_checksums(&bin) {
        Ok(tracks) => {
            let frame450 = ar_ids(toc).map(|ids| parse_frame450(&bin, ids)).unwrap_or_default();
            Ok(Some(DiscEntries { tracks, frame450 }))
        }
        Err(cdtoc::TocError::NoChecksums) => Ok(None),
        Err(e) => Err(e).context("parsing AccurateRip data"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straightforward re-statement of the reference C code, for comparison.
    fn reference(frames: &[u32], first: bool, last: bool) -> Checksums {
        let count = frames.len() as u32;
        let check_start: u32 = if first { 5 * 588 - 1 } else { 0 };
        let check_end: u32 = if last { count - 5 * 588 } else { count };
        let (mut crc, mut crc_new) = (0u32, 0u32);
        let mut multiplier: u32 = 1;
        for &f in frames {
            if multiplier >= check_start && multiplier <= check_end {
                crc = crc.wrapping_add(multiplier.wrapping_mul(f));
                let calc = f as u64 * multiplier as u64;
                crc_new = crc_new.wrapping_add((calc & 0xFFFF_FFFF) as u32);
                crc_new = crc_new.wrapping_add((calc >> 32) as u32);
            }
            multiplier += 1;
        }
        Checksums { v1: crc, v2: crc_new }
    }

    fn noise(n: usize) -> Vec<u32> {
        // Deterministic pseudo-random frames (xorshift).
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect()
    }

    #[test]
    fn matches_reference_for_all_track_positions() {
        let frames = noise(FRAMES_PER_SECTOR * 40);
        for (first, last) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(checksums(&frames, first, last), reference(&frames, first, last));
        }
    }

    #[test]
    fn silence_is_zero() {
        let frames = vec![0u32; FRAMES_PER_SECTOR * 20];
        assert_eq!(checksums(&frames, true, true), Checksums { v1: 0, v2: 0 });
    }

    #[test]
    fn first_track_skips_leading_frames() {
        let mut frames = vec![0u32; FRAMES_PER_SECTOR * 20];
        // Frame at multiplier 2938 is skipped; 2939 is the first counted.
        frames[2937] = 1;
        assert_eq!(checksums(&frames, true, false).v1, 0);
        frames[2938] = 1;
        assert_eq!(checksums(&frames, true, false).v1, 2939);
    }

    #[test]
    fn frame450_parse_and_pressing_search() {
        // A database file with two chunks: ours (2 tracks) and another disc's.
        let ids = (2u8, 0x11u32, 0x22u32, 0x33u32);
        let frames = noise(FRAMES_PER_SECTOR * 470);
        let base = FRAME450_SECTOR * FRAMES_PER_SECTOR;
        // The database pressing's track 2 starts 390 frames later than our read.
        let crc450 = sector_v1(&frames[base + 390..base + 390 + FRAMES_PER_SECTOR]);
        let mut bin = Vec::new();
        for (hdr, crc) in [((2u8, 0x11u32, 0x22u32, 0x33u32), crc450), ((2, 0x99, 0x22, 0x33), 0xDEAD)] {
            bin.push(hdr.0);
            for v in [hdr.1, hdr.2, hdr.3] {
                bin.extend(v.to_le_bytes());
            }
            for c in [0xAAAA_AAAAu32, crc] {
                bin.push(7);
                bin.extend(0x1234u32.to_le_bytes());
                bin.extend(c.to_le_bytes());
            }
        }
        let f450 = parse_frame450(&bin, ids);
        assert_eq!(f450[1].get(&crc450), Some(&7));
        assert!(!f450[1].contains_key(&0xDEAD));
        let db = DiscEntries { tracks: vec![], frame450: f450 };
        assert_eq!(db.find_pressing_shift(2, &frames), Some(390));
        assert_eq!(db.find_pressing_shift(1, &frames), None);
    }

    #[test]
    fn pack_frame_layout() {
        assert_eq!(pack_frame(1, 0), 0x0000_0001);
        assert_eq!(pack_frame(0, 1), 0x0001_0000);
        assert_eq!(pack_frame(-1, -2), 0xFFFE_FFFF);
    }

    #[test]
    fn verdict_prefers_v2() {
        let mut m = BTreeMap::new();
        m.insert(10, 3);
        m.insert(20, 7);
        let db = DiscEntries { tracks: vec![m], ..Default::default() };
        assert_eq!(db.verdict(1, Checksums { v1: 10, v2: 20 }), Verdict::Match { version: 2, confidence: 7 });
        assert_eq!(db.verdict(1, Checksums { v1: 10, v2: 99 }), Verdict::Match { version: 1, confidence: 3 });
        assert_eq!(db.verdict(1, Checksums { v1: 1, v2: 2 }), Verdict::NoMatch);
        assert_eq!(db.verdict(2, Checksums { v1: 1, v2: 2 }), Verdict::NotInDatabase);
    }
}
