//! `ripshelf enrich`: tags and covers for the Miscellaneous library from
//! MusicBrainz. Sure matches are written (old tags saved for --undo);
//! the rest are listed for review.

use anyhow::Result;
use ripshelf_core::config::Settings;
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
    let mut state = State::load(&s.config_dir);
    let wanted = |rel: &str| a.patterns.is_empty() || a.patterns.iter().any(|p| rel.to_lowercase().contains(&p.to_lowercase()));

    if a.undo {
        let mut n = 0;
        for rel in state.albums.keys().cloned().collect::<Vec<_>>() {
            if !wanted(&rel) || state.status(&rel) != Some("applied") {
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
        .filter(|rel| a.retry || !matches!(state.status(rel), Some("applied" | "nomatch" | "mixed")))
        .take(a.limit.unwrap_or(usize::MAX))
        .collect();
    println!("{} Miscellaneous albums to look up{}", albums.len(), if a.dry_run { " (dry run: nothing written)" } else { "" });

    let (mut applied, mut review, mut nomatch, mut mixed) = (0, 0, 0, 0);
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
        // Search by the tags, then by the folder names if those differ.
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
        let record = enrich::apply(&local, &c, &m, &genres, cover.as_deref())?;
        let f = backup_file(&s, &rel);
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(&f, serde_json::to_vec_pretty(&record)?)?;
        println!(
            "      written: {} files{}{}",
            m.mapping.iter().filter(|x| x.is_some()).count(),
            if cover.is_some() { ", cover" } else { "" },
            genres.first().map(|g| format!(", genre {g}")).unwrap_or_default()
        );
        state.albums.insert(
            rel.clone(),
            json!({"status": "applied", "release": c.release_id, "summary": c.summary(), "score": m.score, "when": today()}),
        );
        applied += 1;
        state.save()?;
    }
    println!(
        "Done: {applied} {}, {review} to review, {nomatch} not found, {mixed} mixed folders",
        if a.dry_run { "sure (not written)" } else { "written" }
    );
    Ok(true)
}
