mod ai_enrich_cmd;
mod cover_cmd;
mod repair_cmd;
mod ctdb_cmd;
mod library_cmd;
mod play_cmd;
mod drive_cmds;
mod rip_cmd;
mod enrich_cmd;
mod sync_cmd;
mod superseded_cmd;
mod server_cmd;
mod join_cmd;
mod mcp_cmd;
mod player_cmd;
mod playlist_cmd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use localbeat_core::accuraterip::Verdict;
use localbeat_core::album_check::{self, DiscInfo, Observer, TrackReport};

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
        /// Keep going: after each disc, wait for the next one
        #[arg(short, long = "loop")]
        repeat: bool,
        /// With --loop: tell the phone (KDE Connect) when the drive has waited
        /// a minute for the next disc, and straight away about problems
        #[arg(long)]
        notify_phone: bool,
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
    /// Make Opus phone copies of new or changed FLACs (like flac2phone.sh)
    /// Make the phone's music folder hold the chosen albums (as Opus, made from
    /// the FLAC and Miscellaneous libraries). Starts from what's there now.
    Sync {
        /// The phone over USB (adb, USB debugging on)
        #[arg(long)]
        usb: bool,
        /// The phone over Wi-Fi (KDE Connect on Linux; adb wireless debugging
        /// on Windows, paired once with `localbeat pair`)
        #[arg(long)]
        wifi: bool,
        /// A folder standing in for the phone's music folder (testing)
        #[arg(long)]
        to: Option<std::path::PathBuf>,
        /// Also put on albums whose folder name contains this (repeatable)
        #[arg(long)]
        add: Vec<String>,
        /// Take off albums whose folder name contains this (repeatable)
        #[arg(long)]
        remove: Vec<String>,
        /// Only show what would happen
        #[arg(long)]
        dry_run: bool,
    },
    /// Find tags and cover art on MusicBrainz for Miscellaneous albums (old
    /// .m4a/.mp3): sure matches are written into the files, the rest listed
    Enrich {
        /// Only albums whose folder contains this (repeatable; default: all)
        patterns: Vec<String>,
        /// Only show what would be written
        #[arg(long)]
        dry_run: bool,
        /// At most this many albums this run
        #[arg(long)]
        limit: Option<usize>,
        /// Look up again albums already done, not found or mixed
        #[arg(long)]
        retry: bool,
        /// Put back the tags of the matching written albums
        #[arg(long)]
        undo: bool,
        /// Identify albums name search couldn't (not found, loose songs,
        /// review) by sound, through AcoustID
        #[arg(long)]
        sound: bool,
        /// Save your AcoustID application key (https://acoustid.org/new-application)
        #[arg(long)]
        acoustid_key: Option<String>,
    },
    /// AI-assisted genre, subgenre and mood enrichment (export prompt, import
    /// JSON, review/follow-up, apply tags to files and server)
    AiEnrich {
        #[command(subcommand)]
        action: Option<ai_enrich_cmd::AiEnrichAction>,
    },
    /// Find and embed missing album cover art (local files and server)
    Covers {
        #[command(subcommand)]
        action: Option<cover_cmd::CoversAction>,
    },
    /// Your playlists on the music server: list, show, new (from a song
    /// list with --from), add, remove, move, rename, share, delete
    Playlist {
        #[command(subcommand)]
        action: Option<playlist_cmd::PlaylistAction>,
        /// JSON out (for scripts and AI tools)
        #[arg(long, global = true)]
        json: bool,
    },
    /// Which songs of a list ("Artist - Title" per line; a file, or stdin)
    /// are in the library on the server
    Match {
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// MCP server over stdio, for AI tools: search the library, match song
    /// lists, build playlists (claude mcp add localbeat -- localbeat mcp)
    Mcp,
    /// The LocalBeat app on the phone, live over adb: status, play [playlist or
    /// words], pause, toggle, next, prev, seek, shuffle, repeat, queue, playlists
    Player {
        #[command(subcommand)]
        action: Option<player_cmd::PlayerAction>,
        /// JSON out (for scripts and AI tools)
        #[arg(long, global = true)]
        json: bool,
    },
    /// The LocalBeat server that rips are sent to: show or set it
    Server {
        #[command(subcommand)]
        action: Option<server_cmd::ServerAction>,
    },
    /// Albums waiting to go to the server (and recently sent); `add` queues
    /// albums already in the library
    Outbox {
        #[command(subcommand)]
        action: Option<server_cmd::OutboxAction>,
    },
    /// Send the albums waiting in the outbox to the server now (home network only)
    Push,
    /// List Miscellaneous folders that are now FLAC rips; --move puts them
    /// in Music/Superseded (nothing deleted)
    Superseded {
        #[arg(long = "move")]
        move_them: bool,
    },
    /// Identify one song by sound (AcoustID) and show what it found
    Fingerprint { file: std::path::PathBuf },
    /// Repair an album with CUETools-database parity (and saved reads of
    /// rejected tracks); --write replaces the tracks that then match
    Repair {
        album: PathBuf,
        /// Folder with trackNN/readK.raw (the rejected folder's "reads")
        #[arg(long)]
        reads: Option<PathBuf>,
        /// Replace tracks that now match the database (originals are kept)
        #[arg(long)]
        write: bool,
    },
    /// Check ripped albums against the CUETools database: the pressings it
    /// knows and which of our tracks each confirms
    Ctdb { dirs: Vec<PathBuf> },
    /// Scan the music library as the player sees it (FLAC + Miscellaneous)
    Library {
        /// Also make every album's cover thumbnail
        #[arg(long)]
        covers: bool,
        /// List every album
        #[arg(long)]
        list: bool,
    },
    /// Play songs or album folders through the speakers (FLAC, MP3,
    /// AAC/ALAC, Ogg Vorbis, WAV, AIFF)
    Play {
        paths: Vec<PathBuf>,
        /// Start this many seconds into the first song
        #[arg(long)]
        seek: Option<f64>,
        /// Volume 0..1
        #[arg(long)]
        volume: Option<f32>,
    },
    /// Join a LocalBeat server using a one-time invite link (`localbeat://join/...`)
    /// (also still accepts the legacy home-network `--server` request flow)
    Join {
        /// Invite URI (`localbeat://join/...`), or legacy computer name
        target: Option<String>,
        /// Legacy: ask to rip CDs and send them to the server, not only to listen
        #[arg(long)]
        admin: bool,
        /// Legacy: the server's home address
        #[arg(long, default_value = localbeat_core::join::DEFAULT_SERVER)]
        server: String,
        /// Legacy: withdraw a waiting request
        #[arg(long)]
        cancel: bool,
        /// Legacy: whose computer this is (a user on the server, e.g. jim)
        #[arg(long)]
        user: Option<String>,
        /// Replace an existing server setup without confirmation
        #[arg(long)]
        force: bool,
    },
    /// Create, list, or revoke one-time invites for phones and new Studios (admin only)
    Invite {
        #[command(subcommand)]
        action: Option<join_cmd::InviteAction>,
    },
    /// Pair with the phone for adb over Wi-Fi (once per computer): on the
    /// phone, Developer options > Wireless debugging > Pair device with pairing
    /// code. With only an address (Wireless debugging's IP:port), connect.
    Pair {
        /// IP:port the phone shows
        address: String,
        /// The six-digit pairing code
        code: Option<String>,
    },
    /// Send a test notification to the phone (KDE Connect, same Wi-Fi)
    Notify {
        #[arg(default_value = "localbeat test: phone notifications work.")]
        message: String,
    },
    Phone {
        /// FLAC library or album folder (default: the library)
        src: Option<PathBuf>,
        /// Where the copies go (default: Music/phone, or $PHONE_DIR)
        dest: Option<PathBuf>,
        /// Opus bitrate in kbps
        #[arg(short, long, default_value_t = 160)]
        bitrate: u32,
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
        Command::Rip { drive, repeat, notify_phone, offset, release, country, unknown, reads, no_eject, flac_dir } => {
            rip_cmd::rip(rip_cmd::RipArgs { drive, repeat, notify_phone, offset, release, country, unknown, reads, no_eject, flac_dir })
        }
        Command::Offset { drive, action } => {
            rip_cmd::offset(drive.as_deref(), action.map(|OffsetAction::Set { value }| value))
        }
        Command::Sync { usb, wifi, to, add, remove, dry_run } => {
            sync_cmd::sync(sync_cmd::SyncArgs { to, usb, wifi, add, remove, dry_run })
        }
        Command::Enrich { patterns, dry_run, limit, retry, undo, sound, acoustid_key } => {
            enrich_cmd::enrich(enrich_cmd::EnrichArgs { patterns, dry_run, limit, retry, undo, sound, acoustid_key })
        }
        Command::AiEnrich { action } => ai_enrich_cmd::run(action),
        Command::Covers { action } => cover_cmd::run(action),
        Command::Fingerprint { file } => (|| -> anyhow::Result<bool> {
            let s = localbeat_core::config::Settings::load();
            let key = localbeat_core::config::Prefs::load(&s).acoustid_key;
            let (fp, secs) = localbeat_core::acoustid::fingerprint(&file)?;
            println!("fingerprint: {} characters, {secs} s, starts {}", fp.len(), &fp[..fp.len().min(24)]);
            let hits = localbeat_core::acoustid::lookup(&key, &fp, secs)?;
            println!("{} recordings found", hits.len());
            for h in hits.iter().take(5) {
                let rel = h.releases.first().map(|r| format!("{} ({})", r.group_title, r.year)).unwrap_or_default();
                println!("  {:.0}%  {}  [{} releases, e.g. {rel}]", h.score * 100.0, h.title, h.releases.len());
            }
            Ok(true)
        })(),
        Command::Superseded { move_them } => superseded_cmd::run(move_them),
        Command::Playlist { action, json } => playlist_cmd::playlist(action, json),
        Command::Match { file, json } => playlist_cmd::match_cmd(file, json),
        Command::Mcp => mcp_cmd::run(),
        Command::Player { action, json } => player_cmd::run(action, json),
        Command::Server { action } => server_cmd::server(action),
        Command::Outbox { action } => server_cmd::outbox(action),
        Command::Push => server_cmd::push(),
        Command::Play { paths, seek, volume } => play_cmd::run(paths, seek, volume),
        Command::Library { covers, list } => library_cmd::run(covers, list),
        Command::Ctdb { dirs } => ctdb_cmd::run(dirs),
        Command::Repair { album, reads, write } => repair_cmd::run(album, reads, write),
        Command::Join { target, admin, server, cancel, user, force } => {
            join_cmd::run(join_cmd::JoinArgs { target, admin, server, cancel, user, force })
        }
        Command::Invite { action } => join_cmd::run_invite(action),
        Command::Pair { address, code } => match code {
            Some(code) => localbeat_platform::phone::adb_pair(&address, &code).map(|msg| {
                println!("{msg}\nlocalbeat sync --wifi now finds the phone whenever Wireless debugging is on.");
                true
            }),
            None => localbeat_platform::phone::adb_connect(&address).map(|()| {
                println!("Connected to {address}");
                true
            }),
        },
        Command::Notify { message } => {
            let s = localbeat_core::config::Settings::load();
            localbeat_platform::phone_notify::notify(&s.config_dir, &message).map(|phone| {
                println!("Sent to {phone}: {message}");
                true
            })
        }
        Command::Phone { src, dest, bitrate } => {
            let s = localbeat_core::config::Settings::load();
            let src = src.unwrap_or_else(|| s.flac_dir.clone());
            // An album inside the library maps to the same place under the phone folder.
            let dest = dest.unwrap_or_else(|| match std::path::absolute(&src).ok().and_then(|a| a.strip_prefix(&s.flac_dir).ok().map(|r| r.to_path_buf())) {
                Some(rel) => s.phone_dir.join(rel),
                None => s.phone_dir.clone(),
            });
            println!("{} -> {} (Opus {bitrate} kbps)", src.display(), dest.display());
            match localbeat_core::lock::lock(&s.cache_dir, "phone") {
                Ok(_phone) => rip_cmd::phone_copies(&src, &dest, bitrate),
                Err(e) => Err(e),
            }
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
            Verdict::NoMatch => match r.other_pressing {
                Some((shift, Some(confidence))) => {
                    self.matched += 1;
                    format!("OK  other pressing {shift:+}, conf {confidence}")
                }
                Some((shift, None)) => {
                    self.unmatched += 1;
                    format!("NO MATCH (frame 450 at {shift:+})")
                }
                None => {
                    self.unmatched += 1;
                    "NO MATCH".to_string()
                }
            },
            Verdict::NotInDatabase => {
                self.unknown += 1;
                "not in database".to_string()
            }
        };
        println!("  {:>2}  v1 {:08X}  v2 {:08X}  {result:<24} {name}", r.track, r.sums.v1, r.sums.v2);
        if r.right_pressing {
            println!("        (right pressing: its frame-450 sector matches, so the audio differs elsewhere: a read error?)");
        }
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
