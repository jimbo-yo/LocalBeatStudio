//! Applying approved cover art to albums: locally (embedded in each audio file
//! plus `cover.jpg` in the album folder) and on the LocalBeat server in place
//! over SSH (`localbeat-retag`, preserving `mtime`, followed by `startScan`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use localbeat_core::config::{ServerPrefs, Settings};
use localbeat_core::cover_finder;
use localbeat_core::library::{Library, Origin, Song};
use serde_json::json;

use crate::push;
use crate::retag;

#[derive(Debug, Clone)]
pub struct CoverUpdate {
    pub album_key: String,
    pub data: Vec<u8>,
    pub mime: String,
}

#[derive(Debug, Default, Clone)]
pub struct CoverApplyReport {
    pub albums_applied: Vec<String>,
    pub local_updated: usize,
    pub server_updated: usize,
    pub notes: Vec<String>,
}

impl CoverApplyReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.local_updated > 0 {
            parts.push(format!("{} local file(s)", self.local_updated));
        }
        if self.server_updated > 0 {
            parts.push(format!("{} server file(s)", self.server_updated));
        }
        if parts.is_empty() {
            "No covers updated".into()
        } else {
            format!("Applied cover art to {} across {} album(s)", parts.join(" and "), self.albums_applied.len())
        }
    }
}

fn rel_to(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty() && !parts.iter().any(|p| p == ".." || p == ".")).then(|| parts.join("/"))
}

struct Targets {
    local_files: Vec<PathBuf>,
    local_dirs: BTreeSet<PathBuf>,
    server_paths: Vec<String>,
    server_cache_dirs: BTreeSet<PathBuf>,
}

fn targets_for(s: &Settings, songs: &[&Song]) -> Targets {
    let mut local_files = Vec::new();
    let mut local_dirs = BTreeSet::new();
    let mut server_paths = BTreeSet::new();
    let mut server_cache_dirs = BTreeSet::new();
    for x in songs {
        match x.origin {
            Origin::Server => {
                if let Some(rel) = &x.server_path {
                    server_paths.insert(rel.clone());
                }
                if let Some(dir) = x.path.parent() {
                    server_cache_dirs.insert(dir.to_path_buf());
                }
            }
            Origin::Flac | Origin::Misc => {
                if x.path.is_file() {
                    local_files.push(x.path.clone());
                    if let Some(dir) = x.path.parent() {
                        local_dirs.insert(dir.to_path_buf());
                    }
                }
                let root = if x.origin == Origin::Flac { &s.flac_dir } else { &s.misc_dir };
                if let Some(rel) = rel_to(&x.path, root) {
                    server_paths.insert(rel);
                }
            }
        }
    }
    Targets {
        local_files,
        local_dirs,
        server_paths: server_paths.into_iter().collect(),
        server_cache_dirs,
    }
}

/// Applies a batch of approved cover images to local files and/or the server.
pub fn apply_covers(
    s: &Settings,
    sp: &ServerPrefs,
    lib: &Library,
    updates: &[CoverUpdate],
) -> Result<CoverApplyReport> {
    if updates.is_empty() {
        return Ok(CoverApplyReport::default());
    }
    let by_key: BTreeMap<&str, &localbeat_core::library::Album> = lib.albums.iter().map(|a| (a.key.as_str(), a)).collect();

    let mut report = CoverApplyReport::default();
    let mut server_lines: Vec<String> = Vec::new();
    let mut server_map: Vec<(&CoverUpdate, Vec<String>, BTreeSet<PathBuf>)> = Vec::new();
    let mut local_work: Vec<(&CoverUpdate, Vec<PathBuf>, BTreeSet<PathBuf>)> = Vec::new();

    for u in updates {
        let Some(album) = by_key.get(u.album_key.as_str()) else {
            report.notes.push(format!("Album {} not in current library", u.album_key));
            continue;
        };
        let songs: Vec<&Song> = album.songs.iter().map(|&i| &lib.songs[i]).collect();
        let t = targets_for(s, &songs);
        if t.local_files.is_empty() && t.server_paths.is_empty() {
            report.notes.push(format!("{} — {}: no writable files found", album.artist, album.title));
            continue;
        }
        if t.local_files.is_empty() && !t.server_paths.is_empty() && !sp.enabled() {
            report.notes.push(format!(
                "{} — {}: only on the server (updating server covers needs an admin writer key)",
                album.artist, album.title
            ));
            continue;
        }
        if !t.server_paths.is_empty() && sp.enabled() {
            let b64 = cover_finder::encode_base64(&u.data);
            server_lines.push(
                json!({
                    "paths": t.server_paths,
                    "cover_b64": b64,
                    "cover_mime": u.mime,
                })
                .to_string(),
            );
            server_map.push((u, t.server_paths, t.server_cache_dirs));
        }
        if !t.local_files.is_empty() {
            local_work.push((u, t.local_files, t.local_dirs));
        }
    }

    let mut applied_keys: BTreeSet<String> = BTreeSet::new();

    // 1. Server updates over one SSH `localbeat-retag` stream.
    if !server_lines.is_empty() {
        match retag::send_server_retag(s, sp, &server_lines) {
            Ok(srv) => {
                report.server_updated = srv.updated;
                report.notes.extend(srv.notes);
                for (u, paths, cache_dirs) in &server_map {
                    if paths.iter().any(|p| srv.updated_rels.contains(p)) {
                        applied_keys.insert(u.album_key.clone());
                        // Drop stale cached cover.jpg so Studio fetches the fresh cover.
                        for dir in cache_dirs {
                            let _ = std::fs::remove_file(dir.join("cover.jpg"));
                            let _ = std::fs::remove_file(dir.join("cover.png"));
                        }
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
            Err(e) => report.notes.push(format!("server cover update failed: {e:#}")),
        }
    }

    // 2. Local files + folder cover.jpg.
    let mut touched_local = Vec::new();
    for (u, files, dirs) in local_work {
        let mut ok = false;
        for f in &files {
            match cover_finder::embed_cover_file(f, &u.data, &u.mime) {
                Ok(()) => {
                    report.local_updated += 1;
                    touched_local.push(f.clone());
                    ok = true;
                }
                Err(e) => report.notes.push(format!("{}: {e:#}", f.display())),
            }
        }
        if ok {
            for d in &dirs {
                let _ = cover_finder::write_folder_cover(d, &u.data, &u.mime);
            }
            applied_keys.insert(u.album_key.clone());
        }
    }

    if !touched_local.is_empty() {
        localbeat_core::library::invalidate_cache(s, &touched_local);
    }

    // Invalidate Studio's thumbnail cache for applied albums.
    let thumb_dir = s.cache_dir.join("covers");
    for key in &applied_keys {
        let prefix = format!("{}-", cover_finder::key_short_id(key));
        if let Ok(rd) = std::fs::read_dir(&thumb_dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    report.albums_applied = applied_keys.into_iter().collect();
    Ok(report)
}
