//! The rip session, shared by the command line and the window: pick a drive,
//! identify the disc, rip what's missing, eject, make phone copies, and loop
//! disc after disc. Front ends get [`Event`]s and answer the questions in
//! [`Frontend`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ripshelf_core::accuraterip::{self, Verdict};
use ripshelf_core::config::{self, Settings};
use ripshelf_core::flac_encode::Picture;
use ripshelf_core::musicbrainz::{self, Release};
use ripshelf_core::rip::{self, Acceptance, Metadata, RipContext};
use ripshelf_core::secure_read::SectorSource;
use ripshelf_core::{cover, lock, phone_copy};
use ripshelf_platform::drive::{self, Drive, DriveIdentity};

pub mod notify;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Only this drive (else any free drive with an audio CD).
    pub drive: Option<String>,
    /// Read offset (else the one saved for the drive).
    pub offset: Option<i32>,
    /// Use this MusicBrainz release (single disc only).
    pub release: Option<String>,
    /// Rip with generic names.
    pub unknown: bool,
    pub no_eject: bool,
}

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub position: usize,
    pub title: String,
    pub artist: String,
    pub seconds: u32,
    /// Already in the library.
    pub exists: bool,
}

/// What happened, in order. Owned, so it can cross threads.
#[derive(Debug, Clone)]
pub enum Event {
    /// Waiting for a disc (`done` discs so far).
    Waiting { done: usize },
    DiscStart { drive: String, model: String, offset: i32 },
    /// A step, for a status line ("Looking up the disc...").
    Stage(String),
    Identified { disc_id: String, tracks: usize },
    Release { release: Box<Release>, alternatives: usize },
    UnknownDisc { album: String },
    Cover(Option<Arc<Picture>>),
    AccurateRip { in_database: bool },
    Plan { album_dir: PathBuf, label: String, tracks: Vec<TrackInfo> },
    Skipped { position: usize },
    ReadStart { position: usize, read: u32 },
    Progress { position: usize, read: u32, done: u32, total: u32 },
    ReadDone { position: usize, read: u32, crc: u32, verdict: Verdict, unreadable: usize, repeat: bool },
    Accepted { position: usize, how: Acceptance },
    Rejected { position: usize, reads: u32, kept: Option<PathBuf> },
    /// The rip part is over (before eject and phone copies).
    RipDone { accepted: usize, rejected: Vec<usize>, skipped: usize, elapsed: Duration },
    Ejected { ok: bool },
    PhoneFile { name: String, error: Option<String> },
    PhoneDone { converted: usize, failed: usize },
    /// The disc is finished (`ok` = nothing rejected and no error).
    DiscDone { ok: bool },
    /// The disc failed (it's ejected unless --no-eject).
    DiscFailed(String),
    Note(String),
}

pub enum ReleaseChoice {
    Use(usize),
    Unknown,
    Skip,
}

pub trait Frontend: Sync {
    fn event(&self, e: Event);

    /// Pick from `candidates` (best first; `fuzzy` ones only have a similar
    /// TOC). Empty = MusicBrainz doesn't know the disc.
    fn choose_release(&self, candidates: &[Release]) -> ReleaseChoice;

    /// Stop as soon as possible (checked during reads and while waiting).
    fn cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Cancelled {}

fn saved_offset(s: &Settings, id: Option<&DriveIdentity>) -> Option<i32> {
    let id = id?;
    config::lookup_offset(&s.offsets_file(), &id.key(), &id.serial)
}

/// The first drive with an audio CD that no other ripshelf is using.
pub fn free_drive_with_audio(s: &Settings) -> Result<Option<String>> {
    for path in drive::drive_paths() {
        let free = lock::try_lock(&s.cache_dir, &lock::drive_lock_name(&path))?.is_some();
        if free && drive::has_audio_cd(&path) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// What [`eject_idle`] did, per drive path.
#[derive(Debug, Default)]
pub struct EjectReport {
    pub ejected: Vec<String>,
    /// Being ripped (by this or another ripshelf): left alone.
    pub busy: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Ejects the CD from every drive no rip is using. A drive the rip loop is
/// only checking (it holds the lock for a moment every 2 s) is retried for
/// a few seconds before it counts as busy.
pub fn eject_idle(s: &Settings) -> EjectReport {
    let mut report = EjectReport::default();
    for path in drive::drive_paths() {
        let mut lock = None;
        for _ in 0..8 {
            match lock::try_lock(&s.cache_dir, &lock::drive_lock_name(&path)) {
                Ok(Some(l)) => {
                    lock = Some(l);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                Err(e) => {
                    report.failed.push((path.clone(), format!("{e:#}")));
                    break;
                }
            }
        }
        let Some(_lock) = lock else {
            if !report.failed.iter().any(|(p, _)| p == &path) {
                report.busy.push(path);
            }
            continue;
        };
        if !drive::has_audio_cd(&path) {
            continue;
        }
        match Drive::open(&path).and_then(|d| d.eject()) {
            Ok(()) => report.ejected.push(path),
            Err(e) => report.failed.push((path, format!("{e:#}"))),
        }
    }
    report
}

/// MusicBrainz disc ID of the disc in the drive, if any.
fn disc_in(path: &str) -> Option<String> {
    Drive::open(path).and_then(|d| d.read_toc()).ok().map(|t| t.musicbrainz_id().to_string())
}

/// Rips one disc: `opts.drive`, else the first free drive with an audio CD.
/// Ok(true) if every track is now in the library.
pub fn rip_once(s: &Settings, opts: &Options, fe: &dyn Frontend) -> Result<bool> {
    let path = match &opts.drive {
        Some(p) => drive::normalize_path(p),
        None => free_drive_with_audio(s)?.ok_or_else(|| anyhow!("no drive that isn't busy has an audio CD in it"))?,
    };
    let _lock = lock::try_lock(&s.cache_dir, &lock::drive_lock_name(&path))?
        .ok_or_else(|| anyhow!("{path} is busy: another ripshelf is ripping from it"))?;
    rip_disc(&path, s, opts, fe)
}

/// Rips each disc put into a watched drive (all, or `opts.drive`), ejects,
/// waits for the next, until `fe.cancelled()`. A failed disc is ejected and
/// the loop carries on. A finished disc is ignored until it's removed.
pub fn rip_loop(s: &Settings, opts: &Options, fe: &dyn Frontend) -> Result<()> {
    use std::collections::HashMap;
    if opts.release.is_some() {
        bail!("a specific release can't be used when ripping disc after disc");
    }
    let watch: Vec<String> = opts.drive.iter().map(|d| drive::normalize_path(d)).collect();
    let mut finished: HashMap<String, String> = HashMap::new();
    let mut announce = true;
    let mut done = 0;
    while !fe.cancelled() {
        if announce {
            fe.event(Event::Waiting { done });
            announce = false;
        }
        let paths = if watch.is_empty() { drive::drive_paths() } else { watch.clone() };
        for path in paths {
            if fe.cancelled() {
                break;
            }
            let Some(lock) = lock::try_lock(&s.cache_dir, &lock::drive_lock_name(&path))? else { continue };
            match disc_in(&path) {
                None => {
                    finished.remove(&path);
                }
                Some(id) if finished.get(&path) == Some(&id) => {}
                Some(id) => {
                    // Let the OS finish looking at the new disc.
                    std::thread::sleep(Duration::from_secs(3));
                    match rip_disc(&path, s, opts, fe) {
                        Ok(_) => {}
                        Err(e) if e.is::<Cancelled>() => return Ok(()),
                        Err(e) => {
                            fe.event(Event::DiscFailed(format!("{e:#}")));
                            if !opts.no_eject {
                                let ok = Drive::open(&path).and_then(|d| d.eject()).is_ok();
                                fe.event(Event::Ejected { ok });
                            }
                        }
                    }
                    finished.insert(path.clone(), id);
                    done += 1;
                    announce = true;
                }
            }
            drop(lock);
        }
        for _ in 0..10 {
            if fe.cancelled() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}

/// Fails every read once the front end asks to stop, so a read in progress
/// ends quickly.
struct Cancellable<'a> {
    drive: &'a mut Drive,
    fe: &'a dyn Frontend,
}

impl SectorSource for Cancellable<'_> {
    fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
        if self.fe.cancelled() {
            return Err("stopped".into());
        }
        self.drive.read_sectors(lba, count)
    }
}

/// Rips the disc in `path` (the caller holds the drive's lock).
pub fn rip_disc(path: &str, s: &Settings, opts: &Options, fe: &dyn Frontend) -> Result<bool> {
    let mut drive = Drive::open(path)?;
    let id = drive.identity().ok();
    let model = id.as_ref().map(|i| i.model()).unwrap_or_else(|| "unknown drive".into());
    let offset = match opts.offset.or_else(|| saved_offset(s, id.as_ref())) {
        Some(o) => o,
        None => bail!(
            "No read offset saved for this drive ({model}). Look up the model at \
             https://www.accuraterip.com/driveoffsets.htm and save it with: ripshelf offset set <N>"
        ),
    };
    fe.event(Event::DiscStart { drive: drive.path().to_string(), model: model.clone(), offset });

    fe.event(Event::Stage("Reading the disc".into()));
    let toc = drive.read_toc()?;
    fe.event(Event::Identified { disc_id: toc.musicbrainz_id().to_string(), tracks: toc.audio_len() });

    let now = chrono::Local::now();
    let unknown_album = || format!("Unknown Album {}", now.format("%Y-%m-%d %H%M"));
    let release: Option<Release> = if opts.unknown {
        None
    } else if let Some(rid) = &opts.release {
        Some(musicbrainz::release(rid, &toc)?)
    } else {
        fe.event(Event::Stage("Looking up the disc on MusicBrainz".into()));
        let mut rs = musicbrainz::lookup(&toc)?;
        musicbrainz::rank(&mut rs, s.country.as_deref());
        match fe.choose_release(&rs) {
            ReleaseChoice::Use(i) if i < rs.len() => {
                let n = rs.len();
                let r = rs.swap_remove(i);
                fe.event(Event::Release { release: Box::new(r.clone()), alternatives: n - 1 });
                Some(r)
            }
            ReleaseChoice::Use(_) | ReleaseChoice::Unknown => None,
            ReleaseChoice::Skip => bail!("skipped"),
        }
    };
    let genres = match &release {
        Some(r) => musicbrainz::genres(r).unwrap_or_else(|e| {
            fe.event(Event::Note(format!("genre lookup failed: {e:#}")));
            Vec::new()
        }),
        None => Vec::new(),
    };
    let meta = match &release {
        Some(r) => Metadata::Release { release: r, genres: &genres },
        None => {
            let album = unknown_album();
            fe.event(Event::UnknownDisc { album: album.clone() });
            Metadata::Unknown { album }
        }
    };

    let plan = rip::plan(&toc, &meta, &s.flac_dir);
    let infos: Vec<TrackInfo> = plan
        .tracks
        .iter()
        .map(|t| {
            let tag = |k: &str| t.tags.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            TrackInfo {
                position: t.position,
                title: tag("TITLE"),
                artist: tag("ARTIST"),
                seconds: (t.sectors.end - t.sectors.start) / 75,
                exists: t.exists,
            }
        })
        .collect();
    fe.event(Event::Plan { album_dir: plan.album_dir.clone(), label: plan.label.clone(), tracks: infos });

    let started = Instant::now();
    let mut ok = true;
    if plan.missing().next().is_none() {
        fe.event(Event::Note(format!("All {} tracks are already in the library", plan.tracks.len())));
        fe.event(Event::RipDone { accepted: 0, rejected: vec![], skipped: plan.tracks.len(), elapsed: started.elapsed() });
    } else {
        let cover: Option<Picture> = match release.as_ref() {
            Some(r) => {
                fe.event(Event::Stage("Fetching the cover".into()));
                match cover::fetch_front(r) {
                    Ok(p) => p,
                    Err(e) => {
                        fe.event(Event::Note(format!("cover download failed: {e:#}")));
                        None
                    }
                }
            }
            None => None,
        };
        let cover = cover.map(Arc::new);
        fe.event(Event::Cover(cover.clone()));
        let entries = match accuraterip::fetch(&toc) {
            Ok(e) => e,
            Err(e) => {
                fe.event(Event::Note(format!("AccurateRip lookup failed: {e:#}")));
                None
            }
        };
        fe.event(Event::AccurateRip { in_database: entries.is_some() });

        let mut header = format!(
            "== ripshelf {} rip, {} ==\nDrive: {model} ({})\nDisc ID: {}  CDTOC: {toc}\n",
            env!("CARGO_PKG_VERSION"),
            now.format("%Y-%m-%d %H:%M"),
            id.as_ref().map(|i| i.key()).unwrap_or_default(),
            toc.musicbrainz_id(),
        );
        if let Some(r) = &release {
            header.push_str(&format!("Release: {} [{}]\n", r.summary(), r.id));
        }
        let ctx = RipContext {
            settings: s,
            offset,
            accuraterip: entries.as_ref(),
            cover: cover.as_deref(),
            log_header: header,
        };

        fe.event(Event::Stage("Ripping".into()));
        let mut src = Cancellable { drive: &mut drive, fe };
        let mut cancelled = false;
        let sum = rip::rip(&mut src, &plan, &ctx, &mut |e| {
            if fe.cancelled() {
                cancelled = true;
            }
            let ev = match e {
                rip::Event::Skipped { position, .. } => Event::Skipped { position },
                rip::Event::ReadStart { position, read } => Event::ReadStart { position, read },
                rip::Event::Progress { position, read, done, total } => Event::Progress { position, read, done, total },
                rip::Event::ReadDone { position, read, crc, verdict, unreadable, repeat } => {
                    Event::ReadDone { position, read, crc, verdict, unreadable, repeat }
                }
                rip::Event::Accepted { position, how, .. } => Event::Accepted { position, how },
                rip::Event::Rejected { position, path, reads } => {
                    Event::Rejected { position, reads, kept: path.map(|p| p.to_path_buf()) }
                }
                rip::Event::Note(n) => Event::Note(n),
            };
            // Don't report the junk reads made after Stop.
            if !cancelled {
                fe.event(ev);
            }
        });
        if cancelled || fe.cancelled() {
            return Err(Cancelled.into());
        }
        let sum = sum?;
        ok = sum.rejected.is_empty();
        fe.event(Event::RipDone {
            accepted: sum.accepted.len(),
            rejected: sum.rejected.clone(),
            skipped: sum.skipped.len(),
            elapsed: started.elapsed(),
        });
    }

    if opts.no_eject {
        drop(drive);
    } else {
        let ok = drive.eject().is_ok();
        fe.event(Event::Ejected { ok });
    }

    if plan.album_dir.is_dir() {
        if let Ok(rel) = plan.album_dir.strip_prefix(&s.flac_dir) {
            fe.event(Event::Stage("Making phone copies".into()));
            let _phone = lock::lock(&s.cache_dir, "phone")?;
            let stats = phone_copy::convert_tree(&plan.album_dir, &s.phone_dir.join(rel), s.opus_kbps, &|rel, r| {
                fe.event(Event::PhoneFile {
                    name: rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    error: r.as_ref().err().map(|e| format!("{e:#}")),
                })
            })?;
            fe.event(Event::PhoneDone { converted: stats.converted.len(), failed: stats.failed.len() });
        }
    }
    fe.event(Event::DiscDone { ok });
    Ok(ok)
}
