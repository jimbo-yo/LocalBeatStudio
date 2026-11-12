//! Reading one track from a disc: read-offset correction, chunked reads that
//! fall back to single sectors so a bad spot only costs the sectors that
//! really fail, and drive-cache flushing before re-reads.
//!
//! The drive itself sits behind [`SectorSource`], so this is tested with a
//! fake disc.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::accuraterip::FRAMES_PER_SECTOR;

pub const SECTOR_BYTES: usize = 2352;

/// Sectors per read command (cd-da-reader caps one transfer at 64 KiB = 27
/// audio sectors).
pub const CHUNK_SECTORS: u32 = 26;

/// How long a damaged spot may hold up one read of a track. A failed read
/// command that took `slow` or longer is a stall (on Linux a read the drive
/// can't finish takes the 30 s SCSI timeout, then a USB reset). After the
/// first stalled sector the rest of its chunk is marked unreadable untried,
/// and once stalls add up to `budget` the rest of the track is too.
#[derive(Debug, Clone, Copy)]
pub struct StallLimits {
    /// A failed read command that took this long is a stall.
    pub slow: Duration,
    /// Stalls may cost this much per read; then the rest of the track is
    /// marked unreadable untried.
    pub budget: Duration,
    /// After a stall, this many sectors are skipped (marked unreadable,
    /// not tried) before reading again; doubled on each further stall up
    /// to `skip_max`, back to `skip_first` after a chunk reads well.
    pub skip_first: u32,
    pub skip_max: u32,
}

impl Default for StallLimits {
    fn default() -> Self {
        // A stall costs a 30 s timeout: 150 s is about five of them.
        Self { slow: Duration::from_secs(10), budget: Duration::from_secs(150), skip_first: 25, skip_max: 750 }
    }
}

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
    /// The read stopped trying after too many stalls ([`StallLimits`]); the
    /// rest of the track is in `unreadable`.
    pub gave_up: bool,
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
    read_track_limited(src, track, readable_end, offset, StallLimits::default(), progress)
}

/// [`read_track`] with explicit [`StallLimits`].
pub fn read_track_limited(
    src: &mut dyn SectorSource,
    track: Range<u32>,
    readable_end: u32,
    offset: i32,
    limits: StallLimits,
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
    let mut stalled = Duration::ZERO;
    let mut gave_up = false;
    // Sectors still to skip after a stall, and the next skip's length.
    let mut skip_ahead: u32 = 0;
    let mut next_skip = limits.skip_first.max(1);
    let lost = |buf: &mut Vec<u8>, unreadable: &mut Vec<u32>, s: u32| {
        buf.extend(std::iter::repeat_n(0u8, SECTOR_BYTES));
        unreadable.push(s);
    };
    while lba < end_sector {
        let count = CHUNK_SECTORS.min(end_sector - lba);
        // Past a stall: skip ahead (or, budget spent, everything) untried.
        if gave_up || skip_ahead > 0 {
            let n = if gave_up { count } else { count.min(skip_ahead) };
            for s in lba..lba + n {
                lost(&mut buf, &mut unreadable, s);
            }
            if !gave_up {
                skip_ahead -= n;
            }
            lba += n;
            progress(lba - first_sector, total);
            continue;
        }
        let t = Instant::now();
        match src.read_sectors(lba, count) {
            Ok(data) if data.len() == count as usize * SECTOR_BYTES => {
                buf.extend_from_slice(&data);
                next_skip = limits.skip_first.max(1);
            }
            _ => {
                let mut stall = false;
                if t.elapsed() >= limits.slow {
                    // The chunk stalled: trying its sectors one by one
                    // would stall again on each. Give it up and skip on.
                    stalled += t.elapsed();
                    stall = true;
                    for s in lba..lba + count {
                        lost(&mut buf, &mut unreadable, s);
                    }
                } else {
                    // A quick failure: retry sector by sector so one bad
                    // spot doesn't lose the chunk (stopping at a stall).
                    for s in lba..lba + count {
                        if stall {
                            lost(&mut buf, &mut unreadable, s);
                            continue;
                        }
                        let t = Instant::now();
                        match src.read_sectors(s, 1) {
                            Ok(data) if data.len() == SECTOR_BYTES => buf.extend_from_slice(&data),
                            _ => {
                                lost(&mut buf, &mut unreadable, s);
                                if t.elapsed() >= limits.slow {
                                    stalled += t.elapsed();
                                    stall = true;
                                }
                            }
                        }
                    }
                }
                if stall {
                    skip_ahead = next_skip;
                    next_skip = (next_skip * 2).min(limits.skip_max.max(1));
                    if stalled >= limits.budget {
                        gave_up = true;
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

    TrackRead { frames, unreadable, edge_fill, gave_up }
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

    fn noop(_: u32, _: u32) {}

    /// Every failure counts as a stall (the fake fails instantly).
    fn stall_on_any_error(budget: Duration) -> StallLimits {
        StallLimits { slow: Duration::ZERO, budget, skip_first: 26, skip_max: 104 }
    }

    #[test]
    fn a_stall_skips_a_short_stretch_and_reading_goes_on() {
        let mut d = FakeDisc::new(400);
        d.bad = vec![33];
        let lim = stall_on_any_error(Duration::from_secs(3600));
        let r = read_track_limited(&mut d, 26..200, 400, 0, lim, &mut noop);
        // The stalled chunk (26..52) and one skip (52..78) are lost, untried.
        assert_eq!(r.unreadable, (26..78).collect::<Vec<_>>());
        assert!(!d.reads.iter().any(|&(lba, n)| n == 1 || (lba >= 52 && lba < 78)), "{:?}", d.reads);
        // Reading resumed after the skip.
        assert_eq!(r.frames[(78 - 26) * 588], 78 * 588 + 1);
        assert_eq!(*r.frames.last().unwrap(), 200 * 588);
        assert!(!r.gave_up);
    }

    #[test]
    fn skips_grow_on_repeated_stalls_and_reset_after_good_reads() {
        let mut d = FakeDisc::new(1000);
        // Stalls at 30 and again right after the first skip (at 82), then
        // clean until a later stall at 500.
        d.bad = vec![30, 82, 500];
        let lim = stall_on_any_error(Duration::from_secs(3600));
        let r = read_track_limited(&mut d, 26..700, 1000, 0, lim, &mut noop);
        let u = &r.unreadable;
        // 26..52 stalled, skip 26 -> 78; chunk 78..104 stalls, skip 52 -> 156.
        assert!(u.contains(&26) && u.contains(&155) && !u.contains(&156), "{u:?}");
        // Later: the chunk holding 500 (494..520) stalls, skip back to 26 -> 546.
        assert!(u.contains(&494) && u.contains(&545) && !u.contains(&546), "{u:?}");
        assert!(!r.gave_up);
    }

    #[test]
    fn stall_budget_gives_up_rest_of_track() {
        let mut d = FakeDisc::new(200);
        d.bad = vec![30];
        let r = read_track_limited(&mut d, 26..130, 200, 0, stall_on_any_error(Duration::ZERO), &mut noop);
        assert!(r.gave_up);
        assert_eq!(r.unreadable, (26..130).collect::<Vec<_>>());
        // Nothing after the stalled chunk was asked of the drive.
        assert!(d.reads.iter().all(|&(lba, _)| lba < 52));
        assert_eq!(r.frames.len(), 104 * 588);
    }

    #[test]
    fn quick_failures_are_not_stalls() {
        // Default limits: an instant error is a plain bad sector.
        let mut d = FakeDisc::new(200);
        d.bad = vec![30, 40];
        let r = read_track(&mut d, 26..78, 200, 0, &mut noop);
        assert_eq!(r.unreadable, vec![30, 40]);
        assert!(!r.gave_up);
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
