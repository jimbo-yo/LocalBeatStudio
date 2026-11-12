//! `localbeat repair ALBUM [--reads DIR] [--write]`: repair a disc's
//! missing or damaged tracks with the CUETools database (the same as the rip
//! does after rejecting tracks; core::repair_disc). `--reads` is the disc's
//! `<cache>/rejected/<Artist - Album>/reads` folder (its parent holds the
//! rejected FLACs). Without --write it only reports.

use std::path::PathBuf;

use anyhow::{Result, bail};
use localbeat_core::accuraterip::Verdict;
use localbeat_core::config::Settings;
use localbeat_core::repair_disc::{self, Source};

pub fn run(album: PathBuf, reads: Option<PathBuf>, write: bool) -> Result<bool> {
    let s = Settings::load();
    let rejected = reads.as_ref().and_then(|r| r.parent().map(|p| p.to_path_buf()));
    let mut tocs = repair_disc::cdtocs_in(&album)?;
    if tocs.is_empty() {
        if let Some(r) = &rejected {
            tocs = repair_disc::cdtocs_in(r)?;
        }
    }
    if tocs.is_empty() {
        bail!("no CDTOC tag in the album's (or the rejected) files: can't tell the disc's layout");
    }
    let mut all_ok = true;
    for toc in tocs {
        let t0 = std::time::Instant::now();
        let r = repair_disc::repair_disc(&s, &album, &toc, rejected.as_deref(), write)?;
        for l in &r.log {
            println!("{l}");
        }
        for t in &r.tracks {
            let src = match t.source {
                Source::Reads(n) => format!(" (from {n} saved reads)"),
                Source::Album => String::new(),
                Source::Rejected => " (from the rejected copy)".into(),
            };
            let ar = match t.accuraterip {
                Some(Verdict::Match { confidence, .. }) => format!("AccurateRip match ({confidence})"),
                Some(Verdict::NoMatch) => "AccurateRip: no match".into(),
                _ => "not in AccurateRip".into(),
            };
            let state = if t.written.is_some() {
                "  -> written"
            } else if t.verified && t.source != Source::Album {
                "  -> verified (run with --write)"
            } else {
                ""
            };
            println!("  track {:>2}: CUETools {}, {ar}{src}{state}", t.position, if t.ctdb_match { "matches" } else { "DIFFERS" });
        }
        if r.entry.is_some() {
            println!("whole disc: {} ({:.0} s)", if r.whole_disc { "matches CUETools" } else { "differs from CUETools" }, t0.elapsed().as_secs_f64());
        }
        let missing = r.still_missing();
        if !missing.is_empty() {
            all_ok = false;
            println!("still missing: track(s) {}", missing.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", "));
        }
    }
    Ok(all_ok)
}
