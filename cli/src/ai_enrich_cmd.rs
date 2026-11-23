//! `localbeat ai-enrich`: CLI interface to the AI genre/subgenre/mood enrichment
//! workflow (shared with Studio's "AI Genres & Moods" page).

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use localbeat_core::ai_enrich::{self, Confidence, ExportOptions, PendingStore, Taxonomy};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::library::{Library, ScanStats};
use localbeat_core::server_library::{self, Catalogue};
use localbeat_engine::retag;

#[derive(Subcommand)]
pub enum AiEnrichAction {
    /// Show how many albums are AI-enriched, pending review, flagged, or remaining
    Status,
    /// Export the next batch of unenriched albums as an AI prompt (stdout or --out FILE)
    Export {
        /// Albums per batch (default 60)
        #[arg(short, long, default_value_t = ai_enrich::DEFAULT_BATCH_SIZE)]
        batch_size: usize,
        /// Include albums already tagged with the current taxonomy version
        #[arg(long)]
        all: bool,
        /// Write the prompt to this file instead of stdout
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Import an AI's JSON reply (from FILE or stdin) into the review queue
    Import {
        /// File containing the AI reply (default: stdin)
        file: Option<PathBuf>,
    },
    /// Export flagged albums (with full tracklists and notes) for a follow-up AI discussion
    Followup {
        /// Write the prompt to this file instead of stdout
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Apply reviewed proposals to the audio files (and the server, if configured)
    Apply {
        /// Apply only high-confidence proposals with no warnings or flags
        #[arg(long)]
        high_only: bool,
        /// Apply specific album IDs only
        ids: Vec<String>,
    },
    /// Restore an album's pre-AI tags from its undo backup
    Undo { album_id: String },
    /// Print the path to the editable taxonomy file (creating defaults if needed)
    Taxonomy,
}

fn load_merged_library(s: &Settings) -> Result<Library> {
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

pub fn run(action: Option<AiEnrichAction>) -> Result<bool> {
    let s = Settings::load();
    let prefs = Prefs::load(&s);
    let tax = Taxonomy::load(&s.config_dir);
    let action = action.unwrap_or(AiEnrichAction::Status);

    match action {
        AiEnrichAction::Taxonomy => {
            println!("{}", Taxonomy::file(&s.config_dir).display());
            Ok(true)
        }
        AiEnrichAction::Status => {
            let lib = load_merged_library(&s)?;
            let pending = PendingStore::load(&s.config_dir);
            let st = ai_enrich::stats(&lib, &tax, &pending);
            println!("Taxonomy: {} ({} broad genres, {} moods)", tax.version, tax.broad_genres.len(), tax.moods.len());
            println!(
                "Albums: {} total · {} AI-enriched · {} pending review ({} flagged) · {} remaining",
                st.total_albums, st.enriched_current, st.pending_review, st.flagged, st.remaining_to_export
            );
            Ok(true)
        }
        AiEnrichAction::Export { batch_size, all, out } => {
            let lib = load_merged_library(&s)?;
            let pending = PendingStore::load(&s.config_dir);
            let opts = ExportOptions { batch_size, include_enriched: all, include_pending: false };
            let Some(batch) = ai_enrich::export_batch_prompt(&lib, &tax, &pending, &opts) else {
                eprintln!("All {} albums are already enriched or pending review.", lib.albums.len());
                return Ok(true);
            };
            match out {
                Some(p) => {
                    std::fs::write(&p, &batch.prompt).with_context(|| format!("writing {}", p.display()))?;
                    eprintln!(
                        "Wrote {} album(s) to {} ({} remaining after this batch)",
                        batch.album_ids.len(),
                        p.display(),
                        batch.remaining_after
                    );
                }
                None => {
                    print!("{}", batch.prompt);
                    eprintln!(
                        "\n# Exported {} album(s) ({} remaining after this batch)",
                        batch.album_ids.len(),
                        batch.remaining_after
                    );
                }
            }
            Ok(true)
        }
        AiEnrichAction::Import { file } => {
            let text = match file {
                Some(p) => std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?,
                None => {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
                    buf
                }
            };
            let lib = load_merged_library(&s)?;
            let mut pending = PendingStore::load(&s.config_dir);
            let sum = ai_enrich::import_response(&text, &lib, &tax, &mut pending)?;
            pending.save(&s.config_dir)?;
            println!(
                "Imported {} new, {} updated ({} with taxonomy warnings; {} unmatched).",
                sum.added,
                sum.updated,
                sum.warning_count,
                sum.unmatched.len()
            );
            for u in &sum.unmatched {
                eprintln!("  unmatched: {u}");
            }
            Ok(true)
        }
        AiEnrichAction::Followup { out } => {
            let lib = load_merged_library(&s)?;
            let pending = PendingStore::load(&s.config_dir);
            let Some((prompt, n)) = ai_enrich::export_followup_prompt(&lib, &tax, &pending) else {
                eprintln!("No flagged albums to discuss.");
                return Ok(true);
            };
            match out {
                Some(p) => {
                    std::fs::write(&p, &prompt).with_context(|| format!("writing {}", p.display()))?;
                    eprintln!("Wrote follow-up prompt for {n} flagged album(s) to {}", p.display());
                }
                None => print!("{prompt}"),
            }
            Ok(true)
        }
        AiEnrichAction::Apply { high_only, ids } => {
            let lib = load_merged_library(&s)?;
            let mut pending = PendingStore::load(&s.config_dir);
            let chosen: Vec<_> = pending
                .items
                .values()
                .filter(|p| {
                    if !ids.is_empty() {
                        ids.iter().any(|i| i.eq_ignore_ascii_case(&p.album_id))
                    } else if high_only {
                        p.confidence == Confidence::High && !p.flagged && p.warnings.is_empty()
                    } else {
                        !p.flagged
                    }
                })
                .cloned()
                .collect();
            if chosen.is_empty() {
                bail!("no matching proposals to apply");
            }
            let report = retag::apply_proposals(&s, &prefs.server, &lib, &chosen, &tax.version)?;
            for id in &report.albums_applied {
                pending.items.remove(id);
            }
            pending.save(&s.config_dir)?;
            println!("{}", report.summary());
            for note in &report.notes {
                eprintln!("  note: {note}");
            }
            Ok(true)
        }
        AiEnrichAction::Undo { album_id } => {
            let msg = retag::undo_album(&s, &prefs.server, &album_id)?;
            println!("{msg}");
            Ok(true)
        }
    }
}
