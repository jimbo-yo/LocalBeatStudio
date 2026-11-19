//! The rip session, shared by the command line and the window: pick a drive,
//! identify the disc, rip what's missing, eject, make phone copies, and loop
//! disc after disc. Front ends get [`Event`]s and answer the questions in
//! [`Frontend`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use localbeat_core::accuraterip::{self, Verdict};
use localbeat_core::config::{self, Settings};
use localbeat_core::flac_encode::Picture;
use localbeat_core::musicbrainz::{self, Release};
use localbeat_core::rip::{self, Acceptance, Metadata, RipContext};
use localbeat_core::secure_read::SectorSource;
use localbeat_core::{cover, lock};
use localbeat_platform::drive::{self, Drive, DriveIdentity};

pub mod join;
pub mod notify;
pub mod push;
pub mod remove;

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
    /// Rejected tracks are being repaired with the CUETools database.
    RepairStart,
    /// A track was repaired (or added) and is now in the library, verified.
    Repaired { position: usize },
    /// The repair is over: tracks now in the library, and still missing
    /// (clean the disc and rip it again: the reads add up).
    RepairDone { repaired: Vec<usize>, missing: Vec<usize> },
    /// The disc was stopped on request ([`Frontend::stop_disc`]); it's
    /// ejected next and the loop carries on.
    DiscStopped,
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

    /// Give up on the disc being ripped (eject it) but keep watching the
    /// drive. The front end clears its flag when the next disc starts.
    fn stop_disc(&self) -> bool {
        false
    }

    /// After [`Frontend::stop_disc`]: eject the disc (else it stays in, and
    /// isn't ripped again until it's taken out and put back).
    fn eject_stopped(&self) -> bool {
        true
    }
}

/// Whether the disc being ripped should end now.
fn stopping(fe: &dyn Frontend) -> bool {
    fe.cancelled() || fe.stop_disc()
}

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Cancelled {}

/// The drive's saved offset, else the one known for its model (then saved,
/// so Settings shows it as the drive's own).
fn saved_offset(s: &Settings, id: Option<&DriveIdentity>) -> Option<i32> {
    let id = id?;
    let file = s.offsets_file();
    config::lookup_offset(&file, &id.key(), &id.serial).or_else(|| {
        let known = config::known_offset(&id.model())?;
        let _ = config::save_offset(&file, &id.key(), known);
        Some(known)
    })
}

/// The first drive with an audio CD that no other localbeat is using.
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
    /// Being ripped (by this or another LocalBeat Studio): left alone.
    pub busy: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Ejects the CD from every drive no rip is using. A drive the rip loop is
/// only checking (it holds the lock for a moment every 2 s) is retried for
/// a few seconds before it counts as busy.
pub fn eject_idle(s: &Settings) -> EjectReport {
    let mut report = EjectReport::default();
    for path in drive::drive_paths() {
        match eject_one(s, &path, false) {
            Ok(EjectOutcome::Ejected) => report.ejected.push(path),
            Ok(EjectOutcome::Busy) => report.busy.push(path),
            Ok(EjectOutcome::NoDisc) => {}
            Err(e) => report.failed.push((path, format!("{e:#}"))),
        }
    }
    report
}

#[derive(Debug, PartialEq)]
pub enum EjectOutcome {
    Ejected,
    /// Being ripped (by this or another LocalBeat Studio): left alone.
    Busy,
    NoDisc,
}

/// Ejects `path` if no rip is using it (see [`eject_idle`]). With `any`, a
/// drive without an audio CD is opened too (a data disc, or an empty tray).
pub fn eject_one(s: &Settings, path: &str, any: bool) -> Result<EjectOutcome> {
    let mut lock = None;
    for _ in 0..8 {
        match lock::try_lock(&s.cache_dir, &lock::drive_lock_name(path))? {
            Some(l) => {
                lock = Some(l);
                break;
            }
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    let Some(_lock) = lock else { return Ok(EjectOutcome::Busy) };
    if !any && !drive::has_audio_cd(path) {
        return Ok(EjectOutcome::NoDisc);
    }
    Drive::open(path).and_then(|d| d.eject())?;
    Ok(EjectOutcome::Ejected)
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
        .ok_or_else(|| anyhow!("{path} is busy: another localbeat is ripping from it"))?;
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
    let mut woken: std::collections::HashSet<String> = Default::default();
    let mut announce = true;
    let mut done = 0;
    while !fe.cancelled() {
        if announce {
            fe.event(Event::Waiting { done });
            announce = false;
        }
        let present = drive::drive_paths();
        // A drive that went away (unplugged) needs waking again when back.
        woken.retain(|p| present.contains(p));
        let paths = if watch.is_empty() { present.clone() } else { watch.clone() };
        for path in paths {
            if fe.cancelled() {
                break;
            }
            let Some(lock) = lock::try_lock(&s.cache_dir, &lock::drive_lock_name(&path))? else { continue };
            if present.contains(&path) && !woken.contains(&path) {
                // Tried once per appearance, so a failure is said only once.
                woken.insert(path.clone());
                if let Err(e) = drive::wake(&path) {
                    fe.event(Event::Note(format!("{e:#}")));
                }
            }
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
                        Err(e) if e.is::<Cancelled>() && fe.cancelled() => return Ok(()),
                        Err(e) if e.is::<Cancelled>() => {
                            // Stopped on request: eject even with --no-eject
                            // (that's what was asked for).
                            fe.event(Event::DiscStopped);
                            if fe.eject_stopped() {
                                let ok = Drive::open(&path).and_then(|d| d.eject()).is_ok();
                                fe.event(Event::Ejected { ok });
                            }
                        }
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
        if stopping(self.fe) {
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
             https://www.accuraterip.com/driveoffsets.htm and save it with: localbeat offset set <N>"
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
        let mut rs = musicbrainz::lookup_with(&toc, &|secs| {
            fe.event(Event::Stage(format!("MusicBrainz is busy; trying again in {secs} s")))
        })?;
        musicbrainz::rank(&mut rs, s.country.as_deref());
        match fe.choose_release(&rs) {
            ReleaseChoice::Use(i) if i < rs.len() => {
                let n = rs.len();
                let r = rs.swap_remove(i);
                if r.near {
                    fe.event(Event::Note(format!(
                        "MusicBrainz has this release with a disc ID a sector or two off from this disc's \
                         (another drive's reading of the TOC); same layout, so it's this disc: {}",
                        r.summary()
                    )));
                }
                fe.event(Event::Release { release: Box::new(r.clone()), alternatives: n - 1 });
                Some(r)
            }
            ReleaseChoice::Use(_) | ReleaseChoice::Unknown => None,
            ReleaseChoice::Skip if stopping(fe) => return Err(Cancelled.into()),
            ReleaseChoice::Skip => bail!("skipped"),
        }
    };
    if stopping(fe) {
        return Err(Cancelled.into());
    }
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
    // Held while this disc writes into the album, so the album isn't sent to
    // the server half done (the pusher skips it until the next round).
    let album_name = localbeat_core::outbox::album_name(s, &plan.album_dir).ok();
    let _ripping = match &album_name {
        Some(a) => Some(localbeat_core::lock::lock(&s.cache_dir, &localbeat_core::outbox::ripping_lock_name(a))?),
        None => None,
    };
    // Whether this disc put anything new in the album (then it goes to the server).
    let mut wrote = false;

    let started = Instant::now();
    let mut ok = true;
    let mut rejected_now: Vec<usize> = Vec::new();
    if plan.missing().next().is_none() {
        fe.event(Event::Note(format!("All {} tracks are already in the library", plan.tracks.len())));
        fe.event(Event::RipDone { accepted: 0, rejected: vec![], skipped: plan.tracks.len(), elapsed: started.elapsed() });
    } else {
        // A full disk failed each disc after a track or two (the encode
        // couldn't be written) and ejected it: wait for room instead.
        wait_for_space(&plan, fe)?;
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
        if stopping(fe) {
            return Err(Cancelled.into());
        }
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
            "== LocalBeat Studio {} rip, {} ==\nDrive: {model} ({})\nDisc ID: {}  CDTOC: {toc}\n",
            env!("CARGO_PKG_VERSION"),
            now.format("%Y-%m-%d %H:%M"),
            id.as_ref().map(|i| i.key()).unwrap_or_default(),
            toc.musicbrainz_id(),
        );
        if let Some(r) = &release {
            header.push_str(&format!("Release: {} [{}]\n", r.summary(), r.id));
        }
        // Can the CUETools database repair this disc, if it comes to that?
        let ctdb_parity = localbeat_core::ctdb::has_parity(&toc.to_string());
        let ctx = RipContext {
            settings: s,
            offset,
            accuraterip: entries.as_ref(),
            cover: cover.as_deref(),
            log_header: header,
            ctdb_parity,
        };

        fe.event(Event::Stage("Ripping".into()));
        let mut src = Cancellable { drive: &mut drive, fe };
        let mut cancelled = false;
        // Tracks already in the library if the rip ends early (Stop, or an
        // error such as a full disk): they still go to the server.
        let mut accepted_any = false;
        let sum = rip::rip(&mut src, &plan, &ctx, &mut |e| {
            if stopping(fe) {
                cancelled = true;
            }
            if matches!(e, rip::Event::Accepted { .. }) {
                accepted_any = true;
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
        if cancelled || stopping(fe) {
            if accepted_any {
                queue_for_server(s, album_name.as_deref(), fe);
            }
            return Err(Cancelled.into());
        }
        let sum = match sum {
            Ok(sum) => sum,
            Err(e) => {
                if accepted_any {
                    queue_for_server(s, album_name.as_deref(), fe);
                }
                return Err(e);
            }
        };
        ok = sum.rejected.is_empty();
        rejected_now = sum.rejected.clone();
        wrote |= !sum.accepted.is_empty();
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

    // Rejected tracks: try the CUETools repair with everything there is
    // (this rip's reads plus any from earlier rips of the disc). The drive
    // is already free: this is computing only.
    if !rejected_now.is_empty() {
        fe.event(Event::Stage("Repairing with the CUETools database".into()));
        fe.event(Event::RepairStart);
        let dir = s.rejected_dir().join(localbeat_core::naming::safe_component(&plan.label));
        match localbeat_core::repair_disc::repair_disc(s, &plan.album_dir, &toc.to_string(), Some(&dir), true) {
            Ok(r) => {
                for l in &r.log {
                    fe.event(Event::Note(l.clone()));
                }
                for (t, words) in r.record().tracks {
                    fe.event(Event::Note(format!("Repair: track {t} {words}")));
                }
                fe.event(Event::Note(format!("Repair finished: {}", r.summary())));
                if let Err(e) = localbeat_core::repair_disc::save_record(&r, &plan.album_dir, &dir, &plan.disc_suffix) {
                    fe.event(Event::Note(format!("couldn't save the repair record: {e:#}")));
                }
                let repaired: Vec<usize> = r.tracks.iter().filter(|t| t.written.is_some()).map(|t| t.position).collect();
                for &p in &repaired {
                    fe.event(Event::Repaired { position: p });
                }
                wrote |= !repaired.is_empty();
                let missing = r.still_missing();
                ok = missing.is_empty();
                fe.event(Event::RepairDone { repaired, missing });
            }
            Err(e) => {
                fe.event(Event::Note(format!("CUETools repair failed: {e:#}")));
                fe.event(Event::RepairDone { repaired: Vec::new(), missing: rejected_now.clone() });
            }
        }
    }

    // Off to the server (docs/server-library.md): queued here, sent by the
    // pusher (the window's, or `localbeat push`) when on the home network.
    if wrote {
        queue_for_server(s, album_name.as_deref(), fe);
    }

    // No phone copies here any more (2026-11-12): phone sync converts from
    // the library at sync time, and making them held the drive for a minute
    // or two after the eject (a disc pushed into the SuperDrive then met a
    // mechanism that wasn't ready).
    fe.event(Event::DiscDone { ok });
    Ok(ok)
}

/// Puts the album in the outbox for the server (when a server is set up).
/// The pusher sends it once the `_ripping` lock is released.
fn queue_for_server(s: &Settings, album: Option<&str>, fe: &dyn Frontend) {
    let Some(a) = album else { return };
    if !config::Prefs::load(s).server.enabled() {
        return;
    }
    match localbeat_core::outbox::Outbox::update(s, |o| o.add(a)) {
        Ok(()) => fe.event(Event::Note(format!("Queued for the server: {a}"))),
        Err(e) => fe.event(Event::Note(format!("couldn't queue {a} for the server: {e:#}"))),
    }
}

/// Room the missing tracks need: FLAC of CD audio is usually 50-70% of the
/// raw size; 80% plus a margin (cover, log, the `.part` being written).
fn space_needed(plan: &rip::DiscPlan) -> u64 {
    let sectors: u64 = plan.missing().map(|t| u64::from(t.sectors.end - t.sectors.start)).sum();
    sectors * 2352 * 4 / 5 + 64 * 1024 * 1024
}

/// Waits, disc still in, until the library's disk has room for this disc
/// (or the rip is stopped). Doesn't wait if the free space can't be found out.
fn wait_for_space(plan: &rip::DiscPlan, fe: &dyn Frontend) -> Result<()> {
    let need = space_needed(plan);
    let mb = |b: u64| b / (1024 * 1024);
    let mut said = false;
    loop {
        let Some(free) = localbeat_platform::disk::free_space(&plan.album_dir) else { return Ok(()) };
        if free >= need {
            if said {
                fe.event(Event::Note(format!("Enough disk space now ({} MB free); ripping", mb(free))));
            }
            return Ok(());
        }
        if !said {
            fe.event(Event::Note(format!(
                "Not enough disk space for this disc: {} MB free where the library is, it needs about {} MB. \
                 Free some space and the rip starts by itself.",
                mb(free),
                mb(need)
            )));
            said = true;
        }
        fe.event(Event::Stage(format!("Waiting for disk space: {} MB free, about {} MB needed", mb(free), mb(need))));
        for _ in 0..25 {
            if stopping(fe) {
                return Err(Cancelled.into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
