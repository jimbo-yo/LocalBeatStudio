//! Drive test commands: list drives, show a disc's TOC, read one track and
//! check it against AccurateRip, eject.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use localbeat_core::accuraterip::{self, Verdict};
use localbeat_core::secure_read;
use localbeat_platform::drive::{self, Drive};

fn open(path: Option<&str>) -> Result<Drive> {
    match path {
        Some(p) => Drive::open(p),
        None => Drive::open_first_with_audio(),
    }
}

fn mmss(sectors: u32) -> String {
    let secs = sectors / 75;
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn drives() -> Result<bool> {
    let list = drive::list_drives()?;
    if list.is_empty() {
        println!("No CD drives found.");
        return Ok(false);
    }
    for d in list {
        println!("{}", d.path);
        match &d.identity {
            Some(id) => {
                println!("  model:    {} (firmware {})", id.model(), id.revision);
                println!("  serial:   {}", if id.serial.is_empty() { "(none reported)" } else { &id.serial });
                println!("  key:      {}", id.key());
            }
            None => println!("  (couldn't read drive identity)"),
        }
        match drive::wake(&d.path) {
            Ok(true) => println!("  woken:    yes (SuperDrive: it now takes discs)"),
            Ok(false) => {}
            Err(e) => println!("  woken:    FAILED: {e:#}"),
        }
        println!("  audio CD: {}", if d.has_audio_cd { "yes" } else { "no" });
    }
    Ok(true)
}

pub fn toc(path: Option<&str>) -> Result<bool> {
    let drive = open(path)?;
    let toc = drive.read_toc()?;
    println!("Drive {}", drive.path());
    println!("  kind:            {}", toc.kind().as_str());
    println!("  CDTOC:           {toc}");
    println!("  MusicBrainz ID:  {}", toc.musicbrainz_id());
    println!("  MusicBrainz URL: {}", toc.musicbrainz_url());
    println!("  AccurateRip ID:  {}", toc.accuraterip_id());
    println!("  length:          {}", mmss(toc.audio_leadout_normalized()));
    println!();
    for t in toc.audio_tracks() {
        let r = t.sector_range_normalized();
        println!("  {:>2}  start {:>6}  length {:>6}", t.number(), r.start, mmss(t.sectors()));
    }
    Ok(true)
}

pub struct ReadTrackArgs {
    pub drive: Option<String>,
    pub track: usize,
    pub offset: i32,
    pub reads: u32,
    pub speed: Option<u8>,
    pub flush: u32,
    pub wav: Option<PathBuf>,
}

/// Reads one track `reads` times. Ok(true) if it matched AccurateRip or two
/// reads agreed.
pub fn read_track(a: ReadTrackArgs) -> Result<bool> {
    let mut drive = open(a.drive.as_deref())?;
    let toc = drive.read_toc()?;
    let t = toc
        .audio_track(a.track)
        .ok_or_else(|| anyhow!("no audio track {} (disc has {})", a.track, toc.audio_len()))?;
    let range = t.sector_range_normalized();
    let readable_end = toc.audio_leadout_normalized();

    let model = drive.identity().map(|i| i.model()).unwrap_or_else(|_| "?".into());
    println!(
        "Drive {} ({model}), track {} of {}, {}, offset {:+}",
        drive.path(),
        a.track,
        toc.audio_len(),
        mmss(t.sectors()),
        a.offset
    );

    if a.speed.is_some() && let Err(e) = drive.set_speed(a.speed) {
        println!("  (speed request failed: {e:#})");
    }

    let entries = match accuraterip::fetch(&toc) {
        Ok(Some(e)) => Some(e),
        Ok(None) => {
            println!("  disc not in AccurateRip; only repeat reads can verify");
            None
        }
        Err(e) => {
            println!("  AccurateRip lookup failed ({e:#}); only repeat reads can verify");
            None
        }
    };

    let pos = t.position();
    let mut crcs: Vec<u32> = Vec::new();
    let mut verified = false;
    let mut last = None;
    for i in 1..=a.reads {
        if i > 1 && a.flush > 0 {
            print!("  read {i}: flushing drive cache ({} sectors)...", a.flush);
            std::io::stdout().flush().ok();
            secure_read::flush_cache(&mut drive, range.clone(), readable_end, a.flush);
            println!();
        }
        let started = Instant::now();
        let r = secure_read::read_track(&mut drive, range.clone(), readable_end, a.offset, &mut |done, total| {
            print!("\r  read {i}: {:>3}%", done * 100 / total.max(1));
            std::io::stdout().flush().ok();
        });
        let secs = started.elapsed().as_secs_f64();
        let speed = (t.sectors() as f64 / 75.0) / secs.max(0.001);

        let sums = accuraterip::checksums(&r.frames, pos.is_first(), pos.is_last());
        let crc = r.crc32();
        let verdict = entries.as_ref().map(|e| e.verdict(a.track, sums)).unwrap_or(Verdict::NotInDatabase);
        let ar = match verdict {
            Verdict::Match { version, confidence } => {
                verified = true;
                format!("AccurateRip v{version} match, confidence {confidence}")
            }
            Verdict::NoMatch => "no AccurateRip match".into(),
            Verdict::NotInDatabase => "not in AccurateRip".into(),
        };
        let repeat = crcs.contains(&crc);
        if repeat {
            verified = true;
        }
        println!(
            "\r  read {i}: CRC {crc:08X}  AR v1 {:08X} v2 {:08X}  {ar}{}  ({secs:.0} s, {speed:.1}x{}{})",
            sums.v1,
            sums.v2,
            if repeat { ", same as an earlier read" } else { "" },
            if r.unreadable.is_empty() { String::new() } else { format!(", {} unreadable sectors", r.unreadable.len()) },
            if r.edge_fill > 0 { format!(", {} edge frames silent", r.edge_fill) } else { String::new() },
        );
        crcs.push(crc);
        last = Some(r);
    }

    if let (Some(path), Some(r)) = (&a.wav, &last) {
        std::fs::write(path, wav_file(r.pcm_bytes())).with_context(|| format!("writing {}", path.display()))?;
        println!("  wrote {}", path.display());
    }
    println!("{}", if verified { "VERIFIED" } else { "NOT VERIFIED" });
    Ok(verified)
}

/// 44-byte RIFF header + CD PCM.
fn wav_file(pcm: Vec<u8>) -> Vec<u8> {
    let len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // channels
    out.extend_from_slice(&44_100u32.to_le_bytes());
    out.extend_from_slice(&176_400u32.to_le_bytes()); // byte rate
    out.extend_from_slice(&4u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

pub fn eject(path: Option<&str>) -> Result<bool> {
    let path = match path {
        Some(p) => drive::normalize_path(p),
        None => drive::list_drives()?
            .into_iter()
            .find(|d| d.has_audio_cd)
            .map(|d| d.path)
            .ok_or_else(|| anyhow!("no drive has a disc; give --drive"))?,
    };
    Drive::open(&path)?.eject()?;
    println!("Ejected {path}");
    Ok(true)
}

/// Identifies the disc on MusicBrainz: candidate releases best first, and
/// the tags track 1 would get.
pub fn identify(path: Option<&str>, cdtoc: Option<&str>, country: Option<&str>) -> Result<bool> {
    use localbeat_core::musicbrainz;
    let toc = match cdtoc {
        Some(t) => cdtoc::Toc::from_cdtoc(t).map_err(|e| anyhow!("bad CDTOC: {e}"))?,
        None => open(path)?.read_toc()?,
    };
    println!("Disc ID {} ({} tracks)", toc.musicbrainz_id(), toc.audio_len());
    let mut releases = musicbrainz::lookup(&toc)?;
    if releases.is_empty() {
        println!("Not in MusicBrainz. Add it: {}", toc.musicbrainz_url());
        return Ok(false);
    }
    musicbrainz::rank(&mut releases, country);
    for (i, r) in releases.iter().enumerate() {
        println!(
            "{} {:>2}. {}{}\n       {}",
            if i == 0 { "→" } else { " " },
            i + 1,
            r.summary(),
            if r.fuzzy { "  [similar TOC, not this exact disc]" } else { "" },
            r.id
        );
    }
    let best = &releases[0];
    let genres = musicbrainz::genres(best).unwrap_or_else(|e| {
        println!("(genre lookup failed: {e:#})");
        Vec::new()
    });
    println!("\nTags for track 1:");
    for (k, v) in musicbrainz::track_tags(best, 1, &genres, &toc) {
        println!("  {k}={v}");
    }
    println!("\nTracks:");
    for t in &best.medium.tracks {
        let secs = t.length_ms / 1000;
        println!("  {:>2}. {} - {}  ({}:{:02})", t.position, t.artist, t.title, secs / 60, secs % 60);
    }
    Ok(true)
}
