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
        Ok(tracks) => Ok(Some(DiscEntries { tracks })),
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
        let db = DiscEntries { tracks: vec![m] };
        assert_eq!(db.verdict(1, Checksums { v1: 10, v2: 20 }), Verdict::Match { version: 2, confidence: 7 });
        assert_eq!(db.verdict(1, Checksums { v1: 10, v2: 99 }), Verdict::Match { version: 1, confidence: 3 });
        assert_eq!(db.verdict(1, Checksums { v1: 1, v2: 2 }), Verdict::NoMatch);
        assert_eq!(db.verdict(2, Checksums { v1: 1, v2: 2 }), Verdict::NotInDatabase);
    }
}
