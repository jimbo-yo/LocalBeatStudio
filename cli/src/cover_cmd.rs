//! CLI subcommand: `localbeat covers [list|fetch|set]`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::cover_finder;
use localbeat_core::library::{Library, ScanStats, norm};
use localbeat_core::server_library::{self, Catalogue};
use localbeat_engine::cover_apply::{self, CoverUpdate};

#[derive(Subcommand)]
pub enum CoversAction {
    /// List albums missing cover art (default)
    List,
    /// Search iTunes + Cover Art Archive for missing covers and show best matches
    Fetch {
        /// Limit how many missing albums to search (0 = all)
        #[arg(short, long, default_value_t = 0)]
        limit: usize,
        /// Immediately download and apply high-confidence matches (>= 80%)
        #[arg(long)]
        apply: bool,
    },
    /// Embed a specific cover image (local file path or URL) into an album
    Set {
        /// Album query (short ID, or substring of "Artist - Album")
        album: String,
        /// Path to an image file, or an http(s):// image URL
        image: String,
    },
}

fn load_library(s: &Settings) -> Result<Library> {
    let (local, _) = Library::scan(s, &|_, _| {})?;
    let mut songs = local.songs;
    if let Some((sub, url)) = server_library::client(s) {
        let cat = Catalogue::fetch(s, &sub, &url)
            .ok()
            .inspect(|c| {
                let _ = c.save(s);
            })
            .or_else(|| Catalogue::load(s, &url));
        if let Some(c) = cat {
            songs.extend(c.songs);
        }
    }
    Ok(Library::build(songs, &mut ScanStats::default()))
}

pub fn run(action: Option<CoversAction>) -> Result<bool> {
    let s = Settings::load();
    let prefs = Prefs::load(&s);
    let lib = load_library(&s)?;

    match action.unwrap_or(CoversAction::List) {
        CoversAction::List => {
            let missing = cover_finder::missing_cover_albums(&lib);
            println!("{} album(s) missing cover art (out of {}):", missing.len(), lib.albums.len());
            for a in &missing {
                let yr = a.year.map(|y| format!(" ({y})")).unwrap_or_default();
                println!("  {}  {} — {}{yr}  [{} tracks]", cover_finder::album_short_id(a), a.artist, a.title, a.songs.len());
            }
            Ok(true)
        }
        CoversAction::Fetch { limit, apply } => {
            let missing = cover_finder::missing_cover_albums(&lib);
            let slice = if limit > 0 { &missing[..missing.len().min(limit)] } else { &missing[..] };
            println!("Searching covers for {} album(s)...", slice.len());
            let mut updates = Vec::new();
            for a in slice {
                let cands = cover_finder::search_covers(&a.artist, &a.title);
                if let Some(best) = cands.first() {
                    println!(
                        "  {:>3.0}%  {} — {}  ->  [{}] {} — {}",
                        best.score * 100.0,
                        a.artist,
                        a.title,
                        best.source,
                        best.artist,
                        best.title
                    );
                    if apply && best.score >= 0.80 {
                        match cover_finder::download_cover(&best.full_url) {
                            Ok(img) => updates.push(CoverUpdate {
                                album_key: a.key.clone(),
                                data: img.data,
                                mime: img.mime,
                            }),
                            Err(e) => eprintln!("    download failed: {e:#}"),
                        }
                    }
                } else {
                    println!("   --   {} — {}  (no match)", a.artist, a.title);
                }
            }
            if apply && !updates.is_empty() {
                let report = cover_apply::apply_covers(&s, &prefs.server, &lib, &updates)?;
                println!("\n{}", report.summary());
                for n in &report.notes {
                    eprintln!("  note: {n}");
                }
            }
            Ok(true)
        }
        CoversAction::Set { album, image } => {
            let q = norm(&album);
            let matched = lib
                .albums
                .iter()
                .find(|a| {
                    cover_finder::album_short_id(a).eq_ignore_ascii_case(album.trim())
                        || norm(&format!("{} {}", a.artist, a.title)).contains(&q)
                })
                .with_context(|| format!("no album matching '{album}'"))?;
            let img = if image.starts_with("http://") || image.starts_with("https://") {
                cover_finder::download_cover(&image)?
            } else {
                let p = PathBuf::from(&image);
                if !p.is_file() {
                    bail!("image file not found: {}", p.display());
                }
                cover_finder::load_local_cover(&p)?
            };
            let report = cover_apply::apply_covers(
                &s,
                &prefs.server,
                &lib,
                &[CoverUpdate {
                    album_key: matched.key.clone(),
                    data: img.data,
                    mime: img.mime,
                }],
            )?;
            println!("{} — {}: {}", matched.artist, matched.title, report.summary());
            for n in &report.notes {
                eprintln!("  note: {n}");
            }
            Ok(!report.albums_applied.is_empty())
        }
    }
}
