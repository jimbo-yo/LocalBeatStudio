//! Reading one track from a disc: read-offset correction, chunked reads that
//! fall back to single sectors so a bad spot only costs the sectors that
//! really fail, and drive-cache flushing before re-reads.
//!
//! The drive itself sits behind [`SectorSource`], so this is tested with a
//! fake disc.

use std::ops::Range;

use crate::accuraterip::FRAMES_PER_SECTOR;

pub const SECTOR_BYTES: usize = 2352;

/// Sectors per read command (cd-da-reader caps one transfer at 64 KiB = 27
/// audio sectors).
pub const CHUNK_SECTORS: u32 = 26;

/// Something that returns raw CD audio sectors (2352 bytes each).
///
/// LBA 0 is the first sector after the 2-second pregap (the "normalized"
/// numbering in `cdtoc`).
pub trait SectorSource {
    fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String>;
}

#[derive(Debug, Default)]
pub struct TrackRead {
    /// Offset-corrected audio, one `u32` per stereo frame (see `accuraterip`).
    pub frames: Vec<u32>,
    /// Sectors (LBA) that failed every attempt; their audio is silence.
    pub unreadable: Vec<u32>,
    /// Frames outside the readable area (before LBA 0 or past the lead-out,
    /// reached because of the offset), filled with silence. Normal for the
    /// first/last track; AccurateRip doesn't count those frames.
    pub edge_fill: usize,
}

impl TrackRead {
    /// CRC32 of the audio bytes, as EAC/cyanrip report it ("Copy CRC"). Two
    /// reads with the same CRC read the same audio.
    pub fn crc32(&self) -> u32 {
        let mut h = crc32fast::Hasher::new();
        for f in &self.frames {
            h.update(&f.to_le_bytes());
        }
        h.finalize()
    }

    /// Raw little-endian PCM (for a WAV file or an encoder).
    pub fn pcm_bytes(&self) -> Vec<u8> {
        self.frames.iter().flat_map(|f| f.to_le_bytes()).collect()
    }
}

/// Reads the sectors `track` (LBA range from the TOC) with the drive's read
/// `offset` (in frames, e.g. +102) applied.
///
/// `readable_end`: first LBA that can't be read (the lead-out, or the end of
/// the audio session on a CD-Extra disc).
///
/// `progress(done, total)` is called after each chunk, in sectors.
pub fn read_track(
    src: &mut dyn SectorSource,
    track: Range<u32>,
    readable_end: u32,
    offset: i32,
    progress: &mut dyn FnMut(u32, u32),
) -> TrackRead {
    let fps = FRAMES_PER_SECTOR as i64;
    // A positive offset means the drive delivers audio early: the track's
    // audio sits `offset` frames later in what it returns.
    let want_start = track.start as i64 * fps + offset as i64;
    let want_end = track.end as i64 * fps + offset as i64;

    let first_sector = want_start.div_euclid(fps).max(0) as u32;
    let end_sector = ((want_end + fps - 1).div_euclid(fps)).clamp(0, readable_end as i64) as u32;

    let mut buf = Vec::with_capacity((end_sector.saturating_sub(first_sector)) as usize * SECTOR_BYTES);
    let mut unreadable = Vec::new();
    let total = end_sector.saturating_sub(first_sector);
    let mut lba = first_sector;
    while lba < end_sector {
        let count = CHUNK_SECTORS.min(end_sector - lba);
        match src.read_sectors(lba, count) {
            Ok(data) if data.len() == count as usize * SECTOR_BYTES => buf.extend_from_slice(&data),
            _ => {
                // Retry sector by sector so one bad spot doesn't lose the chunk.
                for s in lba..lba + count {
                    match src.read_sectors(s, 1) {
                        Ok(data) if data.len() == SECTOR_BYTES => buf.extend_from_slice(&data),
                        _ => {
                            buf.extend(std::iter::repeat_n(0u8, SECTOR_BYTES));
                            unreadable.push(s);
                        }
                    }
                }
            }
        }
        lba += count;
        progress(lba - first_sector, total);
    }

    // Cut the wanted frames out of what was read; silence outside it.
    let have_start = first_sector as i64 * fps;
    let have_end = end_sector as i64 * fps;
    let mut frames = Vec::with_capacity((want_end - want_start) as usize);
    let mut edge_fill = 0;
    for f in want_start..want_end {
        if f >= have_start && f < have_end {
            let i = (f - have_start) as usize * 4;
            frames.push(u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]));
        } else {
            frames.push(0);
            edge_fill += 1;
        }
    }
    // Only report bad sectors that hold this track's audio.
    unreadable.retain(|&s| {
        let (a, b) = (s as i64 * fps, (s as i64 + 1) * fps);
        a < want_end && b > want_start
    });

    TrackRead { frames, unreadable, edge_fill }
}

/// Reads `sectors` sectors far away from `target` and throws them away, so
/// the drive's cache no longer holds the target and a re-read really reads
/// the disc. Errors are ignored.
///
/// Drive caches are typically 0.5-2 MB (about 220-900 sectors); how much is
/// enough is per drive and still to be tested.
pub fn flush_cache(src: &mut dyn SectorSource, target: Range<u32>, readable_end: u32, sectors: u32) {
    let sectors = sectors.min(readable_end);
    // Pick the side of the disc farther from the target.
    let start = if target.start > readable_end - target.end {
        0
    } else {
        readable_end - sectors
    };
    let mut lba = start;
    while lba < start + sectors {
        let count = CHUNK_SECTORS.min(start + sectors - lba);
        let _ = src.read_sectors(lba, count);
        lba += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake disc: frame `n` holds the value `n + 1` (so silence = 0 stands out).
    struct FakeDisc {
        sectors: u32,
        bad: Vec<u32>,
        reads: Vec<(u32, u32)>,
    }

    impl FakeDisc {
        fn new(sectors: u32) -> Self {
            Self { sectors, bad: vec![], reads: vec![] }
        }
    }

    impl SectorSource for FakeDisc {
        fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
            self.reads.push((lba, count));
            if lba + count > self.sectors {
                return Err("past end".into());
            }
            if self.bad.iter().any(|b| (lba..lba + count).contains(b)) {
                return Err("bad sector".into());
            }
            let first = lba as usize * FRAMES_PER_SECTOR;
            let n = count as usize * FRAMES_PER_SECTOR;
            Ok((first..first + n).flat_map(|f| (f as u32 + 1).to_le_bytes()).collect())
        }
    }

    fn noop(_: u32, _: u32) {}

    #[test]
    fn zero_offset_reads_exact_track() {
        let mut d = FakeDisc::new(200);
        let r = read_track(&mut d, 10..70, 200, 0, &mut noop);
        assert_eq!(r.frames.len(), 60 * FRAMES_PER_SECTOR);
        assert_eq!(r.frames[0], 10 * 588 + 1);
        assert_eq!(*r.frames.last().unwrap(), 70 * 588);
        assert_eq!(r.edge_fill, 0);
        assert!(r.unreadable.is_empty());
    }

    #[test]
    fn positive_offset_shifts_and_fills_lead_out() {
        let mut d = FakeDisc::new(100);
        // Middle track: audio comes from 102 frames later.
        let r = read_track(&mut d, 10..20, 100, 102, &mut noop);
        assert_eq!(r.frames[0], 10 * 588 + 102 + 1);
        assert_eq!(r.frames.len(), 10 * 588);
        assert_eq!(r.edge_fill, 0);
        // Last track: the final 102 frames would be in the lead-out.
        let r = read_track(&mut d, 90..100, 100, 102, &mut noop);
        assert_eq!(r.edge_fill, 102);
        assert_eq!(r.frames[r.frames.len() - 103], 100 * 588);
        assert_eq!(*r.frames.last().unwrap(), 0);
    }

    #[test]
    fn negative_offset_fills_before_lba_zero() {
        let mut d = FakeDisc::new(100);
        let r = read_track(&mut d, 0..10, 100, -30, &mut noop);
        assert_eq!(r.edge_fill, 30);
        assert_eq!(r.frames[29], 0);
        assert_eq!(r.frames[30], 1);
    }

    #[test]
    fn bad_sector_costs_only_that_sector() {
        let mut d = FakeDisc::new(200);
        d.bad = vec![33];
        let r = read_track(&mut d, 10..70, 200, 0, &mut noop);
        assert_eq!(r.unreadable, vec![33]);
        let silent = r.frames.iter().filter(|&&f| f == 0).count();
        assert_eq!(silent, FRAMES_PER_SECTOR);
        // Sector 34 (right after) was still read.
        assert_eq!(r.frames[(34 - 10) * 588], 34 * 588 + 1);
    }

    #[test]
    fn bad_sector_outside_track_audio_not_reported() {
        // With +102 the read touches sector 20 (the next track) for its first
        // 102 frames; a failure later in sector 20 still affects us, but one
        // in sector 9 (before) is never read.
        let mut d = FakeDisc::new(100);
        d.bad = vec![9];
        let r = read_track(&mut d, 10..20, 100, 102, &mut noop);
        assert!(r.unreadable.is_empty());
    }

    #[test]
    fn progress_reaches_total() {
        let mut d = FakeDisc::new(200);
        let mut last = (0, 0);
        read_track(&mut d, 0..100, 200, 0, &mut |a, b| last = (a, b));
        assert_eq!(last, (100, 100));
    }

    #[test]
    fn crc_differs_on_different_audio() {
        let mut d = FakeDisc::new(200);
        let a = read_track(&mut d, 0..10, 200, 0, &mut noop);
        let b = read_track(&mut d, 0..10, 200, 0, &mut noop);
        let c = read_track(&mut d, 0..10, 200, 1, &mut noop);
        assert_eq!(a.crc32(), b.crc32());
        assert_ne!(a.crc32(), c.crc32());
    }

    #[test]
    fn flush_reads_far_side() {
        let mut d = FakeDisc::new(10_000);
        flush_cache(&mut d, 100..200, 10_000, 1000);
        assert_eq!(d.reads.first().unwrap().0, 9_000);
        d.reads.clear();
        flush_cache(&mut d, 9_000..9_500, 10_000, 1000);
        assert_eq!(d.reads.first().unwrap().0, 0);
    }
}
