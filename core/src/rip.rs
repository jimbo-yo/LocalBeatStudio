//! Ripping one disc: plan what's missing from the library, then read, verify,
//! encode and file each missing track.
//!
//! Rules (same as the cyanrip version of rip-cd.sh):
//! - a track is kept if a read matches AccurateRip, or once a clean read
//!   (no unreadable sectors) gives the same CRC as an earlier clean read;
//! - after `Settings::reads` reads without that, the last read is saved to
//!   the rejected folder instead;
//! - nothing in the library is ever overwritten, so running again on the same
//!   disc only fills in what's missing.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cdtoc::Toc;

use crate::accuraterip::{self, DiscEntries, Verdict};
use crate::vote;
use crate::config::Settings;
use crate::cover;
use crate::flac_encode::{self, Picture};
use crate::musicbrainz::{self, Release};
use crate::naming;
use crate::secure_read::{self, SectorSource, TrackRead};

/// Where the disc's names come from.
pub enum Metadata<'a> {
    Release { release: &'a Release, genres: &'a [String] },
    /// MusicBrainz doesn't know the disc: "Unknown Artist", the given album
    /// name (dated by the caller, so two unknown discs don't collide), "Track N".
    Unknown { album: String },
}

pub struct TrackPlan {
    pub position: usize,
    pub sectors: Range<u32>,
    pub first: bool,
    pub last: bool,
    pub dest: PathBuf,
    pub tags: Vec<(String, String)>,
    /// Already in the library: not ripped.
    pub exists: bool,
}

pub struct DiscPlan {
    pub album_dir: PathBuf,
    /// "Artist - Album" (+ " CD2" on multi-disc albums), for logs and messages.
    pub label: String,
    /// " CD2" on multi-disc albums, else empty (log file names).
    pub disc_suffix: String,
    pub readable_end: u32,
    pub tracks: Vec<TrackPlan>,
}

impl DiscPlan {
    pub fn missing(&self) -> impl Iterator<Item = &TrackPlan> {
        self.tracks.iter().filter(|t| !t.exists)
    }
}

pub fn plan(toc: &Toc, meta: &Metadata, flac_dir: &Path) -> DiscPlan {
    let (artist, album, disc) = match meta {
        Metadata::Release { release: r, .. } => {
            (r.artist.clone(), r.title.clone(), (r.media_count > 1).then_some(r.medium.position))
        }
        Metadata::Unknown { album } => ("Unknown Artist".to_string(), album.clone(), None),
    };
    let album_dir = flac_dir.join(naming::safe_component(&artist)).join(naming::safe_component(&album));
    let disc_suffix = disc.map(|d| format!(" CD{d}")).unwrap_or_default();

    let tracks = toc
        .audio_tracks()
        .map(|t| {
            let n = t.number() as usize;
            let (track_artist, title, tags) = match meta {
                Metadata::Release { release: r, genres } => {
                    let tm = r.medium.tracks.iter().find(|x| x.position == n);
                    (
                        tm.map(|x| x.artist.clone()).unwrap_or_else(|| r.artist.clone()),
                        tm.map(|x| x.title.clone()).unwrap_or_else(|| format!("Track {n}")),
                        musicbrainz::track_tags(r, n, genres, toc),
                    )
                }
                Metadata::Unknown { album } => {
                    let title = format!("Track {n}");
                    let tags = vec![
                        ("TITLE".into(), title.clone()),
                        ("ARTIST".into(), "Unknown Artist".into()),
                        ("ALBUM".into(), album.clone()),
                        ("ALBUMARTIST".into(), "Unknown Artist".into()),
                        ("TRACKNUMBER".into(), n.to_string()),
                        ("TRACKTOTAL".into(), toc.audio_len().to_string()),
                        ("MUSICBRAINZ_DISCID".into(), toc.musicbrainz_id().to_string()),
                        ("CDTOC".into(), toc.to_string()),
                    ];
                    ("Unknown Artist".to_string(), title, tags)
                }
            };
            let dest = album_dir.join(format!("{}.flac", naming::track_stem(disc, n, &track_artist, &title)));
            let pos = t.position();
            TrackPlan {
                position: n,
                sectors: t.sector_range_normalized(),
                first: pos.is_first(),
                last: pos.is_last(),
                exists: flac_encode::present(&dest),
                dest,
                tags,
            }
        })
        .collect();

    DiscPlan {
        label: format!("{artist} - {album}{disc_suffix}"),
        album_dir,
        disc_suffix,
        readable_end: toc.audio_leadout_normalized(),
        tracks,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    AccurateRip { confidence: u8 },
    /// Matches AccurateRip for a pressing whose audio sits `shift` frames
    /// later (negative: earlier) than this disc's; the audio is the same.
    OtherPressing { confidence: u8, shift: i64 },
    /// Two clean reads with the same CRC (`reads` = reads it took).
    Repeat { reads: u32 },
    /// No two reads agreed, but voting sector by sector over `reads` reads
    /// (`sectors` of them differing) rebuilt audio that matches AccurateRip.
    Voted { confidence: u8, reads: u32, sectors: usize },
    /// Like `Repeat`, but AccurateRip has this pressing (its frame-450 sector
    /// matches) and the track still doesn't match: the audio differs
    /// somewhere, most likely a damaged spot read wrong the same way each
    /// time. Kept, but worth cleaning the disc and re-ripping.
    RepeatDiffers { reads: u32 },
}

pub enum Event<'a> {
    Skipped { position: usize, path: &'a Path },
    ReadStart { position: usize, read: u32 },
    Progress { position: usize, read: u32, done: u32, total: u32 },
    ReadDone { position: usize, read: u32, crc: u32, verdict: Verdict, unreadable: usize, repeat: bool },
    Accepted { position: usize, path: &'a Path, how: Acceptance },
    Rejected { position: usize, path: Option<&'a Path>, reads: u32 },
    Note(String),
}

#[derive(Debug, Default)]
pub struct Summary {
    pub accepted: Vec<usize>,
    pub rejected: Vec<usize>,
    pub skipped: Vec<usize>,
    /// Text for the album's rip log.
    pub log: String,
}

pub struct RipContext<'a> {
    pub settings: &'a Settings,
    pub offset: i32,
    pub accuraterip: Option<&'a DiscEntries>,
    pub cover: Option<&'a Picture>,
    /// First lines of the log (date, drive, release...), written by the caller.
    pub log_header: String,
    /// The CUETools database has repair data for this disc: a track that
    /// can't be verified gets the extra reads anyway (the repair votes over
    /// every read, however different they are).
    pub ctdb_parity: bool,
}

/// Reads added for sector voting when a damaged track runs out of reads.
pub const EXTRA_VOTE_READS: u32 = 4;

/// Votes over the clean reads; the vote only counts if every differing sector
/// has a clear majority and the result matches AccurateRip.
fn try_vote(reads: &[Vec<u32>], t: &TrackPlan, db: &DiscEntries) -> Result<(u8, vote::Vote), String> {
    let v = vote::vote(reads);
    if v.differing.is_empty() {
        return Err("the reads don't differ".into());
    }
    if !v.unresolved.is_empty() {
        return Err(format!("no majority in {} of the {} differing sectors", v.unresolved.len(), v.differing.len()));
    }
    match db.verdict(t.position, accuraterip::checksums(&v.frames, t.first, t.last)) {
        Verdict::Match { confidence, .. } => Ok((confidence, v)),
        _ => Err(format!(
            "every differing sector had a majority ({} only sample by sample), but the result doesn't match AccurateRip",
            v.by_sample
        )),
    }
}

/// One read of a track and its unreadable stretches (frames, track-relative).
type KeptRead = (Vec<u32>, Vec<(usize, usize)>);

/// Saves the clean reads of a track that couldn't be verified, as raw PCM
/// (16-bit stereo, 44.1 kHz), for the CUETools repair. Reads from earlier
/// rips of the disc are kept: new ones take the next free numbers, so a
/// re-rip after cleaning adds to them and the repair votes over all.
/// A read with unreadable sectors also gets `readK.gaps`: one "start end"
/// line (frames, track-relative, end exclusive) per unreadable stretch.
fn keep_reads(dir: &Path, t: &TrackPlan, reads: &[KeptRead]) -> Result<PathBuf> {
    let dir = dir.join(format!("track{:02}", t.position));
    std::fs::create_dir_all(&dir)?;
    let mut next = 1;
    for (r, gaps) in reads {
        while dir.join(format!("read{next}.raw")).exists() {
            next += 1;
        }
        let bytes: Vec<u8> = r.iter().flat_map(|f| f.to_le_bytes()).collect();
        std::fs::write(dir.join(format!("read{next}.raw")), bytes)?;
        if !gaps.is_empty() {
            let text: String = gaps.iter().map(|(a, b)| format!("{a} {b}\n")).collect();
            std::fs::write(dir.join(format!("read{next}.gaps")), text)?;
        }
    }
    Ok(dir)
}

/// A read's unreadable sectors as frame ranges of the track (merged).
fn gaps_of(r: &TrackRead, t: &TrackPlan, offset: i32) -> Vec<(usize, usize)> {
    let fps = accuraterip::FRAMES_PER_SECTOR as i64;
    let start = t.sectors.start as i64 * fps + offset as i64;
    let len = r.frames.len() as i64;
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &s in &r.unreadable {
        let a = (s as i64 * fps - start).clamp(0, len) as usize;
        let b = ((s as i64 + 1) * fps - start).clamp(0, len) as usize;
        if a >= b {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 >= a => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// A clean read that matched nothing: is it a pressing the database has at
/// another position? The frame-450 sector points at a shift; then the few
/// sectors past the track's edge are read and the whole track is checked at
/// that shift. (confidence, shift) on a full match.
fn check_other_pressing(
    src: &mut dyn SectorSource,
    t: &TrackPlan,
    plan: &DiscPlan,
    offset: i32,
    frames: &[u32],
    db: &DiscEntries,
) -> Option<(u8, i64)> {
    let shift = db.find_pressing_shift(t.position, frames)?;
    let n = shift.unsigned_abs() as usize;
    let fps = accuraterip::FRAMES_PER_SECTOR;
    let sectors = (n / fps + 2) as u32;
    let combined: Vec<u32> = if shift > 0 {
        let edge = t.sectors.end..(t.sectors.end + sectors).min(plan.readable_end.max(t.sectors.end));
        let after = secure_read::read_track(src, edge, plan.readable_end, offset, &mut |_, _| {});
        if !after.unreadable.is_empty() {
            return None;
        }
        let mut tail = after.frames;
        tail.resize(n.max(tail.len()), 0); // past the lead-out: silence (not counted on the last track)
        frames[n..].iter().chain(&tail[..n]).copied().collect()
    } else {
        let start = t.sectors.start.saturating_sub(sectors);
        let before = secure_read::read_track(src, start..t.sectors.start, plan.readable_end, offset, &mut |_, _| {});
        if !before.unreadable.is_empty() {
            return None;
        }
        let mut head = vec![0u32; n.saturating_sub(before.frames.len())];
        head.extend_from_slice(&before.frames[before.frames.len().saturating_sub(n)..]);
        head.iter().chain(&frames[..frames.len() - n]).copied().collect()
    };
    match db.verdict(t.position, accuraterip::checksums(&combined, t.first, t.last)) {
        Verdict::Match { confidence, .. } => Some((confidence, shift)),
        _ => None,
    }
}

pub fn rip(
    src: &mut dyn SectorSource,
    plan: &DiscPlan,
    ctx: &RipContext,
    on: &mut dyn FnMut(Event),
) -> Result<Summary> {
    let s = ctx.settings;
    let mut sum = Summary { log: ctx.log_header.clone(), ..Default::default() };
    let _ = writeln!(sum.log, "Read offset: {:+}, reads per track: up to {}", ctx.offset, s.reads);

    for t in &plan.tracks {
        if t.exists {
            on(Event::Skipped { position: t.position, path: &t.dest });
            sum.skipped.push(t.position);
            let _ = writeln!(sum.log, "Track {:>2}: already in the library", t.position);
            continue;
        }

        let mut clean_crcs: Vec<u32> = Vec::new();
        let mut accepted = None;
        let mut last: Option<TrackRead> = None;
        let mut reads_done = 0;
        // Clean reads kept for sector voting, and the audio a vote produced.
        let mut clean_reads: Vec<Vec<u32>> = Vec::new();
        // Every read, with its unreadable stretches (frames, track-relative),
        // for the CUETools repair: gaps are known erasures there.
        let mut all_reads: Vec<KeptRead> = Vec::new();
        let mut voted: Option<Vec<u32>> = None;

        let mut limit = s.reads.max(1);
        let mut read = 0;
        let mut gave_up = 0;
        while read < limit {
            read += 1;
            if read > 1 && s.flush_sectors > 0 {
                secure_read::flush_cache(src, t.sectors.clone(), plan.readable_end, s.flush_sectors);
            }
            on(Event::ReadStart { position: t.position, read });
            let r = secure_read::read_track(src, t.sectors.clone(), plan.readable_end, ctx.offset, &mut |done, total| {
                on(Event::Progress { position: t.position, read, done, total })
            });
            reads_done = read;
            let sums = accuraterip::checksums(&r.frames, t.first, t.last);
            let verdict = ctx.accuraterip.map(|e| e.verdict(t.position, sums)).unwrap_or(Verdict::NotInDatabase);
            let crc = r.crc32();
            let clean = r.unreadable.is_empty();
            // Only clean reads count: two reads that both failed on the same
            // sectors would agree on the silence.
            let repeat = clean && clean_crcs.contains(&crc);
            on(Event::ReadDone { position: t.position, read, crc, verdict, unreadable: r.unreadable.len(), repeat });
            let _ = writeln!(
                sum.log,
                "Track {:>2} read {read}: CRC {crc:08X}, AR v1 {:08X} v2 {:08X}, {}{}",
                t.position,
                sums.v1,
                sums.v2,
                match verdict {
                    Verdict::Match { version, confidence } => format!("AccurateRip v{version} match (confidence {confidence})"),
                    Verdict::NoMatch => "no AccurateRip match".into(),
                    Verdict::NotInDatabase => "not in AccurateRip".into(),
                },
                if clean { String::new() } else { format!(", {} unreadable sectors", r.unreadable.len()) }
            );
            // Kept for the sector vote (AccurateRip discs) and the CUETools
            // repair (any disc).
            if clean {
                clean_reads.push(r.frames.clone());
            }
            all_reads.push((r.frames.clone(), gaps_of(&r, t, ctx.offset)));
            if clean {
                if let Verdict::Match { confidence, .. } = verdict {
                    accepted = Some(Acceptance::AccurateRip { confidence });
                } else if let Some((confidence, shift)) = (verdict == Verdict::NoMatch)
                    .then(|| ctx.accuraterip.and_then(|db| check_other_pressing(src, t, plan, ctx.offset, &r.frames, db)))
                    .flatten()
                {
                    let _ = writeln!(sum.log, "Track {:>2} read {read}: matches AccurateRip for another pressing (shifted {shift:+} frames, confidence {confidence})", t.position);
                    on(Event::Note(format!("track {}: matches AccurateRip for another pressing (shifted {shift:+} frames)", t.position)));
                    accepted = Some(Acceptance::OtherPressing { confidence, shift });
                } else if let Some((confidence, v)) = (clean_reads.len() >= 3 && !repeat)
                    .then_some(ctx.accuraterip)
                    .flatten()
                    .and_then(|db| match try_vote(&clean_reads, t, db) {
                        Ok(x) => Some(x),
                        Err(why) => {
                            let _ = writeln!(sum.log, "Track {:>2}: vote over {} reads: {why}", t.position, clean_reads.len());
                            None
                        }
                    })
                {
                    let _ = writeln!(
                        sum.log,
                        "Track {:>2}: sector vote over {} reads fixed {} — matches AccurateRip (confidence {confidence})",
                        t.position,
                        clean_reads.len(),
                        vote::describe(&v.differing)
                    );
                    on(Event::Note(format!("track {}: rebuilt by sector vote ({}), AccurateRip match", t.position, vote::describe(&v.differing))));
                    accepted = Some(Acceptance::Voted { confidence, reads: read, sectors: v.differing.len() });
                    voted = Some(v.frames);
                } else if repeat {
                    let right_pressing = verdict == Verdict::NoMatch && ctx.accuraterip.is_some_and(|db| db.frame450_matches(t.position, &r.frames));
                    accepted = Some(if right_pressing { Acceptance::RepeatDiffers { reads: read } } else { Acceptance::Repeat { reads: read } });
                }
                clean_crcs.push(crc);
            }
            if r.gave_up {
                gave_up += 1;
                let _ = writeln!(sum.log, "Track {:>2} read {read}: the drive stalled on a damaged spot; rest of the read skipped", t.position);
            }
            last = Some(r);
            if accepted.is_some() {
                break;
            }
            // A spot the drive can't get through twice: give up on the track
            // so the rest of the disc gets ripped.
            if gave_up >= 2 {
                let _ = writeln!(sum.log, "Track {:>2}: the drive stalled on the same damage twice; giving up on this track", t.position);
                on(Event::Note(format!(
                    "track {}: the drive keeps stalling on a damaged spot; skipping the track (try another drive later)",
                    t.position
                )));
                break;
            }
            // Out of reads, but the reads differ only in a few places: a
            // few more reads give the vote more to go on.
            if read == limit && limit == s.reads.max(1) && clean_reads.len() >= 2 {
                let v = vote::vote(&clean_reads);
                let sectors = clean_reads[0].len().div_ceil(accuraterip::FRAMES_PER_SECTOR);
                if !v.differing.is_empty() && v.differing.len() * 20 <= sectors {
                    let _ = writeln!(sum.log, "Track {:>2}: reads differ in {}; reading up to {EXTRA_VOTE_READS} more times to vote", t.position, vote::describe(&v.differing));
                    on(Event::Note(format!(
                        "track {}: reads differ in {}; up to {EXTRA_VOTE_READS} more reads to vote on",
                        t.position,
                        vote::describe(&v.differing)
                    )));
                    limit += EXTRA_VOTE_READS;
                }
            }
            // Still out of reads: if the CUETools repair can have a go, more
            // reads give it more to vote on, however much they differ.
            if read == limit && limit == s.reads.max(1) && ctx.ctdb_parity {
                let _ = writeln!(sum.log, "Track {:>2}: {EXTRA_VOTE_READS} more reads for the CUETools repair", t.position);
                on(Event::Note(format!("track {}: up to {EXTRA_VOTE_READS} more reads for the CUETools repair", t.position)));
                limit += EXTRA_VOTE_READS;
            }
        }
        let mut r = last.expect("at least one read");
        if let Some(frames) = voted {
            r = TrackRead { frames, unreadable: Vec::new(), edge_fill: r.edge_fill, gave_up: false };
        }

        match accepted {
            Some(how) => {
                if flac_encode::present(&t.dest) {
                    on(Event::Note(format!("track {} appeared in the library meanwhile; not replaced", t.position)));
                    sum.skipped.push(t.position);
                    continue;
                }
                std::fs::create_dir_all(&plan.album_dir)
                    .with_context(|| format!("creating {}", plan.album_dir.display()))?;
                write_atomically(&t.dest, &r, &t.tags, ctx.cover, s.compression)?;
                let _ = writeln!(sum.log, "Track {:>2}: accepted ({}) -> {}", t.position, describe(how), t.dest.display());
                on(Event::Accepted { position: t.position, path: &t.dest, how });
                sum.accepted.push(t.position);
            }
            None => {
                let dir = s.rejected_dir().join(naming::safe_component(&plan.label));
                let path = dir.join(t.dest.file_name().expect("file name"));
                let saved = std::fs::create_dir_all(&dir)
                    .map_err(anyhow::Error::from)
                    .and_then(|()| write_atomically(&path, &r, &t.tags, ctx.cover, s.compression));
                if let Err(e) = &saved {
                    on(Event::Note(format!("couldn't save the rejected read of track {}: {e:#}", t.position)));
                }
                if !all_reads.is_empty() {
                    match keep_reads(&dir.join("reads"), t, &all_reads) {
                        Ok(d) => {
                            let _ = writeln!(sum.log, "Track {:>2}: its {} reads are kept in {} (for the CUETools repair)", t.position, all_reads.len(), d.display());
                        }
                        Err(e) => on(Event::Note(format!("couldn't keep the reads of track {}: {e:#}", t.position))),
                    }
                }
                let _ = writeln!(sum.log, "Track {:>2}: REJECTED after {reads_done} reads", t.position);
                on(Event::Rejected { position: t.position, path: saved.is_ok().then_some(path.as_path()), reads: reads_done });
                sum.rejected.push(t.position);
            }
        }
    }

    // Cover file next to the tracks (for players that look for one).
    if let Some(p) = ctx.cover {
        let path = plan.album_dir.join(cover::file_name(p));
        if plan.album_dir.is_dir() && !path.exists() && let Err(e) = std::fs::write(&path, &p.data) {
            on(Event::Note(format!("couldn't save {}: {e}", path.display())));
        }
    }

    let _ = writeln!(
        sum.log,
        "Result: {} accepted, {} rejected, {} already present\n",
        sum.accepted.len(),
        sum.rejected.len(),
        sum.skipped.len()
    );
    if plan.album_dir.is_dir() {
        append_log(plan, &sum.log)?;
    }
    Ok(sum)
}

fn describe(how: Acceptance) -> String {
    match how {
        Acceptance::AccurateRip { confidence } => format!("AccurateRip, confidence {confidence}"),
        Acceptance::OtherPressing { confidence, shift } => format!("AccurateRip for another pressing ({shift:+} frames), confidence {confidence}"),
        Acceptance::Repeat { reads } => format!("two identical reads, {reads} reads"),
        Acceptance::Voted { confidence, reads, sectors } => {
            format!("sector vote over {reads} reads ({sectors} damaged sectors), AccurateRip confidence {confidence}")
        }
        Acceptance::RepeatDiffers { reads } => {
            format!("two identical reads ({reads} reads), but it differs from AccurateRip's copy of this pressing: probably damaged, clean and re-rip")
        }
    }
}

/// Encodes to `<name>.part` in the same folder, then renames, so a crash
/// never leaves a half-written file under the real name.
fn write_atomically(
    dest: &Path,
    r: &TrackRead,
    tags: &[(String, String)],
    cover: Option<&Picture>,
    compression: u32,
) -> Result<()> {
    let name = dest.file_name().expect("file name").to_string_lossy();
    let tmp = dest.with_file_name(format!("{name}.part"));
    flac_encode::write_flac(&tmp, &r.frames, tags, cover, compression)?;
    flac_encode::put_in_place(&tmp, dest)
}

/// Appends to `<Album>[ CDn].log` in the album folder (one section per run).
fn append_log(plan: &DiscPlan, text: &str) -> Result<()> {
    use std::io::Write;
    let album = plan.album_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path = plan.album_dir.join(format!("{album}{}.log", plan.disc_suffix));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accuraterip::FRAMES_PER_SECTOR;
    use std::collections::BTreeMap;

    /// Fake disc: deterministic audio; `flaky` sectors return different data
    /// on every read; `bad` sectors always fail.
    struct FakeDisc {
        sectors: u32,
        flaky: Vec<u32>,
        bad: Vec<u32>,
        reads: u32,
    }

    impl SectorSource for FakeDisc {
        fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
            if lba + count > self.sectors || self.bad.iter().any(|b| (lba..lba + count).contains(b)) {
                return Err("read error".into());
            }
            self.reads += 1;
            let mut out = Vec::with_capacity(count as usize * 2352);
            for s in lba..lba + count {
                for i in 0..FRAMES_PER_SECTOR as u32 {
                    let mut v = (s * 588 + i).wrapping_mul(2_654_435_761);
                    if self.flaky.contains(&s) {
                        v ^= self.reads.wrapping_mul(0x9E37_79B9);
                    }
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Ok(out)
        }
    }

    fn setup(name: &str) -> (Settings, PathBuf) {
        let root = std::env::temp_dir().join(format!("localbeat-rip-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let s = Settings {
            flac_dir: root.join("FLAC"),
            phone_dir: root.join("phone"),
            misc_dir: root.join("Miscellaneous"),
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            country: None,
            reads: 4,
            compression: 0,
            flush_sectors: 0,
            opus_kbps: 160,
        };
        (s, root)
    }

    /// 3 tracks of 40 sectors each.
    fn toc() -> Toc {
        Toc::from_parts(vec![150, 190, 230], None, 270).unwrap()
    }

    #[test]
    fn widely_differing_reads_get_extra_reads_only_for_a_ctdb_repair() {
        // Track 2 (sectors 40..80) differs in many sectors on every read.
        let flaky: Vec<u32> = (42..78).step_by(2).collect();
        for (parity, want) in [(false, 4), (true, 8)] {
            let (s, root) = setup(if parity { "ctdb-more" } else { "ctdb-none" });
            let p = plan(&toc(), &Metadata::Unknown { album: "Test Album".into() }, &s.flac_dir);
            let mut disc = FakeDisc { sectors: 120, flaky: flaky.clone(), bad: vec![], reads: 0 };
            let ctx = RipContext { settings: &s, offset: 0, accuraterip: None, cover: None, log_header: String::new(), ctdb_parity: parity };
            let sum = rip(&mut disc, &p, &ctx, &mut |_| {}).unwrap();
            assert_eq!(sum.rejected, [2]);
            assert!(sum.log.contains(&format!("Track  2: REJECTED after {want} reads")), "{}", sum.log);
            std::fs::remove_dir_all(&root).ok();
        }
    }

    #[test]
    fn rips_verifies_and_resumes() {
        let (s, root) = setup("basic");
        let toc = toc();
        let meta = Metadata::Unknown { album: "Test Album".into() };
        let p = plan(&toc, &meta, &s.flac_dir);
        assert_eq!(p.tracks.len(), 3);
        assert!(p.tracks[0].dest.ends_with("Unknown Artist/Test Album/01. Unknown Artist - Track 1.flac"));

        // Track 2 is flaky (never reads the same twice) -> rejected.
        let mut disc = FakeDisc { sectors: 120, flaky: vec![55], bad: vec![], reads: 0 };
        let ctx = RipContext { settings: &s, offset: 0, accuraterip: None, cover: None, log_header: String::new(), ctdb_parity: false };
        let mut events = Vec::new();
        let sum = rip(&mut disc, &p, &ctx, &mut |e| {
            if let Event::Accepted { position, how, .. } = e {
                events.push((position, how));
            }
        })
        .unwrap();
        assert_eq!(sum.accepted, [1, 3]);
        assert_eq!(sum.rejected, [2]);
        assert_eq!(events[0], (1, Acceptance::Repeat { reads: 2 }));
        assert!(p.tracks[0].dest.exists());
        assert!(!p.tracks[1].dest.exists());
        assert!(s.rejected_dir().join("Unknown Artist - Test Album").join("02. Unknown Artist - Track 2.flac").exists());
        let log = std::fs::read_to_string(p.album_dir.join("Test Album.log")).unwrap();
        // Reads differing in one sector earn 4 extra reads; all 8 are kept
        // for the CUETools repair (no AccurateRip needed for that).
        assert!(log.contains("REJECTED after 8 reads"), "{log}");
        let reads_dir = s.rejected_dir().join("Unknown Artist - Test Album").join("reads").join("track02");
        assert_eq!(std::fs::read_dir(&reads_dir).unwrap().count(), 8);

        // Audio in the file is the offset-corrected read.
        let (_, frames) = crate::flacfile::read_cd_audio(&p.tracks[0].dest).unwrap();
        assert_eq!(frames.len(), 40 * 588);

        // Second run: only track 2 is missing.
        let p2 = plan(&toc, &meta, &s.flac_dir);
        assert_eq!(p2.missing().map(|t| t.position).collect::<Vec<_>>(), [2]);

        // An empty file left by a crash counts as missing and is replaced.
        std::fs::write(&p.tracks[2].dest, b"").unwrap();
        let p3 = plan(&toc, &meta, &s.flac_dir);
        assert_eq!(p3.missing().map(|t| t.position).collect::<Vec<_>>(), [2, 3]);
        let mut disc = FakeDisc { sectors: 120, flaky: vec![], bad: vec![], reads: 0 };
        let sum = rip(&mut disc, &p3, &ctx, &mut |_| {}).unwrap();
        assert_eq!(sum.accepted, [2, 3]);
        let (_, frames) = crate::flacfile::read_cd_audio(&p.tracks[2].dest).unwrap();
        assert_eq!(frames.len(), 40 * 588);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn accuraterip_match_needs_one_read_and_bad_reads_never_count() {
        let (s, root) = setup("ar");
        let toc = toc();
        let meta = Metadata::Unknown { album: "AR".into() };
        let p = plan(&toc, &meta, &s.flac_dir);

        // Known checksums for track 1 from a clean read of the same fake disc.
        let mut probe = FakeDisc { sectors: 120, flaky: vec![], bad: vec![], reads: 0 };
        let r = secure_read::read_track(&mut probe, p.tracks[0].sectors.clone(), p.readable_end, 0, &mut |_, _| {});
        let sums = accuraterip::checksums(&r.frames, true, false);
        let mut m = BTreeMap::new();
        m.insert(sums.v2, 9u8);
        let entries = DiscEntries { tracks: vec![m, BTreeMap::new(), BTreeMap::new()], ..Default::default() };

        // Track 3 has a sector that always fails: identical (silent) reads
        // must not be accepted as a repeat.
        let mut disc = FakeDisc { sectors: 120, flaky: vec![], bad: vec![100], reads: 0 };
        let ctx = RipContext { settings: &s, offset: 0, accuraterip: Some(&entries), cover: None, log_header: String::new(), ctdb_parity: false };
        let mut reads_of_1 = 0;
        let sum = rip(&mut disc, &p, &ctx, &mut |e| {
            if let Event::ReadStart { position: 1, .. } = e {
                reads_of_1 += 1;
            }
        })
        .unwrap();
        assert_eq!(reads_of_1, 1);
        assert_eq!(sum.accepted, [1, 2]);
        assert_eq!(sum.rejected, [3]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn other_pressing_is_found_by_frame450_and_verified() {
        for shift in [390i32, -390] {
            let (s, root) = setup(&format!("pressing{shift}"));
            // 3 tracks of 500 sectors (sector 450 must lie inside a track).
            let toc = Toc::from_parts(vec![150, 650, 1150], None, 1650).unwrap();
            let meta = Metadata::Unknown { album: "Pressing".into() };
            let p = plan(&toc, &meta, &s.flac_dir);

            // The database's pressing: the same audio, `shift` frames later.
            let mut probe = FakeDisc { sectors: 1500, flaky: vec![], bad: vec![], reads: 0 };
            let t2 = &p.tracks[1];
            let theirs = secure_read::read_track(&mut probe, t2.sectors.clone(), p.readable_end, shift, &mut |_, _| {});
            let sums = accuraterip::checksums(&theirs.frames, false, false);
            let base = accuraterip::FRAME450_SECTOR * FRAMES_PER_SECTOR;
            let crc450 = theirs.frames[base..base + FRAMES_PER_SECTOR]
                .iter()
                .enumerate()
                .fold(0u32, |a, (j, &f)| a.wrapping_add(f.wrapping_mul(j as u32 + 1)));
            let entries = DiscEntries {
                tracks: vec![BTreeMap::new(), BTreeMap::from([(sums.v2, 12u8)]), BTreeMap::new()],
                frame450: vec![BTreeMap::new(), BTreeMap::from([(crc450, 12u8)]), BTreeMap::new()],
            };

            let mut disc = FakeDisc { sectors: 1500, flaky: vec![], bad: vec![], reads: 0 };
            let ctx = RipContext { settings: &s, offset: 0, accuraterip: Some(&entries), cover: None, log_header: String::new(), ctdb_parity: false };
            let mut how = None;
            let mut reads_of_2 = 0;
            rip(&mut disc, &p, &ctx, &mut |e| match e {
                Event::Accepted { position: 2, how: h, .. } => how = Some(h),
                Event::ReadStart { position: 2, .. } => reads_of_2 += 1,
                _ => {}
            })
            .unwrap();
            assert_eq!(how, Some(Acceptance::OtherPressing { confidence: 12, shift: shift as i64 }), "shift {shift}");
            assert_eq!(reads_of_2, 1);
            // The file holds this disc's audio (offset 0), not the shifted one.
            let (_, frames) = crate::flacfile::read_cd_audio(&p.tracks[1].dest).unwrap();
            let ours = secure_read::read_track(&mut probe, t2.sectors.clone(), p.readable_end, 0, &mut |_, _| {});
            assert_eq!(frames, ours.frames);
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// A disc whose track 2 reads wrong in a different sector on each pass.
    struct Scratched {
        disc: FakeDisc,
        passes: u32,
    }

    impl SectorSource for Scratched {
        fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
            if lba == 40 {
                self.passes += 1; // a new read of track 2 starts
            }
            let mut out = self.disc.read_sectors(lba, count)?;
            let bad = 41 + self.passes * 5; // pass 1: sector 46, 2: 51, 3: 56...
            if (lba..lba + count).contains(&bad) && self.passes <= 4 {
                out[((bad - lba) as usize) * 2352 + 7] ^= 0x5A;
            }
            Ok(out)
        }
    }

    #[test]
    fn sector_vote_rebuilds_a_track_no_read_got_right() {
        let (s, root) = setup("vote");
        let toc = toc();
        let meta = Metadata::Unknown { album: "Vote".into() };
        let p = plan(&toc, &meta, &s.flac_dir);
        let mut probe = FakeDisc { sectors: 120, flaky: vec![], bad: vec![], reads: 0 };
        let good = secure_read::read_track(&mut probe, p.tracks[1].sectors.clone(), p.readable_end, 0, &mut |_, _| {});
        let sums = accuraterip::checksums(&good.frames, false, false);
        let entries = DiscEntries { tracks: vec![BTreeMap::new(), BTreeMap::from([(sums.v2, 30u8)]), BTreeMap::new()], ..Default::default() };

        let mut disc = Scratched { disc: FakeDisc { sectors: 120, flaky: vec![], bad: vec![], reads: 0 }, passes: 0 };
        let ctx = RipContext { settings: &s, offset: 0, accuraterip: Some(&entries), cover: None, log_header: String::new(), ctdb_parity: false };
        let mut how = None;
        rip(&mut disc, &p, &ctx, &mut |e| {
            if let Event::Accepted { position: 2, how: h, .. } = e {
                how = Some(h);
            }
        })
        .unwrap();
        assert_eq!(how, Some(Acceptance::Voted { confidence: 30, reads: 3, sectors: 3 }));
        let (_, frames) = crate::flacfile::read_cd_audio(&p.tracks[1].dest).unwrap();
        assert_eq!(frames, good.frames);
        std::fs::remove_dir_all(&root).ok();
    }
}
