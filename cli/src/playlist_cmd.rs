//! `localbeat playlist …` and `localbeat match`: the user's playlists on the
//! music server, and matching song lists from elsewhere against the library
//! there (LocalBeat docs/control.md, step 2). Everything goes through the
//! server's Subsonic API with this computer's login (`localbeat server`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use localbeat_core::config::Settings;
use localbeat_core::server_library;
use localbeat_core::song_match::{Candidate, Match, Matcher, Status};
use localbeat_core::subsonic::{PlaylistChange, Subsonic};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum PlaylistAction {
    /// Your playlists, then those shared with you and public ones
    List,
    /// A playlist's songs
    Show { playlist: String },
    /// Make a playlist: from a list of songs (--from) and/or song IDs
    New {
        name: String,
        /// A text file of songs, one per line ("Artist - Title"); - for stdin
        #[arg(long)]
        from: Option<PathBuf>,
        /// Also take the best suggestion for lines that only matched closely
        #[arg(long)]
        close: bool,
        /// Song IDs (as `localbeat match --json` gives them)
        songs: Vec<String>,
    },
    /// Add songs at the end: from a list (--from) and/or song IDs
    Add {
        playlist: String,
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        close: bool,
        songs: Vec<String>,
    },
    /// Take songs out, by position (1 = the first, as `show` numbers them)
    Remove {
        playlist: String,
        #[arg(required = true)]
        positions: Vec<usize>,
    },
    /// Move the song at one position to another
    Move { playlist: String, from: usize, to: usize },
    Rename { playlist: String, name: String },
    /// Share with users (replaces the list; `--nobody` to stop), or with
    /// everyone (--public / --private). With nothing else: show who
    Share {
        playlist: String,
        users: Vec<String>,
        #[arg(long, conflicts_with = "users")]
        nobody: bool,
        #[arg(long, conflicts_with = "private")]
        public: bool,
        #[arg(long)]
        private: bool,
    },
    Delete { playlist: String },
    /// The users on the server (to share with)
    Users,
}

fn client() -> Result<Subsonic> {
    let s = Settings::load();
    server_library::client(&s)
        .map(|(c, _)| c)
        .ok_or_else(|| anyhow!("no music server login on this computer: join the server (Settings page, or `localbeat join`)"))
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn read_list(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading the list from stdin")?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}

/// The whole library on the server, for matching.
fn library(sub: &Subsonic) -> Result<Vec<Candidate>> {
    eprint!("Reading the library on the server… ");
    let songs: Vec<Candidate> = sub.all_songs()?.iter().filter_map(Candidate::from_json).collect();
    eprintln!("{} songs", songs.len());
    Ok(songs)
}

fn describe(c: &Candidate) -> String {
    let year = if c.year > 0 { format!(", {}", c.year) } else { String::new() };
    format!("{} – {} ({}{year}, {}) [{}]", c.artist, c.title, c.album, c.suffix, c.id)
}

/// One line per song of the list, then a summary. To stderr when the JSON
/// goes to stdout.
fn report(matches: &[Match], close_taken: bool, out: &mut dyn std::io::Write) {
    for m in matches {
        let _ = match m.status {
            Status::Found => writeln!(
                out,
                "found    {}\n         → {}{}",
                m.line,
                describe(&m.songs[0]),
                if m.note.is_empty() { String::new() } else { format!("  ({})", m.note) }
            ),
            Status::Close => {
                let _ = writeln!(out, "close?   {}  ({})", m.line, m.note);
                for c in m.songs.iter().take(3) {
                    let _ = writeln!(out, "         maybe {}", describe(c));
                }
                Ok(())
            }
            Status::NotFound => writeln!(
                out,
                "missing  {}{}",
                m.line,
                if m.note.is_empty() { String::new() } else { format!("  ({})", m.note) }
            ),
        };
    }
    let count = |s: Status| matches.iter().filter(|m| m.status == s).count();
    let (found, close, missing) = (count(Status::Found), count(Status::Close), count(Status::NotFound));
    let mut summary = format!("Found {found} of {}", matches.len());
    if close > 0 {
        summary += &format!("; {close} close ({})", if close_taken { "best suggestion taken" } else { "left out: check them, or use --close" });
    }
    if missing > 0 {
        summary += &format!("; {missing} not in the library");
    }
    let _ = writeln!(out, "{summary}.");
}

/// `localbeat match`: the list's songs in the library.
pub fn match_cmd(file: Option<PathBuf>, json_out: bool) -> Result<bool> {
    let text = read_list(file.as_deref().unwrap_or(Path::new("-")))?;
    let sub = client()?;
    let lib = library(&sub)?;
    let matches = Matcher::new(&lib).match_text(&text);
    if matches.is_empty() {
        bail!("no songs in the list (one per line: Artist - Title)");
    }
    if json_out {
        print_json(&serde_json::to_value(&matches)?);
    } else {
        report(&matches, false, &mut std::io::stdout());
    }
    Ok(true)
}

/// Song IDs from a list file (matched) plus IDs given, in that order, each
/// once. Prints the matching report (stderr if `json_out`).
fn songs_from(sub: &Subsonic, from: Option<&Path>, close: bool, ids: &[String], json_out: bool) -> Result<(Vec<String>, Vec<Match>)> {
    let mut out: Vec<String> = Vec::new();
    let mut matches = Vec::new();
    if let Some(path) = from {
        let text = read_list(path)?;
        let lib = library(sub)?;
        matches = Matcher::new(&lib).match_text(&text);
        if matches.is_empty() {
            bail!("no songs in {} (one per line: Artist - Title)", path.display());
        }
        if json_out {
            report(&matches, close, &mut std::io::stderr());
        } else {
            report(&matches, close, &mut std::io::stdout());
        }
        for m in &matches {
            let pick = match m.status {
                Status::Found => m.songs.first(),
                Status::Close if close => m.songs.first(),
                _ => None,
            };
            if let Some(c) = pick {
                out.push(c.id.clone());
            }
        }
    }
    out.extend(ids.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    out.retain(|id| seen.insert(id.clone()));
    Ok((out, matches))
}

/// A playlist by ID, or by name (yours first; case doesn't matter).
fn find(sub: &Subsonic, wanted: &str) -> Result<Value> {
    let all = sub.playlists()?;
    if let Some(p) = all.iter().find(|p| p["id"].as_str() == Some(wanted)) {
        return Ok(p.clone());
    }
    let named: Vec<&Value> = all.iter().filter(|p| p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(wanted.trim()))).collect();
    let mine: Vec<&&Value> = named.iter().filter(|p| p["readonly"] != true).collect();
    match (mine.as_slice(), named.as_slice()) {
        ([one], _) => Ok((**one).clone()),
        ([], [one]) => Ok((*one).clone()),
        ([], []) => bail!("no playlist called {wanted:?} (localbeat playlist list)"),
        _ => bail!(
            "several playlists are called {wanted:?}: use the ID ({})",
            named.iter().filter_map(|p| p["id"].as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn id_of(p: &Value) -> String {
    p["id"].as_str().unwrap_or_default().to_string()
}

fn print_playlist_line(p: &Value) {
    let mut extra = Vec::new();
    if p["readonly"] == true {
        extra.push(format!("from {}", p["owner"].as_str().unwrap_or("?")));
    }
    if p["public"] == true {
        extra.push("public".into());
    }
    if let Some(users) = p["sharedWith"].as_array().filter(|u| !u.is_empty()) {
        extra.push(format!("shared with {}", users.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    }
    if p["missing"].as_u64().unwrap_or(0) > 0 {
        extra.push(format!("{} not found", p["missing"]));
    }
    let mins = p["duration"].as_u64().unwrap_or(0) / 60;
    println!(
        "{}  {}  ({} songs, {} min{}{})",
        id_of(p),
        p["name"].as_str().unwrap_or(""),
        p["songCount"].as_u64().unwrap_or(0),
        mins,
        if extra.is_empty() { "" } else { "; " },
        extra.join("; ")
    );
}

fn print_songs(p: &Value) {
    print_playlist_line(p);
    if !p["comment"].as_str().unwrap_or("").is_empty() {
        println!("  {}", p["comment"].as_str().unwrap_or(""));
    }
    for (i, s) in p["entry"].as_array().into_iter().flatten().enumerate() {
        let secs = s["duration"].as_u64().unwrap_or(0);
        println!(
            "{:>4}. {} – {}  ({}, {}:{:02}) [{}]",
            i + 1,
            s["artist"].as_str().unwrap_or(""),
            s["title"].as_str().unwrap_or(""),
            s["album"].as_str().unwrap_or(""),
            secs / 60,
            secs % 60,
            s["id"].as_str().unwrap_or("")
        );
    }
}

/// 1-based positions from the user → 0-based, checked against the playlist.
fn positions(given: &[usize], count: usize) -> Result<Vec<usize>> {
    given
        .iter()
        .map(|&n| if n >= 1 && n <= count { Ok(n - 1) } else { Err(anyhow!("no song at position {n} (the playlist has {count})")) })
        .collect()
}

pub fn playlist(action: Option<PlaylistAction>, json_out: bool) -> Result<bool> {
    let sub = client()?;
    match action.unwrap_or(PlaylistAction::List) {
        PlaylistAction::List => {
            let all = sub.playlists()?;
            if json_out {
                print_json(&json!(all));
            } else if all.is_empty() {
                println!("No playlists yet (localbeat playlist new NAME --from list.txt).");
            } else {
                all.iter().for_each(print_playlist_line);
            }
        }
        PlaylistAction::Show { playlist } => {
            let p = sub.playlist(&id_of(&find(&sub, &playlist)?))?;
            if json_out { print_json(&p) } else { print_songs(&p) }
        }
        PlaylistAction::New { name, from, close, songs } => {
            let (ids, matches) = songs_from(&sub, from.as_deref(), close, &songs, json_out)?;
            let p = sub.create_playlist(&name, &ids)?;
            if json_out {
                print_json(&json!({ "playlist": p, "matches": matches }));
            } else {
                println!("Made playlist {} \"{}\" with {} songs.", id_of(&p), name, p["songCount"]);
            }
        }
        PlaylistAction::Add { playlist, from, close, songs } => {
            let id = id_of(&find(&sub, &playlist)?);
            let (ids, matches) = songs_from(&sub, from.as_deref(), close, &songs, json_out)?;
            if ids.is_empty() {
                bail!("nothing to add");
            }
            sub.update_playlist(&id, &PlaylistChange { add: ids.clone(), ..Default::default() })?;
            let p = sub.playlist(&id)?;
            if json_out {
                print_json(&json!({ "playlist": p, "matches": matches }));
            } else {
                let n = ids.len();
                println!("Added {n} song{}; {} now has {}.", if n == 1 { "" } else { "s" }, p["name"].as_str().unwrap_or(""), p["songCount"]);
            }
        }
        PlaylistAction::Remove { playlist, positions: given } => {
            let p = find(&sub, &playlist)?;
            let id = id_of(&p);
            let remove = positions(&given, p["songCount"].as_u64().unwrap_or(0) as usize)?;
            sub.update_playlist(&id, &PlaylistChange { remove, ..Default::default() })?;
            let p = sub.playlist(&id)?;
            if json_out { print_json(&p) } else { println!("Done; {} songs left.", p["songCount"]) }
        }
        PlaylistAction::Move { playlist, from, to } => {
            let p = sub.playlist(&id_of(&find(&sub, &playlist)?))?;
            let mut ids: Vec<String> = p["entry"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
            let at = positions(&[from, to], ids.len())?;
            let song = ids.remove(at[0]);
            ids.insert(at[1], song);
            let p = sub.set_playlist_songs(&id_of(&p), &ids)?;
            if json_out { print_json(&p) } else { print_songs(&p) }
        }
        PlaylistAction::Rename { playlist, name } => {
            let id = id_of(&find(&sub, &playlist)?);
            sub.update_playlist(&id, &PlaylistChange { name: Some(name.clone()), ..Default::default() })?;
            if !json_out {
                println!("Renamed to \"{name}\".");
            }
        }
        PlaylistAction::Share { playlist, users, nobody, public, private } => {
            let id = id_of(&find(&sub, &playlist)?);
            let change = PlaylistChange {
                shared_with: (nobody || !users.is_empty()).then_some(users),
                public: if public { Some(true) } else if private { Some(false) } else { None },
                ..Default::default()
            };
            if change.shared_with.is_some() || change.public.is_some() {
                sub.update_playlist(&id, &change)?;
            }
            let p = sub.playlist(&id)?;
            if json_out {
                print_json(&json!({ "id": id, "public": p["public"], "sharedWith": p["sharedWith"] }));
            } else {
                print_playlist_line(&p);
            }
        }
        PlaylistAction::Delete { playlist } => {
            let p = find(&sub, &playlist)?;
            sub.delete_playlist(&id_of(&p))?;
            if !json_out {
                println!("Deleted \"{}\".", p["name"].as_str().unwrap_or(""));
            }
        }
        PlaylistAction::Users => {
            let users = sub.users()?;
            if json_out { print_json(&json!(users)) } else { users.iter().for_each(|u| println!("{u}")) }
        }
    }
    Ok(true)
}
