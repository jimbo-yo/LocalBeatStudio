//! `ripshelf rip` and `ripshelf offset`.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use ripshelf_core::accuraterip::{self, Verdict};
use ripshelf_core::config::{self, Settings};
use ripshelf_core::rip::{self, Acceptance, Event, Metadata, RipContext};
use ripshelf_core::{cover, musicbrainz};
use ripshelf_platform::drive::{Drive, DriveIdentity};

pub struct RipArgs {
    pub drive: Option<String>,
    pub offset: Option<i32>,
    pub release: Option<String>,
    pub country: Option<String>,
    pub unknown: bool,
    pub reads: Option<u32>,
    pub no_eject: bool,
    pub flac_dir: Option<PathBuf>,
}

fn open(path: Option<&str>) -> Result<Drive> {
    match path {
        Some(p) => Drive::open(p),
        None => Drive::open_first_with_audio(),
    }
}

fn saved_offset(s: &Settings, id: Option<&DriveIdentity>) -> Option<i32> {
    let id = id?;
    config::lookup_offset(&s.offsets_file(), &id.key(), &id.serial)
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

    let mut drive = open(a.drive.as_deref())?;
    let id = drive.identity().ok();
    let model = id.as_ref().map(|i| i.model()).unwrap_or_else(|| "unknown drive".into());
    let offset = match a.offset.or_else(|| saved_offset(&s, id.as_ref())) {
        Some(o) => o,
        None => bail!(
            "No read offset saved for this drive ({model}).\n\
             Look up the model at https://www.accuraterip.com/driveoffsets.htm, then save it with\n\
             \x20 ripshelf offset set <N>\n\
             (or pass --offset <N> for this rip only)."
        ),
    };
    println!("Drive {} ({model}), read offset {offset:+}", drive.path());

    let toc = drive.read_toc()?;
    println!("Disc: {} tracks, MusicBrainz disc ID {}", toc.audio_len(), toc.musicbrainz_id());

    // Identify.
    let now = chrono::Local::now();
    let release;
    let genres;
    let meta = if a.unknown {
        Metadata::Unknown { album: format!("Unknown Album {}", now.format("%Y-%m-%d %H%M")) }
    } else {
        release = match &a.release {
            Some(rid) => musicbrainz::release(rid, &toc)?,
            None => choose_release(&toc, s.country.as_deref())?,
        };
        println!("Release: {}", release.summary());
        genres = musicbrainz::genres(&release).unwrap_or_else(|e| {
            println!("  (genre lookup failed: {e:#})");
            Vec::new()
        });
        Metadata::Release { release: &release, genres: &genres }
    };

    let plan = rip::plan(&toc, &meta, &s.flac_dir);
    let missing: Vec<usize> = plan.missing().map(|t| t.position).collect();
    println!("Album folder: {}", plan.album_dir.display());
    if missing.is_empty() {
        println!("All {} tracks are already in the library; nothing to rip.", plan.tracks.len());
        eject(&drive, a.no_eject);
        return Ok(true);
    }
    if missing.len() < plan.tracks.len() {
        println!("{} of {} tracks already present; ripping {:?}", plan.tracks.len() - missing.len(), plan.tracks.len(), missing);
    }

    let cover = match &meta {
        Metadata::Release { release, .. } => match cover::fetch_front(release) {
            Ok(Some(p)) => {
                println!("Cover: {}x{} {}", p.width, p.height, p.mime);
                Some(p)
            }
            Ok(None) => {
                println!("Cover: none in the Cover Art Archive");
                None
            }
            Err(e) => {
                println!("Cover: download failed ({e:#})");
                None
            }
        },
        Metadata::Unknown { .. } => None,
    };
    let entries = match accuraterip::fetch(&toc) {
        Ok(Some(e)) => Some(e),
        Ok(None) => {
            println!("AccurateRip: disc not in the database; tracks verify by two identical reads");
            None
        }
        Err(e) => {
            println!("AccurateRip: lookup failed ({e:#}); tracks verify by two identical reads");
            None
        }
    };

    let mut header = format!(
        "== ripshelf {} rip, {} ==\nDrive: {model} ({})\nDisc ID: {}  CDTOC: {toc}\n",
        env!("CARGO_PKG_VERSION"),
        now.format("%Y-%m-%d %H:%M"),
        id.as_ref().map(|i| i.key()).unwrap_or_default(),
        toc.musicbrainz_id(),
    );
    if let Metadata::Release { release, .. } = &meta {
        header.push_str(&format!("Release: {} [{}]\n", release.summary(), release.id));
    }
    let ctx = RipContext { settings: &s, offset, accuraterip: entries.as_ref(), cover: cover.as_ref(), log_header: header };

    let total = plan.tracks.len();
    let title = |n: usize| -> String {
        plan.tracks
            .iter()
            .find(|t| t.position == n)
            .and_then(|t| t.tags.iter().find(|(k, _)| k == "TITLE").map(|(_, v)| v.clone()))
            .unwrap_or_default()
    };
    println!();
    let started = std::time::Instant::now();
    let sum = rip::rip(&mut drive, &plan, &ctx, &mut |e| match e {
        Event::Skipped { position, .. } => println!("{position:>2}/{total} {}: already in the library", title(position)),
        Event::ReadStart { position, read } => {
            if read == 1 {
                println!("{position:>2}/{total} {}", title(position));
            }
        }
        Event::Progress { read, done, total: t, .. } => {
            print!("\r      read {read}: {:>3}%", done * 100 / t.max(1));
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
            Acceptance::Repeat { reads } => println!("      saved (two identical reads, {reads} reads)"),
        },
        Event::Rejected { reads, path, .. } => {
            println!("      REJECTED: no verified read in {reads} reads");
            if let Some(p) = path {
                println!("      last read kept in {}", p.display());
            }
        }
        Event::Note(n) => println!("      note: {n}"),
    })?;

    let mins = started.elapsed().as_secs() / 60;
    println!(
        "\nDone in {mins} min: {} saved, {} rejected, {} already present.",
        sum.accepted.len(),
        sum.rejected.len(),
        sum.skipped.len()
    );
    if !sum.rejected.is_empty() {
        println!("Rejected tracks {:?}: clean the disc (or try another drive) and run rip again; it will only redo those.", sum.rejected);
    }
    eject(&drive, a.no_eject);
    Ok(sum.rejected.is_empty())
}

fn choose_release(toc: &cdtoc::Toc, country: Option<&str>) -> Result<musicbrainz::Release> {
    let mut rs = musicbrainz::lookup(toc)?;
    if rs.is_empty() {
        bail!(
            "MusicBrainz doesn't know this disc.\nAdd it at {}\nor rip it with generic names: ripshelf rip --unknown",
            toc.musicbrainz_url()
        );
    }
    musicbrainz::rank(&mut rs, country);
    let list = |rs: &[musicbrainz::Release]| {
        rs.iter().map(|r| format!("  {}  {}", r.id, r.summary())).collect::<Vec<_>>().join("\n")
    };
    if rs[0].fuzzy {
        bail!(
            "MusicBrainz doesn't have this exact disc, only releases with a similar TOC (track names may not fit):\n{}\n\
             Pick one with --release <ID>, or rip with --unknown.",
            list(&rs)
        );
    }
    if rs.len() > 1 {
        println!("{} releases match; using the first (choose another with --release <ID>):\n{}", rs.len(), list(&rs));
    }
    Ok(rs.swap_remove(0))
}

fn eject(drive: &Drive, skip: bool) {
    if skip {
        return;
    }
    if let Err(e) = drive.eject() {
        println!("(couldn't eject: {e:#})");
    }
}

/// `ripshelf offset [set N]`: show or save the drive's read offset.
pub fn offset(drive: Option<&str>, value: Option<i32>) -> Result<bool> {
    let s = Settings::load();
    let d = match drive {
        Some(p) => Drive::open(p)?,
        None => {
            let list = ripshelf_platform::drive::list_drives()?;
            let first = list.first().ok_or_else(|| anyhow!("no CD drives found"))?;
            Drive::open(&first.path)?
        }
    };
    let id = d.identity()?;
    let file = s.offsets_file();
    match value {
        None => {
            match saved_offset(&s, Some(&id)) {
                Some(o) => println!("{} ({}): read offset {o:+}", id.model(), id.key()),
                None => println!("{} ({}): no offset saved in {}", id.model(), id.key(), file.display()),
            }
            Ok(true)
        }
        Some(v) => {
            if let Some(old) = saved_offset(&s, Some(&id)) {
                if old != v {
                    println!("Replacing saved offset {old:+} with {v:+}.");
                }
            }
            config::save_offset(&file, &id.key(), v)?;
            println!("Saved read offset {v:+} for {} in {}", id.model(), file.display());
            Ok(true)
        }
    }
}
