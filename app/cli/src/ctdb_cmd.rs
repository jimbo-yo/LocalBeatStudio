//! `localbeat ctdb ALBUM...`: checks a ripped album against the CUETools
//! database (CTDB): which pressings it knows, and which of our tracks each
//! one confirms. Step 1 of CTDB repair (docs/ctdb-repair.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use localbeat_core::{ctdb, flacfile};

pub fn run(dirs: Vec<PathBuf>) -> Result<bool> {
    let mut all_ok = true;
    for dir in dirs {
        all_ok &= check(&dir)?;
    }
    Ok(all_ok)
}

fn check(dir: &Path) -> Result<bool> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .flac files in {}", dir.display());
    }
    // One disc at a time (multi-disc albums share a folder).
    let mut discs: BTreeMap<u32, Vec<(usize, PathBuf)>> = BTreeMap::new();
    for p in files {
        let f = flacfile::read_tags(&p)?;
        let disc = f.tag("DISCNUMBER").and_then(|d| d.split('/').next()?.trim().parse().ok()).unwrap_or(1);
        discs.entry(disc).or_default().push((f.track_number().unwrap_or(0), p));
    }
    let mut ok = true;
    for (disc, mut tracks) in discs {
        tracks.sort();
        println!("\n{} (disc {disc}, {} tracks)", dir.display(), tracks.len());
        let audio: Vec<Vec<u32>> = tracks.iter().map(|(_, p)| flacfile::read_cd_audio(p).map(|(_, a)| a)).collect::<Result<_>>()?;
        let lens: Vec<usize> = audio.iter().map(Vec::len).collect();
        let toc = ctdb::toc_from_lengths(&lens)?;
        let refs: Vec<&[u32]> = audio.iter().map(Vec::as_slice).collect();
        let ours = ctdb::track_crcs(&refs);
        let disc_crc = ctdb::disc_crc(&refs);
        println!("  TOC {toc}");
        println!("  our CRCs: disc {disc_crc:08x}, tracks {}", ours.iter().map(|c| format!("{c:08x}")).collect::<Vec<_>>().join(" "));
        let mut entries = ctdb::lookup(&toc)?;
        if entries.is_empty() {
            println!("  not in the CUETools database");
            continue;
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.confidence));
        let mut shown = 0;
        for e in &entries {
            let marks: String = ours.iter().zip(&e.track_crcs).map(|(a, b)| if a == b { '✓' } else { '·' }).collect();
            let hits = ours.iter().zip(&e.track_crcs).filter(|(a, b)| a == b).count();
            // The well-confirmed entries, and any that agree with us.
            if e.confidence < 3 && hits == 0 {
                continue;
            }
            shown += 1;
            let parity = match &e.parity_url {
                Some(_) => format!("parity {}", e.npar),
                None => "no parity".into(),
            };
            let whole = if e.crc32 == disc_crc { "  WHOLE DISC MATCHES" } else { "" };
            println!("  entry {:>9}  confirmed by {:>5}  {parity:>9}  tracks {marks} ({hits}/{}){whole}", e.id, e.confidence, ours.len());
        }
        // Parity: compare our columns with the best-confirmed entry that has it.
        let hits = |e: &ctdb::Entry| ours.iter().zip(&e.track_crcs).filter(|(a, b)| a == b).count();
        if let Some(e) = entries.iter().filter(|e| e.parity_url.is_some()).max_by_key(|e| (hits(e), e.confidence)) {
            let t = std::time::Instant::now();
            let words = ctdb::disc_words(&refs);
            let npar = e.npar as usize;
            let ours = ctdb::column_syndromes(&words, npar);
            match ctdb::fetch_parity(e) {
                Ok(theirs) => {
                    let m = ctdb::matching_columns(&ours, &theirs, npar);
                    println!("  parity of entry {} (npar {npar}): {m} of {} columns agree at offset 0 ({:.1} s)", e.id, ctdb::STRIDE, t.elapsed().as_secs_f64());
                }
                Err(err) => println!("  parity of entry {}: {err:#}", e.id),
            }
        }
        let others = entries.len() - shown;
        if others > 0 {
            println!("  ({others} more single submissions that match none of our tracks)");
        }
        let best = entries.iter().map(|e| ours.iter().zip(&e.track_crcs).filter(|(a, b)| a == b).count()).max().unwrap_or(0);
        if best < ours.len() {
            ok = false;
            println!("  Tracks marked · differ from that pressing's copy (or the entry was ripped at another alignment: repair finds that with the parity).");
        }
    }
    Ok(ok)
}
