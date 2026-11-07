use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ripshelf_core::accuraterip::Verdict;
use ripshelf_core::album_check::{self, DiscInfo, Observer, TrackReport};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check ripped albums against AccurateRip (decodes every track)
    Check {
        /// Album folders containing .flac files
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check { dirs } => check(&dirs),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Printer {
    matched: usize,
    unmatched: usize,
    unknown: usize,
    skipped: usize,
}

impl Observer for Printer {
    fn disc(&mut self, d: &DiscInfo) {
        println!(
            "  disc: {} tracks, AccurateRip ID {} (TOC from {}){}",
            d.track_count,
            d.accuraterip_id,
            d.toc_source,
            if d.in_database { "" } else { " — not in AccurateRip" }
        );
    }

    fn track(&mut self, r: &TrackReport) {
        let name = r.path.file_name().unwrap_or_default().to_string_lossy();
        let result = match r.verdict {
            Verdict::Match { version, confidence } => {
                self.matched += 1;
                format!("OK  v{version}, confidence {confidence}")
            }
            Verdict::NoMatch => {
                self.unmatched += 1;
                "NO MATCH".to_string()
            }
            Verdict::NotInDatabase => {
                self.unknown += 1;
                "not in database".to_string()
            }
        };
        println!("  {:>2}  v1 {:08X}  v2 {:08X}  {result:<24} {name}", r.track, r.sums.v1, r.sums.v2);
    }

    fn skipped(&mut self, path: &Path, why: &str) {
        self.skipped += 1;
        println!("  skipped {}: {why}", path.display());
    }
}

/// Returns Ok(true) if every checked track matched.
fn check(dirs: &[PathBuf]) -> Result<bool> {
    let mut p = Printer::default();
    for dir in dirs {
        println!("{}", dir.display());
        if let Err(e) = album_check::check_dir(dir, &mut p) {
            eprintln!("  error: {e:#}");
            p.skipped += 1;
        }
    }
    println!(
        "\n{} matched, {} no match, {} not in database, {} skipped/failed",
        p.matched, p.unmatched, p.unknown, p.skipped
    );
    Ok(p.unmatched == 0 && p.unknown == 0 && p.skipped == 0)
}
