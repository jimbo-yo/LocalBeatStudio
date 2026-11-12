//! Repairing a ripped disc's missing tracks with the CUETools database, as
//! used by the rip (after a track is rejected) and `localbeat repair`.
//!
//! The disc is put together from the best source for each track: its saved
//! reads (voted; their disagreements become erasures), else the album's
//! FLAC (verified), else the rejected FLAC (one read). The CTDB entry is the
//! one whose parity lines up (exactly, or for a damaged disc by fewest
//! corrections), the repair runs at that entry's alignment, and a track is
//! only written if CUETools AND AccurateRip vouch for it (or, when
//! AccurateRip doesn't know the track, a CTDB entry confirmed 3+ times).
//! See docs/ctdb-repair.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::accuraterip::{self, Verdict};
use crate::config::Settings;
use crate::ctdb_repair::{self, Doubt, RepairReport};
use crate::{ctdb, flac_encode, flacfile};

/// Where a track's audio came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// Saved reads (how many), voted.
    Reads(usize),
    /// The album's (verified) FLAC.
    Album,
    /// The rejected FLAC: one read.
    Rejected,
}

#[derive(Debug, Clone)]
pub struct TrackResult {
    pub position: usize,
    pub source: Source,
    pub ctdb_match: bool,
    pub accuraterip: Option<Verdict>,
    /// CUETools and AccurateRip vouch for it.
    pub verified: bool,
    /// Written to the album (repaired, or added where it was missing).
    pub written: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct DiscRepair {
    /// (entry id, confirmations, npar, shift in samples).
    pub entry: Option<(u64, u32, u32, isize)>,
    pub report: RepairReport,
    pub tracks: Vec<TrackResult>,
    pub whole_disc: bool,
    /// Lines for a log / the user.
    pub log: Vec<String>,
}

impl DiscRepair {
    /// Tracks still not in the album after this (missing or unverified).
    pub fn still_missing(&self) -> Vec<usize> {
        self.tracks.iter().filter(|t| t.source != Source::Album && t.written.is_none()).map(|t| t.position).collect()
    }
}

fn read_raw(p: &Path) -> Result<Vec<u32>> {
    let b = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    Ok(b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// FLACs of this disc (same CDTOC tag) by track number.
fn flacs_by_track(dir: &Path, cdtoc: &str) -> Result<HashMap<usize, PathBuf>> {
    let mut out = HashMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")) {
            continue;
        }
        let f = flacfile::read_tags(&p)?;
        if f.tag("CDTOC").is_some_and(|t| t.eq_ignore_ascii_case(cdtoc)) {
            if let Some(n) = f.track_number() {
                out.insert(n, p);
            }
        }
    }
    Ok(out)
}

/// The CDTOC tags found in a folder's FLACs (one per disc).
pub fn cdtocs_in(dir: &Path) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")) {
            if let Some(t) = flacfile::read_tags(&p)?.tag("CDTOC") {
                if !out.iter().any(|o| o.eq_ignore_ascii_case(t)) {
                    out.push(t.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Repairs the disc `cdtoc` of `album`. `rejected` = the disc's rejected
/// folder (its FLACs, and `reads/trackNN/readK.raw`). With `write`, verified
/// tracks that differ from (or are missing in) the album are written, the
/// originals kept in `<cache>/repaired-originals/`, and the written tracks'
/// reads and rejected copies removed.
pub fn repair_disc(s: &Settings, album: &Path, cdtoc: &str, rejected: Option<&Path>, write: bool) -> Result<DiscRepair> {
    let mut out = DiscRepair::default();
    let in_album = flacs_by_track(album, cdtoc)?;
    let rejected_flacs = match rejected {
        Some(d) => flacs_by_track(d, cdtoc)?,
        None => HashMap::new(),
    };
    let reads = rejected.map(|d| d.join("reads"));
    let toc = cdtoc::Toc::from_cdtoc(cdtoc).map_err(|e| anyhow!("bad CDTOC {cdtoc:?}: {e}"))?;
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
        // A track in the album was verified when ripped, unless it says
        // otherwise (a hand repair: ACCURATERIP=unverified); then its saved
        // reads are the better source.
        let album_verified = match in_album.get(&t) {
            Some(p) => !flacfile::read_tags(p)?.tag("ACCURATERIP").is_some_and(|v| v.eq_ignore_ascii_case("unverified")),
            None => false,
        };
        let reads_dir = reads.as_ref().map(|d| d.join(format!("track{t:02}"))).filter(|d| d.is_dir());
        if let (Some(dir), false) = (&reads_dir, album_verified) {
            let mut paths: Vec<PathBuf> =
                std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "raw")).collect();
            paths.sort();
            let rs: Vec<Vec<u32>> = paths.iter().map(|p| read_raw(p)).collect::<Result<_>>()?;
            if rs.is_empty() || rs.iter().any(|r| r.len() != len) {
                bail!("the reads in {} aren't track {t}'s length", dir.display());
            }
            let (w, d, a) = ctdb_repair::vote_words_alts(&rs);
            words.extend_from_slice(&w);
            let n_doubt = d.len();
            for ((k, s), v) in d.into_iter().zip(a) {
                doubts.push((start + k, s));
                alts.insert(start + k, v);
            }
            out.log.push(format!("track {t}: {} saved reads, {n_doubt} doubtful words", rs.len()));
            sources.push(Source::Reads(rs.len()));
        } else if let Some(a) = &album_audio {
            if a.len() != len {
                bail!("track {t} in the album isn't the length the CDTOC says");
            }
            words.extend(a.iter().flat_map(|f| [*f as u16, (*f >> 16) as u16]));
            sources.push(Source::Album);
        } else if let Some(p) = rejected_flacs.get(&t) {
            let a = flacfile::read_cd_audio(p)?.1;
            if a.len() != len {
                bail!("the rejected track {t} isn't the length the CDTOC says");
            }
            words.extend(a.iter().flat_map(|f| [*f as u16, (*f >> 16) as u16]));
            out.log.push(format!("track {t}: only the rejected copy (one read)"));
            sources.push(Source::Rejected);
        } else {
            bail!("track {t}: not in the album, and no saved reads or rejected copy");
        }
        originals.push(album_audio);
    }

    // The entry whose parity lines up: exactly, else by fewest corrections.
    let entries = ctdb::lookup(&ctdb_toc)?;
    let with_parity: Vec<&ctdb::Entry> = entries.iter().filter(|e| e.parity_url.is_some()).collect();
    if with_parity.is_empty() {
        out.log.push(if entries.is_empty() { "this pressing isn't in the CUETools database".into() } else { "the CUETools database has no repair data for this pressing".into() });
        return Ok(out);
    }
    let mut chosen = None;
    let mut fetched = Vec::new();
    for e in &with_parity {
        let npar = e.npar as usize;
        let theirs = ctdb::fetch_parity(e)?;
        let base = ctdb::column_syndromes(&words, npar);
        if let Some(k) = ctdb::find_shift(&words, &base, &theirs, npar) {
            chosen = Some(((*e).clone(), theirs, k, false));
            break;
        }
        fetched.push(((*e).clone(), theirs, base));
    }
    if chosen.is_none() {
        for (e, theirs, base) in fetched {
            if let Some(k) = ctdb_repair::find_shift_damaged(&words, &base, &theirs, e.npar as usize) {
                chosen = Some((e, theirs, k, true));
                break;
            }
        }
    }
    let Some((entry, theirs, k, damaged)) = chosen else {
        out.log.push("no CUETools entry's repair data lines up with this disc".into());
        return Ok(out);
    };
    let npar = entry.npar as usize;
    out.entry = Some((entry.id, entry.confidence, entry.npar, k / 2));
    out.log.push(format!(
        "CUETools entry {} (confirmed by {}, parity {npar}) lines up at {} samples{}",
        entry.id,
        entry.confidence,
        k / 2,
        if damaged { " (found by fewest corrections)" } else { "" }
    ));

    // Repair at the entry's alignment (their word p = our word p + k).
    let len = words.len() as isize;
    let mut shifted: Vec<u16> = (0..len).map(|p| words[(p + k).clamp(0, len - 1) as usize]).collect();
    let mut sdoubts: Vec<(usize, Doubt)> =
        doubts.iter().filter_map(|&(w, d)| usize::try_from(w as isize - k).ok().filter(|&x| (x as isize) < len).map(|x| (x, d))).collect();
    sdoubts.sort();
    let salts: HashMap<usize, Vec<u16>> =
        alts.iter().filter_map(|(&w, v)| usize::try_from(w as isize - k).ok().filter(|&x| (x as isize) < len).map(|x| (x, v.clone()))).collect();
    let before = shifted.clone();
    out.report = ctdb_repair::repair_full(&mut shifted, &sdoubts, &salts, &theirs, npar, 1);
    for (p, (a, b)) in shifted.iter().zip(&before).enumerate() {
        if a != b {
            let w = p as isize + k;
            if (0..len).contains(&w) {
                words[w as usize] = *a;
            }
        }
    }
    out.log.push(format!(
        "repair: {} damaged columns, {} fixed, {} beyond reach; {} words corrected",
        out.report.bad_columns, out.report.fixed_columns, out.report.failed_columns, out.report.fixed_words
    ));

    // Check each track: CUETools at its alignment, AccurateRip at ours.
    let split = |w: &[u16]| -> Vec<Vec<u32>> {
        let frames: Vec<u32> = w.chunks_exact(2).map(|x| x[0] as u32 | (x[1] as u32) << 16).collect();
        let mut v = Vec::new();
        let mut pos = 0;
        for l in &lens {
            v.push(frames[pos..pos + l].to_vec());
            pos += l;
        }
        v
    };
    let ours = split(&words);
    let theirs_tracks = split(&shifted);
    let refs: Vec<&[u32]> = theirs_tracks.iter().map(Vec::as_slice).collect();
    let crcs = ctdb::track_crcs(&refs);
    out.whole_disc = ctdb::disc_crc(&refs) == entry.crc32;
    let ar = accuraterip::fetch(&toc).unwrap_or(None);
    for i in 0..n {
        let ctdb_match = entry.track_crcs.get(i) == Some(&crcs[i]);
        let v = ar.as_ref().map(|db| db.verdict(i + 1, accuraterip::checksums(&ours[i], i == 0, i + 1 == n)));
        let verified = ctdb_match
            && match v {
                Some(Verdict::Match { .. }) => true,
                Some(Verdict::NoMatch) => false,
                _ => entry.confidence >= 3,
            };
        out.tracks.push(TrackResult { position: i + 1, source: sources[i], ctdb_match, accuraterip: v, verified, written: None });
    }

    if write {
        let note = format!(
            "CUETools DB entry {}, confirmed by {} (LocalBeat Studio, {})",
            entry.id,
            entry.confidence,
            chrono::Local::now().format("%Y-%m-%d")
        );
        let backup = s.cache_dir.join("repaired-originals").join(album.file_name().unwrap_or_default());
        for i in 0..n {
            let t = i + 1;
            if !out.tracks[i].verified || originals[i].as_deref() == Some(ours[i].as_slice()) {
                continue;
            }
            let (dest, tag_source) = match (in_album.get(&t), rejected_flacs.get(&t)) {
                (Some(p), _) => (p.clone(), p.clone()),
                (None, Some(r)) => (album.join(r.file_name().context("file name")?), r.clone()),
                (None, None) => continue,
            };
            std::fs::create_dir_all(&backup)?;
            write_track(&dest, &tag_source, &ours[i], &note, &backup, s.compression)?;
            out.log.push(format!("track {t}: repaired and written"));
            out.tracks[i].written = Some(dest);
            // Its reads and rejected copy did their job.
            if let Some(d) = reads.as_ref().map(|d| d.join(format!("track{t:02}"))) {
                let _ = std::fs::remove_dir_all(d);
            }
            if let Some(r) = rejected_flacs.get(&t) {
                let _ = std::fs::remove_file(r);
            }
        }
    }
    Ok(out)
}

/// Writes `frames` to `dest` with `tag_source`'s tags (minus ACCURATERIP,
/// plus a CTDBREPAIR note) and front cover. The new file is decoded and
/// compared before an existing `dest` is moved to `backup`.
fn write_track(dest: &Path, tag_source: &Path, frames: &[u32], note: &str, backup: &Path, compression: u32) -> Result<()> {
    let mut tags = flacfile::read_tags(tag_source)?.tags;
    tags.retain(|(k, _)| !k.eq_ignore_ascii_case("ACCURATERIP") && !k.eq_ignore_ascii_case("CTDBREPAIR"));
    tags.push(("CTDBREPAIR".into(), note.into()));
    let cover = flacfile::read_pictures(tag_source)?.into_iter().find(|p| p.kind == 3).map(|p| {
        let (w, h) = imagesize::blob_size(&p.data).map(|s| (s.width as u32, s.height as u32)).unwrap_or((0, 0));
        flac_encode::Picture { mime: p.mime, data: p.data, width: w, height: h }
    });
    let name = dest.file_name().context("file name")?.to_string_lossy().into_owned();
    let tmp = dest.with_file_name(format!("{name}.part"));
    flac_encode::write_flac(&tmp, frames, &tags, cover.as_ref(), compression)?;
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
