//! `localbeat repair ALBUM --reads DIR`: try CUETools-parity repair on an
//! album, using the saved reads of rejected tracks (`<DIR>/trackNN/
//! readK.raw`) in place of those tracks. For now it only reports (nothing
//! is written); when the album folder already has a good copy of a track
//! (e.g. ripped again on another drive) the result is compared with it word
//! by word. Step 3 of docs/ctdb-repair.md.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use localbeat_core::ctdb_repair::{self, Doubt};
use localbeat_core::{ctdb, flacfile};

fn read_raw(p: &Path) -> Result<Vec<u32>> {
    let b = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

pub fn run(album: PathBuf, reads: Option<PathBuf>) -> Result<bool> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(&album)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .flac files in {}", album.display());
    }
    let library: Vec<Vec<u32>> = files.iter().map(|p| flacfile::read_cd_audio(p).map(|(_, a)| a)).collect::<Result<_>>()?;
    let lens: Vec<usize> = library.iter().map(Vec::len).collect();
    let toc = ctdb::toc_from_lengths(&lens)?;

    // The disc to repair: the library's tracks, with voted reads swapped in.
    let mut words = ctdb::disc_words(&library.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let truth = words.clone();
    let mut doubts: Vec<(usize, Doubt)> = Vec::new();
    let mut alts: std::collections::HashMap<usize, Vec<u16>> = std::collections::HashMap::new();
    let mut replaced = Vec::new();
    let mut start = 0usize; // word index of each track
    for (i, len) in lens.iter().enumerate() {
        let dir = reads.as_ref().map(|d| d.join(format!("track{:02}", i + 1)));
        if let Some(dir) = dir.filter(|d| d.is_dir()) {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "raw")).collect();
            paths.sort();
            let rs: Vec<Vec<u32>> = paths.iter().map(|p| read_raw(p)).collect::<Result<_>>()?;
            if rs.iter().any(|r| r.len() != *len) {
                bail!("the reads in {} aren't the track's length", dir.display());
            }
            let (w, d, a) = ctdb_repair::vote_words_alts(&rs);
            words[start..start + w.len()].copy_from_slice(&w);
            for ((k, s), v) in d.into_iter().zip(a) {
                doubts.push((start + k, s));
                alts.insert(start + k, v);
            }
            replaced.push(i + 1);
            println!("track {}: {} reads voted; {} doubtful words", i + 1, rs.len(), doubts.iter().filter(|(k, _)| *k >= start && *k < start + w.len()).count());
        }
        start += len * 2;
    }
    if replaced.is_empty() {
        println!("no saved reads for any track: repairing the library's copy as it is");
    }
    let wrong_before = words.iter().zip(&truth).filter(|(a, b)| a != b).count();
    {
        // How the damage lies in the parity's columns (we know the truth here).
        let layout = ctdb::Layout::new((words.len() / 2) as u64);
        let mut per_col_wrong = vec![0usize; ctdb::STRIDE];
        let mut per_col_flag = vec![0usize; ctdb::STRIDE];
        let flagged: std::collections::HashSet<usize> = doubts.iter().map(|d| d.0).collect();
        let mut wrong_unflagged = 0;
        let mut wrong_strong = 0;
        let strong: std::collections::HashSet<usize> = doubts.iter().filter(|d| d.1.strong()).map(|d| d.0).collect();
        for (w, (a, b)) in words.iter().zip(&truth).enumerate() {
            let row = w / ctdb::STRIDE;
            if row == 0 || row > layout.stridecount {
                continue;
            }
            if a != b {
                per_col_wrong[w % ctdb::STRIDE] += 1;
                if !flagged.contains(&w) { wrong_unflagged += 1; }
                if strong.contains(&w) { wrong_strong += 1; }
            }
            if strong.contains(&w) {
                per_col_flag[w % ctdb::STRIDE] += 1;
            }
        }
        let mut hist = std::collections::BTreeMap::new();
        for &c in &per_col_wrong {
            *hist.entry(c.min(20)).or_insert(0usize) += 1;
        }
        println!("after voting: {wrong_before} words wrong ({wrong_unflagged} of them not flagged, {wrong_strong} flagged strong)");
        println!("wrong words per column (count: columns): {hist:?}");
        // Unflagged wrong words: how close (in samples, same channel) to a flagged word?
        let mut near = [0usize; 6];
        for (w, (a, b)) in words.iter().zip(&truth).enumerate() {
            if a == b || flagged.contains(&w) {
                continue;
            }
            let d = (1..=4).find(|&d: &usize| flagged.contains(&(w + 2 * d)) || (w >= 2 * d && flagged.contains(&(w - 2 * d)))).unwrap_or(5);
            near[d] += 1;
        }
        println!("unflagged wrong words by distance to a flagged one (1..4 samples, 5 = farther): {:?}", &near[1..]);
        let fits = (0..ctdb::STRIDE).filter(|&c| per_col_wrong[c] <= 8).count();
        println!("columns with <= 8 wrong words: {fits} of {}", ctdb::STRIDE);
    }

    // The entry: the one whose parity lines up with the disc.
    let entries = ctdb::lookup(&toc)?;
    let mut chosen = None;
    for e in entries.iter().filter(|e| e.parity_url.is_some()) {
        let npar = e.npar as usize;
        let theirs = ctdb::fetch_parity(e)?;
        let base = ctdb::column_syndromes(&words, npar);
        if let Some(k) = ctdb::find_shift(&words, &base, &theirs, npar) {
            println!("entry {} (confirmed by {}, parity {npar}) lines up at a shift of {} samples", e.id, e.confidence, k / 2);
            chosen = Some((e.clone(), theirs, k));
            break;
        }
    }
    // A damaged disc may line up with nothing exactly: then take the entry
    // with exactly our TOC at shift 0 (TODO: search by fewest corrections).
    if chosen.is_none() {
        if let Some(e) = entries.iter().filter(|e| e.parity_url.is_some()).max_by_key(|e| e.confidence) {
            println!("nothing lines up exactly (damage everywhere): trying entry {} at shift 0", e.id);
            chosen = Some((e.clone(), ctdb::fetch_parity(e)?, 0));
        }
    }
    let Some((entry, theirs, k)) = chosen else { bail!("no CUETools entry with parity lines up with this disc") };
    if k != 0 {
        bail!("this entry is at another alignment ({} samples): repair at a shift isn't written yet", k / 2);
    }
    let npar = entry.npar as usize;
    let voted = words.clone();
    let flagged_set: std::collections::HashSet<usize> = doubts.iter().map(|d| d.0).collect();
    let t = std::time::Instant::now();
    doubts.sort();
    let spare: usize = std::env::var("LOCALBEAT_REPAIR_SPARE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let rep = ctdb_repair::repair_full(&mut words, &doubts, &alts, &theirs, npar, spare);
    println!("(safety margin: {spare} spare syndrome(s) per column)");
    println!(
        "repair: {} columns differed, {} fixed, {} could not be; {} words changed; {} doubtful words outside the parity's reach ({:.1} s)",
        rep.bad_columns,
        rep.fixed_columns,
        rep.failed_columns,
        rep.fixed_words,
        rep.uncovered_suspects,
        t.elapsed().as_secs_f64()
    );
    let wrong_after = words.iter().zip(&truth).filter(|(a, b)| a != b).count();
    {
        // Diagnostics against the album's copy: are "fixed" columns right?
        let mut bad_cols = std::collections::HashSet::new();
        for (w, (a, b)) in words.iter().zip(&truth).enumerate() {
            if a != b {
                bad_cols.insert(w % ctdb::STRIDE);
            }
        }
        println!("columns still wrong: {} (the repair gave up on {})", bad_cols.len(), rep.failed_columns);
        // With perfect flags (exactly the wrong words some read doubted), could
        // they fit? cost = 2*unflagged + flagged-wrong, capacity npar - spare.
        let layout = ctdb::Layout::new((words.len() / 2) as u64);
        let mut cost = vec![(0usize, 0usize); ctdb::STRIDE];
        for (w, (a, b)) in voted.iter().zip(&truth).enumerate() {
            let row = w / ctdb::STRIDE;
            if a == b || row == 0 || row > layout.stridecount || !bad_cols.contains(&(w % ctdb::STRIDE)) {
                continue;
            }
            let c = &mut cost[w % ctdb::STRIDE];
            if flagged_set.contains(&w) { c.1 += 1 } else { c.0 += 1 }
        }
        let mut solvable = 0;
        let mut hist = std::collections::BTreeMap::new();
        for &c in &bad_cols {
            let (u, f) = cost[c];
            if 2 * u + f <= npar - spare { solvable += 1; }
            *hist.entry((u, f)).or_insert(0usize) += 1;
        }
        println!("of those, solvable with perfect flags: {solvable}; (unflagged, flagged) wrong words per column: {hist:?}");
    }
    println!("words differing from the album's copy: {wrong_before} before, {wrong_after} after");

    // Checksums of the result.
    let frames: Vec<u32> = words.chunks_exact(2).map(|w| w[0] as u32 | (w[1] as u32) << 16).collect();
    let mut tracks = Vec::new();
    let mut pos = 0;
    for len in &lens {
        tracks.push(&frames[pos..pos + len]);
        pos += len;
    }
    let crcs = ctdb::track_crcs(&tracks);
    for (i, c) in crcs.iter().enumerate() {
        let ok = entry.track_crcs.get(i) == Some(c);
        let was = if replaced.contains(&(i + 1)) { " (from the saved reads)" } else { "" };
        println!("  track {}: {c:08x} {}{was}", i + 1, if ok { "matches CUETools" } else { "DIFFERS from CUETools" });
    }
    let whole = ctdb::disc_crc(&tracks) == entry.crc32;
    println!("whole disc: {}", if whole { "matches CUETools" } else { "differs" });
    Ok(whole)
}
