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
                exists: dest.exists(),
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
    /// Two clean reads with the same CRC (`reads` = reads it took).
    Repeat { reads: u32 },
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
        for read in 1..=s.reads.max(1) {
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
            if clean {
                if let Verdict::Match { confidence, .. } = verdict {
                    accepted = Some(Acceptance::AccurateRip { confidence });
                } else if repeat {
                    accepted = Some(Acceptance::Repeat { reads: read });
                }
                clean_crcs.push(crc);
            }
            last = Some(r);
            if accepted.is_some() {
                break;
            }
        }
        let r = last.expect("at least one read");

        match accepted {
            Some(how) => {
                if t.dest.exists() {
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
                let _ = writeln!(sum.log, "Track {:>2}: REJECTED after {reads_done} reads", t.position);
                on(Event::Rejected { position: t.position, path: saved.is_ok().then_some(path.as_path()), reads: reads_done });
                sum.rejected.push(t.position);
            }
        }
    }

    // Cover file next to the tracks (for players that look for one).
    if let Some(p) = ctx.cover {
        let path = plan.album_dir.join(cover::file_name(p));
        if plan.album_dir.is_dir() && !path.exists() {
            if let Err(e) = std::fs::write(&path, &p.data) {
                on(Event::Note(format!("couldn't save {}: {e}", path.display())));
            }
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
        Acceptance::Repeat { reads } => format!("two identical reads, {reads} reads"),
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
    std::fs::rename(&tmp, dest).with_context(|| format!("renaming to {}", dest.display()))?;
    Ok(())
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
        let root = std::env::temp_dir().join(format!("ripshelf-rip-{name}-{}", std::process::id()));
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
    fn rips_verifies_and_resumes() {
        let (s, root) = setup("basic");
        let toc = toc();
        let meta = Metadata::Unknown { album: "Test Album".into() };
        let p = plan(&toc, &meta, &s.flac_dir);
        assert_eq!(p.tracks.len(), 3);
        assert!(p.tracks[0].dest.ends_with("Unknown Artist/Test Album/01. Unknown Artist - Track 1.flac"));

        // Track 2 is flaky (never reads the same twice) -> rejected.
        let mut disc = FakeDisc { sectors: 120, flaky: vec![55], bad: vec![], reads: 0 };
        let ctx = RipContext { settings: &s, offset: 0, accuraterip: None, cover: None, log_header: String::new() };
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
        assert!(log.contains("REJECTED after 4 reads"), "{log}");

        // Audio in the file is the offset-corrected read.
        let (_, frames) = crate::flacfile::read_cd_audio(&p.tracks[0].dest).unwrap();
        assert_eq!(frames.len(), 40 * 588);

        // Second run: only track 2 is missing.
        let p2 = plan(&toc, &meta, &s.flac_dir);
        assert_eq!(p2.missing().map(|t| t.position).collect::<Vec<_>>(), [2]);
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
        let entries = DiscEntries { tracks: vec![m, BTreeMap::new(), BTreeMap::new()] };

        // Track 3 has a sector that always fails: identical (silent) reads
        // must not be accepted as a repeat.
        let mut disc = FakeDisc { sectors: 120, flaky: vec![], bad: vec![100], reads: 0 };
        let ctx = RipContext { settings: &s, offset: 0, accuraterip: Some(&entries), cover: None, log_header: String::new() };
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
}
