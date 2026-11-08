mod drive_cmds;
mod rip_cmd;

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
    /// List CD drives with model, serial and whether an audio CD is in
    Drives,
    /// Show the disc's TOC, MusicBrainz and AccurateRip IDs
    Toc {
        /// Drive (E: on Windows, /dev/sr1 on Linux); default: the first with an audio CD
        #[arg(short, long)]
        drive: Option<String>,
    },
    /// Read one track and check it against AccurateRip (drive test)
    ReadTrack {
        /// Track number
        track: usize,
        /// Drive's read offset in samples (HP GTA0N: +102, SuperDrive: +6)
        #[arg(short, long, allow_hyphen_values = true)]
        offset: i32,
        #[arg(short, long)]
        drive: Option<String>,
        /// How many times to read it (compares the reads)
        #[arg(short, long, default_value_t = 1)]
        reads: u32,
        /// Read speed multiplier to request (default: leave as is)
        #[arg(short, long)]
        speed: Option<u8>,
        /// Sectors to read elsewhere before each re-read, to empty the drive cache
        #[arg(long, default_value_t = 1000)]
        flush: u32,
        /// Save the (last) read as a WAV file
        #[arg(long)]
        wav: Option<PathBuf>,
    },
    /// Look the disc up on MusicBrainz: candidate releases and the tags it would get
    Identify {
        #[arg(short, long)]
        drive: Option<String>,
        /// Use this CDTOC tag value instead of reading a drive
        #[arg(long)]
        cdtoc: Option<String>,
        /// Preferred release country ("" for none)
        #[arg(long, default_value = "US")]
        country: String,
    },
    /// Rip the disc: the tracks missing from the library, verified, tagged, filed
    Rip {
        #[arg(short, long)]
        drive: Option<String>,
        /// Read offset for this rip (default: the one saved for the drive)
        #[arg(short, long, allow_hyphen_values = true)]
        offset: Option<i32>,
        /// Use this MusicBrainz release ID
        #[arg(short = 'R', long)]
        release: Option<String>,
        /// Preferred release country (default US, or $COUNTRY; "" for none)
        #[arg(long)]
        country: Option<String>,
        /// Disc not in MusicBrainz: rip with generic names
        #[arg(short = 'U', long)]
        unknown: bool,
        /// Reads per track before giving up (default 4)
        #[arg(long)]
        reads: Option<u32>,
        /// Leave the disc in the drive
        #[arg(long)]
        no_eject: bool,
        /// Library folder (default: Music/FLAC, or $FLAC_DIR)
        #[arg(long)]
        flac_dir: Option<PathBuf>,
    },
    /// Show the drive's saved read offset, or save one: offset set 102
    Offset {
        #[arg(short, long)]
        drive: Option<String>,
        #[command(subcommand)]
        action: Option<OffsetAction>,
    },
    /// Eject the disc
    Eject {
        #[arg(short, long)]
        drive: Option<String>,
    },
}

#[derive(Subcommand)]
enum OffsetAction {
    /// Save the read offset for the drive
    Set {
        #[arg(allow_hyphen_values = true)]
        value: i32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check { dirs } => check(&dirs),
        Command::Drives => drive_cmds::drives(),
        Command::Toc { drive } => drive_cmds::toc(drive.as_deref()),
        Command::ReadTrack { track, offset, drive, reads, speed, flush, wav } => {
            drive_cmds::read_track(drive_cmds::ReadTrackArgs { drive, track, offset, reads, speed, flush, wav })
        }
        Command::Identify { drive, cdtoc, country } => drive_cmds::identify(
            drive.as_deref(),
            cdtoc.as_deref(),
            Some(country.as_str()).filter(|c| !c.is_empty()),
        ),
        Command::Rip { drive, offset, release, country, unknown, reads, no_eject, flac_dir } => {
            rip_cmd::rip(rip_cmd::RipArgs { drive, offset, release, country, unknown, reads, no_eject, flac_dir })
        }
        Command::Offset { drive, action } => {
            rip_cmd::offset(drive.as_deref(), action.map(|OffsetAction::Set { value }| value))
        }
        Command::Eject { drive } => drive_cmds::eject(drive.as_deref()),
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
