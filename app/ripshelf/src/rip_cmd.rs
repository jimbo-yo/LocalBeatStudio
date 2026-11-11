//! `ripshelf rip`, `phone` and `offset`: the engine with a text front end.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ripshelf_core::accuraterip::Verdict;
use ripshelf_core::config::{self, Settings};
use ripshelf_core::musicbrainz::Release;
use ripshelf_core::phone_copy;
use ripshelf_core::rip::Acceptance;
use ripshelf_engine::notify::Notifier;
use ripshelf_engine::{self as engine, Event, Frontend, Options, ReleaseChoice};
use ripshelf_platform::drive::Drive;
use ripshelf_platform::phone_notify;

pub struct RipArgs {
    pub drive: Option<String>,
    /// Keep going: wait for the next disc after each one.
    pub repeat: bool,
    pub notify_phone: bool,
    pub offset: Option<i32>,
    pub release: Option<String>,
    pub country: Option<String>,
    pub unknown: bool,
    pub reads: Option<u32>,
    pub no_eject: bool,
    pub flac_dir: Option<PathBuf>,
}

pub fn rip(a: RipArgs) -> Result<bool> {
    let mut s = Settings::load();
    if let Some(d) = a.flac_dir {
        s.flac_dir = d;
    }
    if let Some(c) = a.country {
        s.country = Some(c).filter(|c| !c.is_empty());
    }
    if let Some(r) = a.reads {
        s.reads = r.max(1);
    }
    let opts = Options { drive: a.drive, offset: a.offset, release: a.release, unknown: a.unknown, no_eject: a.no_eject };
    let mut p = Printer::default();
    if a.repeat && a.notify_phone {
        let after = config::Prefs::load(&s).notify_after_secs;
        let config_dir = s.config_dir.clone();
        p.notifier = Some(Notifier::new(
            true,
            Duration::from_secs(after),
            Box::new(move |msg: &str| match phone_notify::notify(&config_dir, msg) {
                Ok(phone) => println!("(Phone notification sent to {phone})"),
                Err(e) => println!("(Couldn't notify the phone: {e:#})"),
            }),
        ));
        println!("Phone notifications on: after {after} s waiting for a disc, and for problems.");
    }
    if a.repeat {
        println!("Ripping disc after disc (Ctrl+C to stop).");
        engine::rip_loop(&s, &opts, &p)?;
        Ok(true)
    } else {
        engine::rip_once(&s, &opts, &p)
    }
}

/// Prints events; picks releases automatically.
#[derive(Default)]
struct Printer {
    tracks: Mutex<Vec<engine::TrackInfo>>,
    notifier: Option<Arc<Notifier>>,
}

impl Printer {
    fn title(&self, n: usize) -> String {
        self.tracks.lock().unwrap().iter().find(|t| t.position == n).map(|t| t.title.clone()).unwrap_or_default()
    }
    fn total(&self) -> usize {
        self.tracks.lock().unwrap().len()
    }
}

impl Frontend for Printer {
    fn event(&self, e: Event) {
        if let Some(n) = &self.notifier {
            n.observe(&e);
        }
        match e {
            Event::Waiting { done } => {
                let d = if done > 0 { format!(" ({done} done)") } else { String::new() };
                println!("\nWaiting for a disc{d}...");
            }
            Event::DiscStart { drive, model, offset } => {
                println!("\n======== {drive} ({model}), read offset {offset:+} ========")
            }
            Event::Stage(_) => {}
            Event::Identified { disc_id, tracks } => println!("Disc: {tracks} tracks, MusicBrainz disc ID {disc_id}"),
            Event::Release { release, .. } => println!("Release: {}", release.summary()),
            Event::UnknownDisc { album } => println!("Not identified: ripping as \"Unknown Artist / {album}\""),
            Event::Cover(Some(p)) => println!("Cover: {}x{} {}", p.width, p.height, p.mime),
            Event::Cover(None) => println!("Cover: none"),
            Event::AccurateRip { in_database } => {
                if !in_database {
                    println!("AccurateRip: disc not in the database; tracks verify by two identical reads");
                }
            }
            Event::Plan { album_dir, tracks, .. } => {
                println!("Album folder: {}", album_dir.display());
                let have = tracks.iter().filter(|t| t.exists).count();
                if have > 0 && have < tracks.len() {
                    println!("{have} of {} tracks already present", tracks.len());
                }
                *self.tracks.lock().unwrap() = tracks;
                println!();
            }
            Event::Skipped { position } => {
                println!("{position:>2}/{} {}: already in the library", self.total(), self.title(position))
            }
            Event::ReadStart { position, read } => {
                if read == 1 {
                    println!("{position:>2}/{} {}", self.total(), self.title(position));
                }
            }
            Event::Progress { read, done, total, .. } => {
                print!("\r      read {read}: {:>3}%", done * 100 / total.max(1));
                std::io::stdout().flush().ok();
            }
            Event::ReadDone { read, crc, verdict, unreadable, repeat, .. } => {
                let ar = match verdict {
                    Verdict::Match { confidence, .. } => format!("AccurateRip match ({confidence})"),
                    Verdict::NoMatch => "no AccurateRip match".into(),
                    Verdict::NotInDatabase => "not in AccurateRip".into(),
                };
                let extra = match (unreadable, repeat) {
                    (0, true) => ", same as an earlier read".to_string(),
                    (0, false) => String::new(),
                    (n, _) => format!(", {n} unreadable sectors"),
                };
                println!("\r      read {read}: CRC {crc:08X}, {ar}{extra}");
            }
            Event::Accepted { how, .. } => match how {
                Acceptance::AccurateRip { .. } => println!("      saved (verified by AccurateRip)"),
                Acceptance::OtherPressing { shift, .. } => println!("      saved (verified by AccurateRip: another pressing, shifted {shift:+} frames)"),
                Acceptance::Repeat { reads } => println!("      saved (two identical reads, {reads} reads)"),
                Acceptance::Voted { reads, sectors, .. } => {
                    println!("      saved (rebuilt by sector vote over {reads} reads, {sectors} damaged sectors; verified by AccurateRip)")
                }
                Acceptance::RepeatDiffers { reads } => println!(
                    "      saved, but CHECK IT: two identical reads ({reads} reads) that differ from AccurateRip's copy of this pressing (damaged spot? clean and re-rip)"
                ),
            },
            Event::Rejected { reads, kept, .. } => {
                println!("      REJECTED: no verified read in {reads} reads");
                if let Some(p) = kept {
                    println!("      last read kept in {}", p.display());
                }
            }
            Event::RipDone { accepted, rejected, skipped, elapsed } => {
                println!(
                    "\nDone in {} min: {accepted} saved, {} rejected, {skipped} already present.",
                    elapsed.as_secs() / 60,
                    rejected.len()
                );
                if !rejected.is_empty() {
                    println!("Rejected tracks {rejected:?}: clean the disc (or try another drive) and rip again; only those are redone.");
                }
            }
            Event::Ejected { ok } => {
                if !ok {
                    println!("(couldn't eject)");
                }
            }
            Event::PhoneFile { name, error } => match error {
                None => println!("  phone copy: {name}"),
                Some(e) => println!("  phone copy FAILED: {name}: {e}"),
            },
            Event::PhoneDone { converted, failed } => {
                if converted + failed > 0 {
                    println!("Phone copies: {converted} made, {failed} failed");
                }
            }
            Event::DiscDone { .. } => {}
            Event::DiscFailed(e) => println!("\nerror: {e}\nThat disc didn't finish; carrying on."),
            Event::Note(n) => println!("  note: {n}"),
        }
    }

    fn choose_release(&self, rs: &[Release]) -> ReleaseChoice {
        let list = || rs.iter().map(|r| format!("  {}  {}", r.id, r.summary())).collect::<Vec<_>>().join("\n");
        if rs.is_empty() {
            println!("MusicBrainz doesn't know this disc. Rip it with generic names: ripshelf rip --unknown");
            return ReleaseChoice::Skip;
        }
        if rs[0].fuzzy {
            println!(
                "MusicBrainz doesn't have this exact disc, only releases with a similar TOC (track names may not fit):\n{}\n\
                 Pick one with --release <ID>, or rip with --unknown.",
                list()
            );
            return ReleaseChoice::Skip;
        }
        if rs.len() > 1 {
            println!("{} releases match; using the first (choose another with --release <ID>):\n{}", rs.len(), list());
        }
        ReleaseChoice::Use(0)
    }
}

/// Converts `src` (a library or one album) to Opus under `dest`, printing
/// each file. Ok(true) if nothing failed.
pub fn phone_copies(src: &Path, dest: &Path, kbps: u32) -> Result<bool> {
    let stats = phone_copy::convert_tree(src, dest, kbps, &|rel, r| match r {
        Ok(()) => println!("  OK    {}", rel.display()),
        Err(e) => println!("  FAIL  {}: {e:#}", rel.display()),
    })?;
    println!(
        "  {} converted, {} already up to date, {} failed{}  ->  {}",
        stats.converted.len(),
        stats.up_to_date,
        stats.failed.len(),
        if stats.covers_copied > 0 { format!(", {} cover images copied", stats.covers_copied) } else { String::new() },
        dest.display()
    );
    if !stats.protected.is_empty() {
        println!("  {} DRM-protected iTunes files (.m4p) skipped: they can't be converted:", stats.protected.len());
        for p in &stats.protected {
            println!("        {}", p.display());
        }
    }
    Ok(stats.failed.is_empty())
}

/// `ripshelf offset [set N]`: show or save the drive's read offset.
pub fn offset(drive: Option<&str>, value: Option<i32>) -> Result<bool> {
    let s = Settings::load();
    let d = match drive {
        Some(p) => Drive::open(p)?,
        None => {
            let paths = ripshelf_platform::drive::drive_paths();
            let first = paths.first().ok_or_else(|| anyhow!("no CD drives found"))?;
            Drive::open(first)?
        }
    };
    let id = d.identity()?;
    let file = s.offsets_file();
    let saved = config::lookup_offset(&file, &id.key(), &id.serial);
    match value {
        None => {
            match saved {
                Some(o) => println!("{} ({}): read offset {o:+}", id.model(), id.key()),
                None => println!("{} ({}): no offset saved in {}", id.model(), id.key(), file.display()),
            }
            Ok(true)
        }
        Some(v) => {
            if let Some(old) = saved.filter(|&o| o != v) {
                println!("Replacing saved offset {old:+} with {v:+}.");
            }
            config::save_offset(&file, &id.key(), v)?;
            println!("Saved read offset {v:+} for {} in {}", id.model(), file.display());
            Ok(true)
        }
    }
}
