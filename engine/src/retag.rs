//! Applying AI-enriched genres and moods to audio files: locally (for offline
//! users and local copies) and on the LocalBeat server in place over SSH
//! (`localbeat-retag` via `localbeat-writer-session`, followed by `startScan`).
//!
//! Saves undo backups in `<config>/ai-enrich/backups/<album_id>.json` so any
//! album can be restored to its previous GENRE/MOOD tags.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use localbeat_core::ai_enrich::{self, OldFileTags, PendingStore, Proposal};
use localbeat_core::config::{ServerPrefs, Settings};
use localbeat_core::library::{Library, Origin, Song};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::push::{self, Rsync};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub album_id: String,
    pub artist: String,
    pub album: String,
    /// Local file path -> previous tags.
    pub local: BTreeMap<String, OldFileTags>,
    /// Server-relative paths that were updated, with the album's previous tags.
    pub server_paths: Vec<String>,
    pub server_old: OldFileTags,
}

#[derive(Debug, Default, Clone)]
pub struct ApplyReport {
    pub albums_applied: Vec<String>,
    pub local_updated: usize,
    pub server_updated: usize,
    pub notes: Vec<String>,
}

impl ApplyReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.local_updated > 0 {
            parts.push(format!("{} local file(s)", self.local_updated));
        }
        if self.server_updated > 0 {
            parts.push(format!("{} server file(s)", self.server_updated));
        }
        if parts.is_empty() {
            "No files updated".into()
        } else {
            format!("Updated {} across {} album(s)", parts.join(" and "), self.albums_applied.len())
        }
    }
}

fn rel_to(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty() && !parts.iter().any(|p| p == ".." || p == ".")).then(|| parts.join("/"))
}

struct AlbumTargets {
    local: Vec<PathBuf>,
    server: Vec<String>,
}

fn targets_for(s: &Settings, songs: &[&Song]) -> AlbumTargets {
    let mut local = Vec::new();
    let mut server = BTreeSet::new();
    for x in songs {
        match x.origin {
            Origin::Server => {
                if let Some(rel) = &x.server_path {
                    server.insert(rel.clone());
                }
            }
            Origin::Flac | Origin::Misc => {
                if x.path.is_file() {
                    local.push(x.path.clone());
                }
                let root = if x.origin == Origin::Flac { &s.flac_dir } else { &s.misc_dir };
                if let Some(rel) = rel_to(&x.path, root) {
                    server.insert(rel);
                }
            }
        }
    }
    AlbumTargets { local, server: server.into_iter().collect() }
}

/// Applies `proposals` to both local files and the server (when an admin writer
/// key is configured). Returns which album IDs succeeded so the caller can drop
/// them from `PendingStore`.
pub fn apply_proposals(
    s: &Settings,
    sp: &ServerPrefs,
    lib: &Library,
    proposals: &[Proposal],
    taxonomy_version: &str,
) -> Result<ApplyReport> {
    if proposals.is_empty() {
        return Ok(ApplyReport::default());
    }
    let by_key: BTreeMap<&str, &localbeat_core::library::Album> = lib.albums.iter().map(|a| (a.key.as_str(), a)).collect();

    struct PendingServer<'a> {
        proposal: &'a Proposal,
        genres: Vec<String>,
        moods: Vec<String>,
        paths: Vec<String>,
    }

    let mut report = ApplyReport::default();
    let mut server_batch: Vec<PendingServer> = Vec::new();
    let mut backups: BTreeMap<String, UndoRecord> = BTreeMap::new();
    let mut touched_local: Vec<PathBuf> = Vec::new();

    // Check upfront if any proposal needs the server.
    let mut per_album: Vec<(&Proposal, AlbumTargets, Vec<String>, Vec<String>)> = Vec::new();
    for p in proposals {
        let Some(album) = by_key.get(p.album_key.as_str()) else {
            report.notes.push(format!("{} — {}: not in the current library", p.artist, p.album));
            continue;
        };
        let songs: Vec<&Song> = album.songs.iter().map(|&i| &lib.songs[i]).collect();
        let targets = targets_for(s, &songs);
        let genres = p.final_genres();
        let moods = p.moods.clone();
        if targets.local.is_empty() && targets.server.is_empty() {
            report.notes.push(format!("{} — {}: no writable files found", p.artist, p.album));
            continue;
        }
        if targets.local.is_empty() && !targets.server.is_empty() && !sp.enabled() {
            report.notes.push(format!(
                "{} — {}: only on the server (retagging server albums needs an admin writer key)",
                p.artist, p.album
            ));
            continue;
        }
        per_album.push((p, targets, genres, moods));
    }

    // Server first (like `remove.rs`): if we're away from home and an album only
    // lives on the server, fail cleanly before touching local copies.
    let any_server = sp.enabled() && per_album.iter().any(|(_, t, _, _)| !t.server.is_empty());
    if any_server {
        for (p, targets, genres, moods) in &per_album {
            if !targets.server.is_empty() {
                server_batch.push(PendingServer {
                    proposal: p,
                    genres: genres.clone(),
                    moods: moods.clone(),
                    paths: targets.server.clone(),
                });
            }
        }
    }

    let mut server_ok: BTreeSet<String> = BTreeSet::new();
    if !server_batch.is_empty() {
        let lines: Vec<String> = server_batch
            .iter()
            .map(|b| {
                json!({
                    "paths": b.paths,
                    "genres": b.genres,
                    "moods": b.moods,
                    "ai_enriched": taxonomy_version,
                })
                .to_string()
            })
            .collect();
        let srv = send_server_retag(s, sp, &lines)?;
        report.server_updated += srv.updated;
        report.notes.extend(srv.notes);
        for b in &server_batch {
            let any_hit = b.paths.iter().any(|p| srv.updated_rels.contains(p));
            if any_hit {
                server_ok.insert(b.proposal.album_id.clone());
                let rec = backups.entry(b.proposal.album_id.clone()).or_insert_with(|| UndoRecord {
                    album_id: b.proposal.album_id.clone(),
                    artist: b.proposal.artist.clone(),
                    album: b.proposal.album.clone(),
                    local: BTreeMap::new(),
                    server_paths: Vec::new(),
                    server_old: OldFileTags {
                        genres: b.proposal.old_genres.clone(),
                        moods: b.proposal.old_moods.clone(),
                        ai_enriched: None,
                    },
                });
                rec.server_paths = b.paths.clone();
            }
        }
        if srv.updated > 0 {
            if let Some((sub, _)) = localbeat_core::server_library::client(s) {
                if let Err(e) = push::scan(&sub) {
                    report.notes.push(format!("server scan: {e:#} (changes will appear within 10 minutes)"));
                }
            }
        }
    }

    // Local files (offline users, or local copies alongside the server).
    for (p, targets, genres, moods) in per_album {
        let mut local_ok = false;
        for file in &targets.local {
            match ai_enrich::write_tags_to_file(file, &genres, &moods, Some(taxonomy_version)) {
                Ok(old) => {
                    report.local_updated += 1;
                    local_ok = true;
                    touched_local.push(file.clone());
                    let rec = backups.entry(p.album_id.clone()).or_insert_with(|| UndoRecord {
                        album_id: p.album_id.clone(),
                        artist: p.artist.clone(),
                        album: p.album.clone(),
                        local: BTreeMap::new(),
                        server_paths: Vec::new(),
                        server_old: OldFileTags {
                            genres: p.old_genres.clone(),
                            moods: p.old_moods.clone(),
                            ai_enriched: None,
                        },
                    });
                    rec.local.insert(file.to_string_lossy().into_owned(), old);
                }
                Err(e) => report.notes.push(format!("{}: {e:#}", file.display())),
            }
        }
        if local_ok || server_ok.contains(&p.album_id) {
            report.albums_applied.push(p.album_id.clone());
        }
    }

    if !touched_local.is_empty() {
        localbeat_core::library::invalidate_cache(s, &touched_local);
    }

    // Save undo backups (keeping the earliest backup if an album is re-enriched).
    let bdir = PendingStore::backups_dir(&s.config_dir);
    if !backups.is_empty() && std::fs::create_dir_all(&bdir).is_ok() {
        for (id, rec) in backups {
            let f = bdir.join(format!("{id}.json"));
            if !f.exists() {
                let _ = std::fs::write(&f, serde_json::to_vec_pretty(&rec).unwrap_or_default());
            }
        }
    }

    Ok(report)
}

/// Restores an album's pre-AI-enrichment tags from its undo backup.
pub fn undo_album(s: &Settings, sp: &ServerPrefs, album_id: &str) -> Result<String> {
    let f = PendingStore::backups_dir(&s.config_dir).join(format!("{album_id}.json"));
    let bytes = std::fs::read(&f).with_context(|| format!("no undo backup for album {album_id}"))?;
    let rec: UndoRecord = serde_json::from_slice(&bytes).context("reading undo backup")?;
    let mut local_n = 0usize;
    let mut touched = Vec::new();
    for (path_s, old) in &rec.local {
        let p = Path::new(path_s);
        if p.is_file() {
            ai_enrich::write_tags_to_file(p, &old.genres, &old.moods, old.ai_enriched.as_deref())?;
            touched.push(p.to_path_buf());
            local_n += 1;
        }
    }
    if !touched.is_empty() {
        localbeat_core::library::invalidate_cache(s, &touched);
    }
    let mut server_n = 0usize;
    if !rec.server_paths.is_empty() && sp.enabled() {
        let line = json!({
            "paths": rec.server_paths,
            "genres": rec.server_old.genres,
            "moods": rec.server_old.moods,
            "ai_enriched": rec.server_old.ai_enriched,
        })
        .to_string();
        let srv = send_server_retag(s, sp, &[line])?;
        server_n = srv.updated;
        if server_n > 0 {
            if let Some((sub, _)) = localbeat_core::server_library::client(s) {
                let _ = push::scan(&sub);
            }
        }
    }
    let _ = std::fs::remove_file(&f);
    Ok(format!("Restored {} — {} ({local_n} local, {server_n} server file(s))", rec.artist, rec.album))
}

pub(crate) struct ServerRetagResult {
    pub(crate) updated: usize,
    pub(crate) updated_rels: BTreeSet<String>,
    pub(crate) notes: Vec<String>,
}

pub(crate) fn send_server_retag(s: &Settings, sp: &ServerPrefs, json_lines: &[String]) -> Result<ServerRetagResult> {
    push::reachable(sp).map_err(|e| anyhow!("{e}. Retagging server files works from the home network only (SSH)"))?;
    let rs = Rsync::find()?;
    let key = rs.path(&sp.key_path());
    let home = dirs::home_dir().unwrap_or_default().join(".ssh").join("known_hosts");
    let files = format!("UserKnownHostsFile=\"{}\" \"{}\"", rs.path(&home), rs.path(&push::known_hosts(s)));
    let (host, port) = push::host_port(sp);
    let mut child = rs
        .ssh()
        .args(["-p", &port.to_string(), "-i", &key])
        .args([
            "-o", "IdentitiesOnly=yes",
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes",
            "-o", &files,
            "-o", "ConnectTimeout=10",
        ])
        .arg(format!("{}@{host}", sp.user))
        .arg("localbeat-retag")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh for localbeat-retag")?;
    {
        let mut stdin = child.stdin.take().context("ssh stdin")?;
        for line in json_lines {
            writeln!(stdin, "{line}").context("writing to ssh")?;
        }
    }
    let out = child.wait_with_output().context("waiting for ssh")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut res = ServerRetagResult {
        updated: 0,
        updated_rels: BTreeSet::new(),
        notes: Vec::new(),
    };
    let mut answered = false;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("updated ") {
            res.updated += 1;
            answered = true;
            // Strip leading "FLAC/" or "Miscellaneous/" to match the relative song path.
            let rel = rest.split_once('/').map(|(_, r)| r).unwrap_or(rest);
            res.updated_rels.insert(rel.to_string());
        } else if line.starts_with("missing ") {
            answered = true;
        } else if let Some(rest) = line.strip_prefix("refused ") {
            answered = true;
            res.notes.push(format!("server refused {rest}"));
        } else if let Some(rest) = line.strip_prefix("error ") {
            answered = true;
            res.notes.push(format!("server error: {rest}"));
        }
    }
    if !answered {
        let why = stderr.lines().chain(stdout.lines()).find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        if stderr.contains("rrsync") || why.contains("rsync") {
            bail!("the server doesn't support localbeat-retag yet: install the updated localbeat-server & run host/setup.sh on the Mac");
        }
        if stderr.contains("Permission denied") {
            bail!("the server refused this computer's writer key");
        }
        bail!(
            "the server didn't answer ({})",
            if why.is_empty() { format!("ssh exit {:?}", out.status.code()) } else { why.to_string() }
        );
    }
    Ok(res)
}
