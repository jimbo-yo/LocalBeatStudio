//! Sector voting for damaged discs: when no two whole reads of a track agree
//! (a scratch makes the drive return slightly different audio each time),
//! most of the track usually still reads the same every time. Comparing the
//! reads sector by sector and taking, for each sector that differs, the
//! version most reads agree on often rebuilds the true audio, which is then
//! checked against AccurateRip (never accepted on the vote alone).

use crate::accuraterip::FRAMES_PER_SECTOR;

/// The result of voting over several reads of the same track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    /// The voted audio (first read's version where no majority was found).
    pub frames: Vec<u32>,
    /// Sectors (588-frame chunks of the track) where the reads disagree.
    pub differing: Vec<usize>,
    /// Of those, the ones with no clear majority (no version seen at least
    /// twice and more often than any other).
    pub unresolved: Vec<usize>,
}

/// Votes per sector over `reads` (all the same length).
pub fn vote(reads: &[Vec<u32>]) -> Vote {
    let Some(first) = reads.first() else {
        return Vote { frames: Vec::new(), differing: Vec::new(), unresolved: Vec::new() };
    };
    let len = first.len();
    let mut frames = Vec::with_capacity(len);
    let (mut differing, mut unresolved) = (Vec::new(), Vec::new());
    for (i, start) in (0..len).step_by(FRAMES_PER_SECTOR).enumerate() {
        let end = (start + FRAMES_PER_SECTOR).min(len);
        let mine = &first[start..end];
        if reads.iter().all(|r| &r[start..end] == mine) {
            frames.extend_from_slice(mine);
            continue;
        }
        differing.push(i);
        // Group identical versions: (version, how many reads have it).
        let mut groups: Vec<(&[u32], usize)> = Vec::new();
        for r in reads {
            let c = &r[start..end];
            match groups.iter_mut().find(|(v, _)| *v == c) {
                Some(g) => g.1 += 1,
                None => groups.push((c, 1)),
            }
        }
        groups.sort_by(|a, b| b.1.cmp(&a.1));
        let clear = groups[0].1 >= 2 && groups.get(1).is_none_or(|g| g.1 < groups[0].1);
        if clear {
            frames.extend_from_slice(groups[0].0);
        } else {
            unresolved.push(i);
            frames.extend_from_slice(mine);
        }
    }
    Vote { frames, differing, unresolved }
}

/// "around 3:12-3:15" style description of where `sectors` lie in a track
/// (runs closer than 2 s merged), for the log.
pub fn describe(sectors: &[usize]) -> String {
    let secs = |s: usize| s as f64 * FRAMES_PER_SECTOR as f64 / 44_100.0;
    let fmt = |t: f64| format!("{}:{:04.1}", (t / 60.0) as u32, t % 60.0);
    let mut runs: Vec<(f64, f64)> = Vec::new();
    for &s in sectors {
        let (a, b) = (secs(s), secs(s + 1));
        match runs.last_mut() {
            Some(last) if a - last.1 < 2.0 => last.1 = b,
            _ => runs.push((a, b)),
        }
    }
    let parts: Vec<String> = runs.iter().take(6).map(|(a, b)| format!("{}–{}", fmt(*a), fmt(*b))).collect();
    let more = if runs.len() > 6 { format!(" and {} more places", runs.len() - 6) } else { String::new() };
    format!("{} sectors at {}{more}", sectors.len(), parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(n_sectors: usize) -> Vec<u32> {
        (0..(n_sectors * FRAMES_PER_SECTOR) as u32).map(|i| i.wrapping_mul(2_654_435_761)).collect()
    }

    #[test]
    fn majority_per_sector_rebuilds_the_track() {
        let good = track(10);
        // Each read is wrong in a different sector (plus read 3 also in sector 7).
        let mut reads = vec![good.clone(), good.clone(), good.clone()];
        reads[0][2 * 588 + 5] ^= 1;
        reads[1][5 * 588] ^= 1;
        reads[2][7 * 588 + 100] ^= 1;
        let v = vote(&reads);
        assert_eq!(v.frames, good);
        assert_eq!(v.differing, [2, 5, 7]);
        assert!(v.unresolved.is_empty());
    }

    #[test]
    fn no_majority_is_unresolved() {
        let good = track(4);
        let mut reads = vec![good.clone(), good.clone()];
        reads[1][588] ^= 1; // two reads, two versions of sector 1
        let v = vote(&reads);
        assert_eq!((v.differing.clone(), v.unresolved.clone()), (vec![1], vec![1]));
        assert_eq!(v.frames, reads[0]);
    }

    #[test]
    fn describes_places() {
        // 75 sectors = 1 s. Sectors 75..150 (1-2 s) and 4500 (60 s).
        let mut s: Vec<usize> = (75..150).collect();
        s.push(4500);
        assert_eq!(describe(&s), "76 sectors at 0:01.0–0:02.0, 1:00.0–1:00.0");
    }
}
