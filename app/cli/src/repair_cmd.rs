//! `localbeat repair ALBUM [--reads DIR] [--write]`: repair a disc with the
//! CUETools database's parity (docs/ctdb-repair.md).
//!
//! The disc is put together track by track from the best source there is:
//! the saved reads of a rejected track (voted, doubtful words noted), else
//! the album's FLAC, else the rejected FLAC (one read). The disc layout
//! comes from the CDTOC tag. After the repair, each track is checked
//! against the database and against AccurateRip; with `--write`, tracks
//! that pass and differ from what's in the album are written (originals
//! kept in `<cache>/repaired-originals/`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use localbeat_core::accuraterip::{self, Verdict};
use localbeat_core::ctdb_repair::{self, Doubt};
use localbeat_core::{ctdb, flacfile};

fn read_raw(p: &Path) -> Result<Vec<u32>> {
    let b = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// FLACs in a folder by track number.
fn flacs_by_track(dir: &Path) -> Result<HashMap<usize, PathBuf>> {
    let mut out = HashMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")) {
            if let Some(n) = flacfile::read_tags(&p)?.track_number() {
                out.insert(n, p);
            }
        }
    }
    Ok(out)
}

enum Source {
    Reads(usize),
    Album,
    Rejected,
}

pub fn run(album: PathBuf, reads: Option<PathBuf>, write: bool) -> Result<bool> {
    let in_album = flacs_by_track(&album)?;
    // The rejected folder: the reads' parent (it also holds the rejected FLACs).
    let rejected_dir = reads.as_ref().and_then(|r| r.parent().map(Path::to_path_buf));
    let rejected = match &rejected_dir {
        Some(d) => flacs_by_track(d)?,
        None => HashMap::new(),
    };
    let cdtoc = in_album
        .values()
        .chain(rejected.values())
        .find_map(|p| flacfile::read_tags(p).ok()?.tag("CDTOC").map(str::to_string))
        .ok_or_else(|| anyhow!("no CDTOC tag in the album's files: can't tell the disc's layout"))?;
    let toc = cdtoc::Toc::from_cdtoc(&cdtoc).map_err(|e| anyhow!("bad CDTOC {cdtoc:?}: {e}"))?;
    let offs: Vec<u32> = cdtoc.split('+').skip(1).map(|h| u32::from_str_radix(h, 16)).collect::<Result<_, _>>()?;
    let n = toc.audio_len();
    if offs.len() < n + 1 {
        bail!("CDTOC {cdtoc:?} doesn't list {n} tracks and the lead-out");
    }
    let lens: Vec<usize> = (0..n).map(|i| (offs[i + 1] - offs[i]) as usize * 588).collect();
    let ctdb_toc = ctdb::toc_string(&offs[..n], offs[n]);

    // Put the disc together.
    let mut words: Vec<u16> = Vec::with_capacity(lens.iter().sum::<usize>() * 2);
    let mut doubts: Vec<(usize, Doubt)> = Vec::new();
    let mut alts: HashMap<usize, Vec<u16>> = HashMap::new();
    let mut sources = Vec::new();
    let mut originals: Vec<Option<Vec<u32>>> = Vec::new();
    for (i, &len) in lens.iter().enumerate() {
        let t = i + 1;
        let start = words.len();
        let album_audio = match in_album.get(&t) {
            Some(p) => Some(flacfile::read_cd_audio(p)?.1),
            None => None,
        };
        let reads_dir = reads.as_ref().map(|d| d.join(format!("track{t:02}"))).filter(|d| d.is_dir());
        if let Some(dir) = reads_dir {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "raw")).collect();
            paths.sort();
            let rs: Vec<Vec<u32>> = paths.iter().map(|p| read_raw(p)).collect::<Result<_>>()?;
            if rs.is_empty() || rs.iter().any(|r| r.len() != len) {
                bail!("the reads in {} aren't track {t}'s length", dir.display());
            }
            let (w, d, a) = ctdb_repair::vote_words_alts(&rs);
            words.extend_from_slice(&w);
            for ((k, s), v) in d.into_iter().zip(a) {
                doubts.push((start + k, s));
                alts.insert(start + k, v);
            }
            sources.push(Source::Reads(rs.len()));
        } else if let Some(a) = &album_audio {
            if a.len() != len {
                bail!("track {t} in the album isn't the length the CDTOC says");
            }
            words.extend(a.iter().flat_map(|f| [*f as u16, (*f >> 16) as u16]));
            sources.push(Source::Album);
        } else if let Some(p) = rejected.get(&t) {
            let a = flacfile::read_cd_audio(p)?.1;
            if a.len() != len {
                bail!("the rejected track {t} isn't the length the CDTOC says");
            }
            words.extend(a.iter().flat_map(|f| [*f as u16, (*f >> 16) as u16]));
            sources.push(Source::Rejected);
        } else {
            bail!("track {t}: not in the album, and no saved reads or rejected copy");
        }
        originals.push(album_audio);
        let what = match sources.last().unwrap() {
            Source::Reads(k) => format!("{k} saved reads, voted; {} doubtful words", doubts.iter().filter(|(w, _)| *w >= start).count()),
            Source::Album => "the album's file".into(),
            Source::Rejected => "the rejected copy (one read)".into(),
        };
        println!("track {t:>2}: {what}");
    }

    // The entry whose parity lines up.
    let entries = ctdb::lookup(&ctdb_toc)?;
    let mut chosen = None;
    for e in entries.iter().filter(|e| e.parity_url.is_some()) {
        let npar = e.npar as usize;
        let theirs = ctdb::fetch_parity(e)?;
        let base = ctdb::column_syndromes(&words, npar);
        if let Some(k) = ctdb::find_shift(&words, &base, &theirs, npar) {
            println!("CUETools entry {} (confirmed by {}, parity {npar}) lines up at a shift of {} samples", e.id, e.confidence, k / 2);
            chosen = Some((e.clone(), theirs, k));
            break;
        }
    }
    // A damaged disc may line up with nothing exactly: then the shift where
    // the columns need the fewest corrections.
    if chosen.is_none() {
        for e in entries.iter().filter(|e| e.parity_url.is_some()) {
            let npar = e.npar as usize;
            let theirs = ctdb::fetch_parity(e)?;
            let base = ctdb::column_syndromes(&words, npar);
            if let Some(k) = ctdb_repair::find_shift_damaged(&words, &base, &theirs, npar) {
                println!(
                    "CUETools entry {} (confirmed by {}, parity {npar}) lines up at a shift of {} samples (found by fewest corrections: damage everywhere)",
                    e.id,
                    e.confidence,
                    k / 2
                );
                chosen = Some((e.clone(), theirs, k));
                break;
            }
        }
    }
    let Some((entry, theirs, k)) = chosen else { bail!("no CUETools entry's parity lines up with this disc, even allowing for damage") };
    let npar = entry.npar as usize;
    let t0 = std::time::Instant::now();
    // Repair at the entry's alignment: their word p = our word p + k.
    let len = words.len() as isize;
    let mut shifted: Vec<u16> = (0..len).map(|p| words[(p + k).clamp(0, len - 1) as usize]).collect();
    let mut sdoubts: Vec<(usize, Doubt)> = doubts.iter().filter_map(|&(w, d)| usize::try_from(w as isize - k).ok().filter(|&x| (x as isize) < len).map(|x| (x, d))).collect();
    sdoubts.sort();
    let salts: HashMap<usize, Vec<u16>> = alts.iter().filter_map(|(&w, v)| usize::try_from(w as isize - k).ok().filter(|&x| (x as isize) < len).map(|x| (x, v.clone()))).collect();
    let before_shifted = shifted.clone();
    let rep = ctdb_repair::repair_full(&mut shifted, &sdoubts, &salts, &theirs, npar, 1);
    // Back to our alignment: only words the repair changed.
    for (p, (a, b)) in shifted.iter().zip(&before_shifted).enumerate() {
        if a != b {
            let w = p as isize + k;
            if (0..len).contains(&w) {
                words[w as usize] = *a;
            }
        }
    }
    println!(
        "repair: {} columns differed, {} fixed, {} couldn't be; {} words changed ({:.1} s)",
        rep.bad_columns,
        rep.fixed_columns,
        rep.failed_columns,
        rep.fixed_words,
        t0.elapsed().as_secs_f64()
    );

    // Check each track: CUETools and AccurateRip.
    let frames: Vec<u32> = words.chunks_exact(2).map(|w| w[0] as u32 | (w[1] as u32) << 16).collect();
    let mut tracks: Vec<&[u32]> = Vec::new();
    let mut pos = 0;
    for len in &lens {
        tracks.push(&frames[pos..pos + len]);
        pos += len;
    }
    // CUETools' checksums at its alignment, AccurateRip's at ours.
    let sframes: Vec<u32> = shifted.chunks_exact(2).map(|w| w[0] as u32 | (w[1] as u32) << 16).collect();
    let mut stracks: Vec<&[u32]> = Vec::new();
    let mut spos = 0;
    for len in &lens {
        stracks.push(&sframes[spos..spos + len]);
        spos += len;
    }
    let crcs = ctdb::track_crcs(&stracks);
    let ar = accuraterip::fetch(&toc).unwrap_or(None);
    let mut good = vec![false; n];
    for i in 0..n {
        let ctdb_ok = entry.track_crcs.get(i) == Some(&crcs[i]);
        let ar_v = ar.as_ref().map(|db| db.verdict(i + 1, accuraterip::checksums(tracks[i], i == 0, i + 1 == n)));
        let ar_txt = match ar_v {
            Some(Verdict::Match { confidence, .. }) => format!("AccurateRip match ({confidence})"),
            Some(Verdict::NoMatch) => "AccurateRip: no match".into(),
            _ => "not in AccurateRip".into(),
        };
        // Write only what both agree on; AccurateRip may not know a track,
        // then a well-confirmed CUETools entry (3+) is enough.
        good[i] = ctdb_ok
            && match ar_v {
                Some(Verdict::Match { .. }) => true,
                Some(Verdict::NoMatch) => false,
                _ => entry.confidence >= 3,
            };
        let src = match sources[i] {
            Source::Reads(_) => " (from saved reads)",
            Source::Album => "",
            Source::Rejected => " (from the rejected copy)",
        };
        println!(
            "  track {:>2}: CUETools {}, {ar_txt}{src}{}",
            i + 1,
            if ctdb_ok { "matches" } else { "DIFFERS" },
            if good[i] { "  -> verified" } else { "" }
        );
    }
    let whole = ctdb::disc_crc(&stracks) == entry.crc32;
    println!("whole disc: {}", if whole { "matches CUETools" } else { "differs from CUETools" });

    // Write verified tracks that differ from the album (or are missing).
    let to_write: Vec<usize> = (0..n).filter(|&i| good[i] && originals[i].as_deref() != Some(tracks[i])).collect();
    if to_write.is_empty() {
        println!("nothing to write");
    } else if !write {
        println!("would write: track(s) {} (run again with --write)", to_write.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", "));
    } else {
        let settings = localbeat_core::config::Settings::load();
        let note = format!(
            "CUETools DB entry {}, confirmed by {} (LocalBeat Studio, {})",
            entry.id,
            entry.confidence,
            chrono::Local::now().format("%Y-%m-%d")
        );
        let backup = settings.cache_dir.join("repaired-originals").join(album.file_name().unwrap_or_default());
        std::fs::create_dir_all(&backup)?;
        for i in to_write {
            let t = i + 1;
            let (dest, tag_source) = match (in_album.get(&t), rejected.get(&t)) {
                (Some(p), _) => (p.clone(), p.clone()),
                (None, Some(r)) => (album.join(r.file_name().context("file name")?), r.clone()),
                (None, None) => {
                    println!("  track {t}: no file to take the name and tags from; skipped");
                    continue;
                }
            };
            write_track(&dest, &tag_source, tracks[i], &note, &backup, settings.compression)?;
            println!("  wrote track {t}: {}", dest.file_name().unwrap_or_default().to_string_lossy());
        }
        println!("originals (if any) kept in {}", backup.display());
    }
    Ok(whole)
}

/// Writes `frames` to `dest` with `tag_source`'s tags (minus
/// ACCURATERIP=unverified, plus a CTDBREPAIR note) and front cover. The new
/// file is decoded and compared before an existing `dest` is moved to
/// `backup`.
fn write_track(dest: &Path, tag_source: &Path, frames: &[u32], note: &str, backup: &Path, compression: u32) -> Result<()> {
    use localbeat_core::flac_encode::{self, Picture};
    let mut tags = flacfile::read_tags(tag_source)?.tags;
    tags.retain(|(k, _)| !k.eq_ignore_ascii_case("ACCURATERIP") && !k.eq_ignore_ascii_case("CTDBREPAIR"));
    tags.push(("CTDBREPAIR".into(), note.into()));
    let cover = flacfile::read_pictures(tag_source)?.into_iter().find(|p| p.kind == 3).map(|p| {
        let (w, h) = imagesize::blob_size(&p.data).map(|s| (s.width as u32, s.height as u32)).unwrap_or((0, 0));
        Picture { mime: p.mime, data: p.data, width: w, height: h }
    });
    let name = dest.file_name().context("file name")?.to_string_lossy().into_owned();
    let tmp = dest.with_file_name(format!("{name}.part"));
    flac_encode::write_flac(&tmp, frames, &tags, cover.as_ref(), compression)?;
    // The file on disk must decode to exactly these samples.
    let (_, back) = flacfile::read_cd_audio(&tmp)?;
    if back != frames {
        let _ = std::fs::remove_file(&tmp);
        bail!("{name}: the written file doesn't decode to the repaired audio; nothing replaced");
    }
    if dest.exists() {
        let mut keep = backup.join(&name);
        let mut k = 1;
        while keep.exists() {
            k += 1;
            keep = backup.join(format!("{name}.{k}"));
        }
        std::fs::rename(dest, &keep)
            .or_else(|_| std::fs::copy(dest, &keep).map(|_| ()).and_then(|_| std::fs::remove_file(dest)))
            .with_context(|| format!("moving the original to {}", keep.display()))?;
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("putting {name} in place"))?;
    Ok(())
}
