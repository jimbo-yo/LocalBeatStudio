//! `ripshelf enrich`: tags and covers for the Miscellaneous library from
//! MusicBrainz. Sure matches are written (old tags saved for --undo);
//! the rest are listed for review.

use anyhow::Result;
use ripshelf_core::config::{Prefs, Settings};
use ripshelf_core::enrich::{self, State};
use ripshelf_core::musicbrainz::{self, Release};
use ripshelf_core::phone_sync::{self, Origin};
use serde_json::json;

pub struct EnrichArgs {
    pub patterns: Vec<String>,
    pub dry_run: bool,
    pub limit: Option<usize>,
    pub retry: bool,
    pub undo: bool,
    /// Identify the albums name search couldn't, by sound (AcoustID).
    pub sound: bool,
    pub acoustid_key: Option<String>,
}

/// Merges into the album's undo record: the first saved old values of a
/// file are its original ones, so they're never replaced.
fn merge_backup(s: &Settings, rel: &str, new: &serde_json::Value) -> Result<()> {
    let f = backup_file(s, rel);
    let mut record = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in new.as_object().cloned().unwrap_or_default() {
        record.entry(k).or_insert(v);
    }
    if !record.is_empty() {
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(&f, serde_json::to_vec_pretty(&record)?)?;
    }
    Ok(())
}

fn written_count(record: &serde_json::Value) -> usize {
    record.as_object().map(|o| o.keys().filter(|k| !k.starts_with('_')).count()).unwrap_or(0)
}

fn backup_file(s: &Settings, rel: &str) -> std::path::PathBuf {
    s.config_dir.join("enrich").join("backups").join(format!("{}.json", rel.replace('/', "__")))
}

fn today() -> String {
    // Seconds since 1970 are enough to order runs; the date is for people.
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

pub fn enrich(a: EnrichArgs) -> Result<bool> {
    let s = Settings::load();
    let mut prefs = Prefs::load(&s);
    if let Some(k) = &a.acoustid_key {
        prefs.acoustid_key = k.trim().to_string();
        prefs.save(&s)?;
        println!("AcoustID key saved in {}", s.config_dir.join("ripshelf.json").display());
        if !a.sound {
            return Ok(true);
        }
    }
    let mut state = State::load(&s.config_dir);
    if a.sound {
        if prefs.acoustid_key.is_empty() {
            anyhow::bail!("no AcoustID key yet: get one at https://acoustid.org/new-application, then: ripshelf enrich --acoustid-key KEY");
        }
        return sound_pass(&s, &mut state, &a, &prefs.acoustid_key);
    }
    let wanted = |rel: &str| a.patterns.is_empty() || a.patterns.iter().any(|p| rel.to_lowercase().contains(&p.to_lowercase()));

    if a.undo {
        let mut n = 0;
        for rel in state.albums.keys().cloned().collect::<Vec<_>>() {
            if !wanted(&rel) || !matches!(state.status(&rel), Some("applied" | "partial")) {
                continue;
            }
            let f = backup_file(&s, &rel);
            let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&f)?)?;
            let files = enrich::undo(&record)?;
            println!("  undone: {rel} ({files} files)");
            state.albums.remove(&rel);
            let _ = std::fs::remove_file(&f);
            n += 1;
        }
        state.save()?;
        println!("{n} albums put back as they were.");
        return Ok(true);
    }

    let albums: Vec<String> = phone_sync::scan_library(&s.flac_dir, &s.misc_dir)
        .into_iter()
        .filter(|x| x.origin == Origin::Misc && x.track_count() > 0)
        .map(|x| x.rel)
        .filter(|rel| wanted(rel))
        .filter(|rel| a.retry || !matches!(state.status(rel), Some("applied" | "nomatch" | "mixed" | "review")))
        .take(a.limit.unwrap_or(usize::MAX))
        .collect();
    println!("{} Miscellaneous albums to look up{}", albums.len(), if a.dry_run { " (dry run: nothing written)" } else { "" });

    let (mut applied, mut review, mut nomatch, mut mixed, mut errors) = (0, 0, 0, 0, 0);
    for rel in albums {
        let local = match enrich::read_album(&s.misc_dir.join(&rel), &s.misc_dir) {
            Ok(l) => l,
            Err(e) => {
                println!("  ✘ {rel}: {e:#}");
                continue;
            }
        };
        let unnamed = |a: &str| matches!(enrich::norm(a).as_str(), "" | "unknown album" | "untitled" | "unknown");
        if local.mixed || (unnamed(&local.album) && unnamed(&phone_sync::split_album(&rel).1)) {
            let why = if local.mixed { "songs from several albums" } else { "no album name" };
            println!("  ~ {rel}: {why} (left for matching by sound)");
            state.albums.insert(rel.clone(), json!({"status": "mixed", "why": why}));
            mixed += 1;
            state.save()?;
            continue;
        }
        // Search by the tags, then by the folder names if those differ. A
        // network/MusicBrainz problem only skips this album (retried next run).
        let lookup = || -> Result<Vec<enrich::Candidate>> {
            let (fa, ft) = phone_sync::split_album(&rel);
            let mut ids = enrich::search(&local.album, &local.artist)?;
            if ids.is_empty() && (ft != local.album || fa != local.artist) {
                ids = enrich::search(&ft, &fa)?;
            }
            let mut cands = Vec::new();
            for id in ids.iter().take(3) {
                if let Some(c) = enrich::fetch_candidate(id)? {
                    cands.push(c);
                }
            }
            Ok(cands)
        };
        let cands = match lookup() {
            Ok(c) => c,
            Err(e) => {
                println!("  ✘ {rel}: lookup failed ({e:#}); will be tried again next run");
                errors += 1;
                continue;
            }
        };
        let Some((c, m)) = enrich::best(&local, &cands, s.country.as_deref()).filter(|(_, m)| m.score >= 0.5) else {
            println!("  ✘ {rel}: not found on MusicBrainz");
            state.albums.insert(rel.clone(), json!({"status": "nomatch", "when": today()}));
            nomatch += 1;
            state.save()?;
            continue;
        };
        let c = c.clone();
        if !m.sure {
            println!("  ? {rel}\n      best: {} (score {:.2})\n      unsure: {}", c.summary(), m.score, m.why_not_sure);
            state.albums.insert(
                rel.clone(),
                json!({"status": "review", "release": c.release_id, "summary": c.summary(),
                       "score": m.score, "why": m.why_not_sure, "candidates": cands.iter().map(|c| &c.release_id).collect::<Vec<_>>(),
                       "when": today()}),
            );
            review += 1;
            state.save()?;
            continue;
        }
        println!("  ✔ {rel}\n      = {} (score {:.2})", c.summary(), m.score);
        if a.dry_run {
            applied += 1;
            continue;
        }
        let release = Release {
            id: c.release_id.clone(),
            release_group_id: c.release_group_id.clone(),
            artist_ids: c.artist_ids.clone(),
            ..Default::default()
        };
        let genres = musicbrainz::genres(&release).unwrap_or_default();
        let cover = if local.tracks.iter().all(|t| t.has_cover) {
            None
        } else {
            ripshelf_core::cover::fetch_front(&release).ok().flatten().map(|p| p.data)
        };
        let done = enrich::apply(&local, &c, &m, &genres, cover.as_deref(), true);
        merge_backup(&s, &rel, &done.record)?;
        let written = written_count(&done.record);
        println!(
            "      written: {written} files{}{}",
            if cover.is_some() { ", cover" } else { "" },
            genres.first().map(|g| format!(", genre {g}")).unwrap_or_default()
        );
        for (file, e) in &done.failed {
            println!("      ✘ couldn't write {file}: {e}");
        }
        let status = if done.failed.is_empty() { "applied" } else { "partial" };
        state.albums.insert(
            rel.clone(),
            json!({"status": status, "release": c.release_id, "summary": c.summary(), "score": m.score,
                   "failed": done.failed.iter().map(|(f, e)| json!({"file": f, "error": e})).collect::<Vec<_>>(),
                   "when": today()}),
        );
        applied += 1;
        if !done.failed.is_empty() {
            errors += 1;
        }
        state.save()?;
    }
    println!(
        "Done: {applied} {}, {review} to review, {nomatch} not found, {mixed} for matching by sound{}",
        if a.dry_run { "sure (not written)" } else { "written" },
        if errors > 0 { format!(", {errors} with problems (see above)") } else { String::new() }
    );
    Ok(true)
}

/// Albums name search couldn't settle (not found, mixed/loose, review),
/// identified by sound. An album folder whose songs all sit on one release
/// is written as an album; otherwise each identified song gets its own
/// release (its tagged album if that has it, else its original album), with
/// the cover embedded but no cover.jpg (the folder isn't one album).
fn sound_pass(s: &Settings, state: &mut State, a: &EnrichArgs, key: &str) -> Result<bool> {
    let wanted = |rel: &str| a.patterns.is_empty() || a.patterns.iter().any(|p| rel.to_lowercase().contains(&p.to_lowercase()));
    let albums: Vec<String> = phone_sync::scan_library(&s.flac_dir, &s.misc_dir)
        .into_iter()
        .filter(|x| x.origin == Origin::Misc && x.track_count() > 0)
        .map(|x| x.rel)
        .filter(|rel| wanted(rel))
        .filter(|rel| match state.status(rel) {
            Some("nomatch" | "mixed" | "review" | "partial") => true,
            Some("sound_nomatch" | "sound_partial") => a.retry,
            Some(_) => false,
            None => a.retry,
        })
        .take(a.limit.unwrap_or(usize::MAX))
        .collect();
    println!("{} albums to identify by sound{}", albums.len(), if a.dry_run { " (dry run: nothing written)" } else { "" });
    let mut cands: std::collections::BTreeMap<String, Option<enrich::Candidate>> = Default::default();
    let mut extras: std::collections::BTreeMap<String, (Vec<String>, Option<Vec<u8>>)> = Default::default();
    let (mut albums_done, mut songs_done, mut none) = (0, 0, 0);
    for rel in albums {
        let Ok(local) = enrich::read_album(&s.misc_dir.join(&rel), &s.misc_dir) else { continue };
        let hits = enrich::identify(&local, key);
        let known = hits.iter().filter(|h| !h.is_empty()).count();
        if known == 0 {
            println!("  ✘ {rel}: none of its {} songs identified", local.tracks.len());
            state.albums.insert(rel.clone(), json!({"status": "sound_nomatch", "when": today()}));
            none += 1;
            state.save()?;
            continue;
        }
        let mut fetch = |id: &str| -> Option<enrich::Candidate> {
            cands.entry(id.to_string()).or_insert_with(|| enrich::fetch_candidate(id).ok().flatten()).clone()
        };
        let mut extras_for = |c: &enrich::Candidate| -> (Vec<String>, Option<Vec<u8>>) {
            extras
                .entry(c.release_id.clone())
                .or_insert_with(|| {
                    let r = Release { id: c.release_id.clone(), release_group_id: c.release_group_id.clone(), artist_ids: c.artist_ids.clone(), ..Default::default() };
                    (musicbrainz::genres(&r).unwrap_or_default(), ripshelf_core::cover::fetch_front(&r).ok().flatten().map(|p| p.data))
                })
                .clone()
        };

        // As one album, if the folder looks like one.
        let unnamed = |x: &str| matches!(enrich::norm(x).as_str(), "" | "unknown album" | "untitled" | "unknown");
        let album_folder = rel.contains('/') && !local.mixed && !(unnamed(&local.album) && unnamed(&phone_sync::split_album(&rel).1));
        if album_folder {
            let best = enrich::releases_by_sound(&hits)
                .iter()
                .take(3)
                .filter_map(|id| fetch(id))
                .map(|c| {
                    let m = enrich::score_by_sound(&local, &c, &hits);
                    (c, m)
                })
                .max_by(|a, b| (a.1.sure, a.1.score).partial_cmp(&(b.1.sure, b.1.score)).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((c, m)) = best.filter(|(_, m)| m.sure) {
                println!("  ✔ {rel}\n      = {} (by sound, {:.0}%)", c.summary(), m.score * 100.0);
                if !a.dry_run {
                    let (genres, cover) = extras_for(&c);
                    let done = enrich::apply(&local, &c, &m, &genres, cover.as_deref(), true);
                    merge_backup(s, &rel, &done.record)?;
                    for (f, e) in &done.failed {
                        println!("      ✘ couldn't write {f}: {e}");
                    }
                    state.albums.insert(rel.clone(), json!({"status": if done.failed.is_empty() { "applied" } else { "partial" },
                        "by": "sound", "release": c.release_id, "summary": c.summary(), "score": m.score, "when": today()}));
                    state.save()?;
                }
                albums_done += 1;
                continue;
            }
        }

        // Song by song.
        let mut written = 0;
        for (t, th) in local.tracks.iter().zip(&hits) {
            let one = enrich::LocalAlbum { tracks: vec![t.clone()], ..local.clone() };
            // The first of its best few releases that really has the song.
            let found = enrich::song_releases(th, &t.album).iter().take(3).find_map(|id| {
                let c = fetch(id)?;
                let m = enrich::score_by_sound(&one, &c, std::slice::from_ref(th));
                m.sure.then_some((c, m))
            });
            let Some((c, m)) = found else { continue };
            println!("  ♪ {rel}/{}\n      = {} — {}", t.path.file_name().unwrap_or_default().to_string_lossy(), c.tracks[m.mapping[0].unwrap()].title, c.summary());
            if !a.dry_run {
                let (genres, cover) = extras_for(&c);
                let done = enrich::apply(&one, &c, &m, &genres, cover.as_deref(), false);
                merge_backup(s, &rel, &done.record)?;
                written += written_count(&done.record);
            } else {
                written += 1;
            }
        }
        songs_done += written;
        let status = if written == 0 {
            none += 1;
            println!("  ✘ {rel}: {known} of {} songs identified, but none confidently", local.tracks.len());
            "sound_nomatch"
        } else if written == local.tracks.len() {
            "applied"
        } else {
            println!("      ({written} of {} songs written)", local.tracks.len());
            "sound_partial"
        };
        if !a.dry_run || status == "sound_nomatch" {
            state.albums.insert(rel.clone(), json!({"status": status, "by": "sound", "songs": written, "when": today()}));
            state.save()?;
        }
    }
    println!(
        "Done: {albums_done} albums {w} whole, {songs_done} loose songs {w}, {none} folders not identified",
        w = if a.dry_run { "would be written" } else { "written" }
    );
    Ok(true)
}
