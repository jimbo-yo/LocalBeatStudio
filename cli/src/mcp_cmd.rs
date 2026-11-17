//! `localbeat mcp`: an MCP server over stdio, so an AI (Claude Code, Claude
//! Desktop…) can search the library on the music server, match song lists
//! found elsewhere, and build and change the user's playlists there.
//! Design: LocalBeat docs/control.md, step 3. Register it with
//! `claude mcp add localbeat -- localbeat mcp`.
//!
//! JSON-RPC 2.0, one message per line on stdin/stdout (the MCP stdio
//! transport); anything else goes to stderr. Same calls and login as
//! `localbeat playlist` (this computer's music server login).

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use localbeat_core::config::Settings;
use localbeat_core::server_library;
use localbeat_core::song_match::{Candidate, Matcher, Status, norm};
use localbeat_core::subsonic::{PlaylistChange, Subsonic};
use serde_json::{Value, json};

const PROTOCOL: &str = "2025-06-18";
/// The library is read again after this long (an album may have arrived).
const LIBRARY_FRESH: Duration = Duration::from_secs(600);

const INSTRUCTIONS: &str = "LocalBeat is the user's own music library on their home music server. \
Playlists made here are the user's and show up on all their devices (phone, LocalBeat Studio). \
To make a playlist from songs you know or found elsewhere (a web page, your own ideas): call match_songs \
with one 'Artist - Title' per line; only 'found' songs are certainly in the library, 'close' ones are \
suggestions to confirm with the user, 'not_found' ones aren't there. Tell the user what was found and \
what is missing, then create_playlist with the found song IDs (in the order wanted). Use search_songs \
and library_summary to see what the library holds (genres, decades by original release year). \
Song and playlist IDs come only from these tools. Ask the user before deleting a playlist.";

pub fn run() -> Result<bool> {
    let mut server = Server::default();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    eprintln!("localbeat mcp: ready (stdio)");
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(true)
}

/// A library song as the tools show it.
struct Song {
    id: String,
    artist: String,
    title: String,
    album: String,
    /// The album's original release year (0: unknown).
    year: u32,
    genres: Vec<String>,
    seconds: u64,
    suffix: String,
}

struct Library {
    read: Instant,
    songs: Vec<Song>,
    candidates: Vec<Candidate>,
    albums: usize,
}

#[derive(Default)]
pub struct Server {
    sub: Option<Subsonic>,
    lib: Option<Library>,
}

fn rpc_result(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn rpc_error(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

impl Server {
    /// One incoming message; the reply to write, if it needs one.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(rpc_error(&Value::Null, -32700, &format!("not JSON: {e}"))),
        };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        // Notifications (no id) and replies to us need no answer.
        let id = id?;
        let params = &msg["params"];
        Some(match method {
            "initialize" => rpc_result(
                &id,
                json!({
                    "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "localbeat", "title": "LocalBeat", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            ),
            "ping" => rpc_result(&id, json!({})),
            "tools/list" => rpc_result(&id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let empty = json!({});
                let args = if params["arguments"].is_object() { &params["arguments"] } else { &empty };
                let (text, failed) = match self.call(name, args) {
                    Ok(v) => (if let Value::String(s) = v { s } else { v.to_string() }, false),
                    Err(e) => (format!("{e:#}"), true),
                };
                rpc_result(&id, json!({ "content": [{ "type": "text", "text": text }], "isError": failed }))
            }
            "" => rpc_error(&id, -32600, "no method"),
            other => rpc_error(&id, -32601, &format!("unknown method {other}")),
        })
    }

    fn sub(&mut self) -> Result<&Subsonic> {
        if self.sub.is_none() {
            let s = Settings::load();
            let (c, _) = server_library::client(&s)
                .ok_or_else(|| anyhow!("this computer has no music server login (LocalBeat Studio: Settings, join the server)"))?;
            self.sub = Some(c);
        }
        Ok(self.sub.as_ref().unwrap())
    }

    fn library(&mut self) -> Result<&Library> {
        if self.lib.as_ref().is_none_or(|l| l.read.elapsed() > LIBRARY_FRESH) {
            let sub = self.sub()?;
            let albums = sub.all_albums()?;
            let years: HashMap<String, u32> =
                albums.iter().filter_map(|a| Some((a["id"].as_str()?.to_string(), a["year"].as_u64().unwrap_or(0) as u32))).collect();
            let raw = sub.all_songs()?;
            let st = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
            let songs: Vec<Song> = raw
                .iter()
                .filter(|v| v["id"].is_string())
                .map(|v| {
                    let mut genres: Vec<String> =
                        v["genres"].as_array().into_iter().flatten().filter_map(|g| g["name"].as_str().map(str::to_string)).collect();
                    if genres.is_empty() && !st(v, "genre").is_empty() {
                        genres.push(st(v, "genre"));
                    }
                    let year = v["albumId"].as_str().and_then(|a| years.get(a)).copied().filter(|&y| y > 0).unwrap_or(v["year"].as_u64().unwrap_or(0) as u32);
                    Song {
                        id: st(v, "id"),
                        artist: st(v, "artist"),
                        title: st(v, "title"),
                        album: st(v, "album"),
                        year,
                        genres,
                        seconds: v["duration"].as_u64().unwrap_or(0),
                        suffix: st(v, "suffix"),
                    }
                })
                .collect();
            let candidates = raw.iter().filter_map(Candidate::from_json).collect();
            eprintln!("localbeat mcp: library read, {} songs", songs.len());
            self.lib = Some(Library { read: Instant::now(), songs, candidates, albums: albums.len() });
        }
        Ok(self.lib.as_ref().unwrap())
    }

    pub fn call(&mut self, name: &str, a: &Value) -> Result<Value> {
        match name {
            "library_summary" => self.summary(),
            "list_artists" => self.artists(a),
            "search_songs" => self.search(a),
            "match_songs" => self.match_songs(a),
            "list_playlists" => {
                let list = self.sub()?.playlists()?;
                Ok(json!(list.iter().map(playlist_summary).collect::<Vec<_>>()))
            }
            "get_playlist" => {
                let sub = self.sub()?;
                let p = sub.playlist(&playlist_id(sub, a)?)?;
                Ok(playlist_with_songs(&p))
            }
            "create_playlist" => {
                let name = text(a, "name")?;
                let ids = ids(a, "song_ids")?;
                let sub = self.sub()?;
                let p = sub.create_playlist(&name, &ids)?;
                if let Some(c) = a["comment"].as_str() {
                    sub.update_playlist(p["id"].as_str().unwrap_or(""), &PlaylistChange { comment: Some(c.to_string()), ..Default::default() })?;
                }
                Ok(playlist_summary(&p))
            }
            "add_to_playlist" => {
                let ids = ids(a, "song_ids")?;
                if ids.is_empty() {
                    bail!("song_ids is empty");
                }
                self.change(a, PlaylistChange { add: ids, ..Default::default() })
            }
            "remove_from_playlist" => {
                let sub = self.sub()?;
                let p = sub.playlist(&playlist_id(sub, a)?)?;
                let count = p["songCount"].as_u64().unwrap_or(0) as usize;
                let mut remove = Vec::new();
                for n in a["positions"].as_array().ok_or_else(|| anyhow!("positions: a list of numbers, 1 = the first song"))? {
                    match n.as_u64().map(|n| n as usize) {
                        Some(n) if n >= 1 && n <= count => remove.push(n - 1),
                        _ => bail!("no song at position {n} (the playlist has {count}; 1 = the first)"),
                    }
                }
                sub.update_playlist(p["id"].as_str().unwrap_or(""), &PlaylistChange { remove, ..Default::default() })?;
                Ok(playlist_summary(&sub.playlist(p["id"].as_str().unwrap_or(""))?))
            }
            "set_playlist_songs" => {
                let ids = ids(a, "song_ids")?;
                let sub = self.sub()?;
                let id = playlist_id(sub, a)?;
                Ok(playlist_summary(&sub.set_playlist_songs(&id, &ids)?))
            }
            "rename_playlist" => {
                let change = PlaylistChange {
                    name: a["name"].as_str().map(str::to_string),
                    comment: a["comment"].as_str().map(str::to_string),
                    ..Default::default()
                };
                if change.name.is_none() && change.comment.is_none() {
                    bail!("give a new name and/or comment");
                }
                self.change(a, change)
            }
            "share_playlist" => {
                let users = match &a["users"] {
                    Value::Null => None,
                    Value::Array(u) => Some(u.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()),
                    _ => bail!("users: a list of user names ([] to stop sharing)"),
                };
                let change = PlaylistChange { shared_with: users, public: a["public"].as_bool(), ..Default::default() };
                if change.shared_with.is_none() && change.public.is_none() {
                    bail!("give users and/or public");
                }
                self.change(a, change)
            }
            "delete_playlist" => {
                let sub = self.sub()?;
                let id = playlist_id(sub, a)?;
                sub.delete_playlist(&id)?;
                Ok(json!({ "deleted": id }))
            }
            "list_users" => Ok(json!(self.sub()?.users()?)),
            other => bail!("no tool called {other}"),
        }
    }

    fn change(&mut self, a: &Value, c: PlaylistChange) -> Result<Value> {
        let sub = self.sub()?;
        let id = playlist_id(sub, a)?;
        sub.update_playlist(&id, &c)?;
        Ok(playlist_summary(&sub.playlist(&id)?))
    }

    fn summary(&mut self) -> Result<Value> {
        let lib = self.library()?;
        let mut genres: HashMap<&str, usize> = HashMap::new();
        let mut decades: BTreeMap<u32, usize> = BTreeMap::new();
        let mut artists: HashMap<&str, usize> = HashMap::new();
        let mut formats: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &lib.songs {
            for g in &s.genres {
                *genres.entry(g.as_str()).or_default() += 1;
            }
            *decades.entry(s.year / 10 * 10).or_default() += 1;
            *artists.entry(s.artist.as_str()).or_default() += 1;
            *formats.entry(s.suffix.as_str()).or_default() += 1;
        }
        let top = |m: HashMap<&str, usize>, n: usize| -> Vec<Value> {
            let mut v: Vec<(&str, usize)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            v.into_iter().take(n).map(|(k, c)| json!([k, c])).collect()
        };
        let artist_count = artists.len();
        let decades: Vec<Value> =
            decades.into_iter().map(|(d, c)| json!([if d == 0 { "unknown".to_string() } else { format!("{d}s") }, c])).collect();
        Ok(json!({
            "songs": lib.songs.len(),
            "albums": lib.albums,
            "artists": artist_count,
            "hours": lib.songs.iter().map(|s| s.seconds).sum::<u64>() / 3600,
            "formats": formats,
            "decades_by_original_release": decades,
            "top_genres_by_songs": top(genres, 40),
            "top_artists_by_songs": top(artists, 30),
        }))
    }

    fn artists(&mut self, a: &Value) -> Result<Value> {
        let genre = a["genre"].as_str().map(norm);
        let min = a["min_songs"].as_u64().unwrap_or(1) as usize;
        let lib = self.library()?;
        let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for s in &lib.songs {
            if genre.as_ref().is_some_and(|g| !s.genres.iter().any(|x| norm(x).contains(g.as_str()))) {
                continue;
            }
            counts.entry(norm(&s.artist)).or_insert_with(|| (s.artist.clone(), 0)).1 += 1;
        }
        let lines: Vec<String> = counts.into_values().filter(|(_, n)| *n >= min).map(|(name, n)| format!("{name} ({n})")).collect();
        Ok(Value::String(format!("{} artists (songs in brackets):\n{}", lines.len(), lines.join("\n"))))
    }

    fn search(&mut self, a: &Value) -> Result<Value> {
        let field = |k: &str| a[k].as_str().map(norm).filter(|s| !s.is_empty());
        let (query, artist, album, genre) = (field("query"), field("artist"), field("album"), field("genre"));
        let (mut from, mut to) = (a["year_from"].as_u64().map(|y| y as u32), a["year_to"].as_u64().map(|y| y as u32));
        if let Some(d) = decade(&a["decade"])? {
            from = Some(d);
            to = Some(d + 9);
        }
        let offset = a["offset"].as_u64().unwrap_or(0) as usize;
        let limit = a["limit"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
        let lib = self.library()?;
        let hits: Vec<&Song> = lib
            .songs
            .iter()
            .filter(|s| {
                query.as_ref().is_none_or(|q| [&s.artist, &s.title, &s.album].iter().any(|x| norm(x).contains(q.as_str())))
                    && artist.as_ref().is_none_or(|q| norm(&s.artist).contains(q.as_str()))
                    && album.as_ref().is_none_or(|q| norm(&s.album).contains(q.as_str()))
                    && genre.as_ref().is_none_or(|q| s.genres.iter().any(|g| norm(g).contains(q.as_str())))
                    && from.is_none_or(|y| s.year >= y)
                    && to.is_none_or(|y| s.year > 0 && s.year <= y)
            })
            .collect();
        let total = hits.len();
        let mut sorted = hits;
        sorted.sort_by_cached_key(|s| (norm(&s.artist), s.year, norm(&s.album), norm(&s.title)));
        let page: Vec<String> = sorted.into_iter().skip(offset).take(limit).map(song_line).collect();
        let shown = if page.is_empty() { "none shown".to_string() } else { format!("showing {}-{}", offset + 1, offset + page.len()) };
        Ok(Value::String(format!(
            "{total} songs match ({shown}). Lines: id | artist | title | album | original year | genres | length\n{}",
            page.join("\n")
        )))
    }

    fn match_songs(&mut self, a: &Value) -> Result<Value> {
        let text = match &a["songs"] {
            Value::String(s) => s.clone(),
            Value::Array(v) => v.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
            _ => bail!("songs: one 'Artist - Title' per line, as a string or a list of strings"),
        };
        let lib = self.library()?;
        let matches = Matcher::new(&lib.candidates).match_text(&text);
        if matches.is_empty() {
            bail!("no songs in the list (one 'Artist - Title' per line)");
        }
        let count = |st: Status| matches.iter().filter(|m| m.status == st).count();
        let lines: Vec<Value> = matches
            .iter()
            .map(|m| {
                json!({
                    "line": m.line,
                    "status": m.status,
                    "note": m.note,
                    "songs": m.songs.iter().take(3).map(|c| json!({
                        "id": c.id, "artist": c.artist, "title": c.title, "album": c.album, "format": c.suffix
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({
            "found": count(Status::Found),
            "close": count(Status::Close),
            "not_found": count(Status::NotFound),
            "note": "For 'found' the first song is the best version (original album, FLAC). 'close' songs are only suggestions: confirm with the user.",
            "results": lines,
        }))
    }
}

/// "1970", 1970, "1970s", "70s", "'70s".
fn decade(v: &Value) -> Result<Option<u32>> {
    let n = match v {
        Value::Null => return Ok(None),
        Value::Number(n) => n.as_u64().unwrap_or(0) as u32,
        Value::String(s) => {
            let d: String = s.chars().filter(char::is_ascii_digit).collect();
            d.parse::<u32>().map_err(|_| anyhow!("decade: like 1970 or \"1970s\""))?
        }
        _ => bail!("decade: like 1970 or \"1970s\""),
    };
    let n = if n < 100 { if n >= 30 { 1900 + n } else { 2000 + n } } else { n };
    Ok(Some(n / 10 * 10))
}

fn song_line(s: &Song) -> String {
    let year = if s.year > 0 { s.year.to_string() } else { "?".into() };
    format!("{} | {} | {} | {} | {} | {} | {}:{:02}", s.id, s.artist, s.title, s.album, year, s.genres.join(", "), s.seconds / 60, s.seconds % 60)
}

fn text(a: &Value, k: &str) -> Result<String> {
    a[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| anyhow!("{k} is needed"))
}

fn ids(a: &Value, k: &str) -> Result<Vec<String>> {
    match &a[k] {
        Value::Null => Ok(Vec::new()),
        Value::Array(v) => v.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| anyhow!("{k}: song IDs (strings)"))).collect(),
        _ => bail!("{k}: a list of song IDs"),
    }
}

/// The `playlist` argument: an ID or a name.
fn playlist_id(sub: &Subsonic, a: &Value) -> Result<String> {
    let wanted = text(a, "playlist")?;
    let p = crate::playlist_cmd::find(sub, &wanted)?;
    Ok(p["id"].as_str().unwrap_or_default().to_string())
}

fn playlist_summary(p: &Value) -> Value {
    let mut v = json!({
        "id": p["id"], "name": p["name"], "owner": p["owner"], "songs": p["songCount"],
        "minutes": p["duration"].as_u64().unwrap_or(0) / 60, "public": p["public"],
        "yours": p["readonly"] != true,
    });
    for k in ["comment", "sharedWith", "missing"] {
        if p[k].as_str().is_some_and(|s| !s.is_empty()) || p[k].as_array().is_some_and(|a| !a.is_empty()) || p[k].as_u64().is_some_and(|n| n > 0) {
            v[k] = p[k].clone();
        }
    }
    v
}

fn playlist_with_songs(p: &Value) -> Value {
    let mut v = playlist_summary(p);
    let lines: Vec<String> = p["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "{}. {} | {} | {} | {}",
                i + 1,
                s["id"].as_str().unwrap_or(""),
                s["artist"].as_str().unwrap_or(""),
                s["title"].as_str().unwrap_or(""),
                s["album"].as_str().unwrap_or("")
            )
        })
        .collect();
    v["song_lines"] = json!("position. id | artist | title | album");
    v["songs_in_order"] = json!(lines);
    v
}

fn tool(name: &str, title: &str, description: &str, read_only: bool, destructive: bool, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": { "title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "openWorldHint": false },
    })
}

pub fn tools() -> Vec<Value> {
    let playlist = json!({ "type": "string", "description": "The playlist's ID (from list_playlists), or its exact name" });
    let song_ids = json!({ "type": "array", "items": { "type": "string" }, "description": "Song IDs from match_songs or search_songs, in order" });
    vec![
        tool("library_summary", "Library summary",
            "What the music library holds: numbers of songs, albums and artists, formats, songs per decade (original release), the biggest genres and artists.",
            true, false, json!({}), &[]),
        tool("list_artists", "List artists",
            "Every artist in the library with their number of songs, optionally only those with a genre.",
            true, false, json!({
                "genre": { "type": "string", "description": "Only songs with this genre (part of the name is enough)" },
                "min_songs": { "type": "integer", "description": "Leave out artists with fewer songs" },
            }), &[]),
        tool("search_songs", "Search songs",
            "Songs in the library, filtered: text in artist/title/album, artist, album, genre, decade or years (original release). Compact lines with song IDs; paged.",
            true, false, json!({
                "query": { "type": "string", "description": "Text anywhere in artist, title or album" },
                "artist": { "type": "string" },
                "album": { "type": "string" },
                "genre": { "type": "string" },
                "decade": { "type": ["integer", "string"], "description": "e.g. 1970 or \"70s\"" },
                "year_from": { "type": "integer" },
                "year_to": { "type": "integer" },
                "offset": { "type": "integer", "description": "Skip this many (paging)" },
                "limit": { "type": "integer", "description": "At most this many (default 100, max 500)" },
            }), &[]),
        tool("match_songs", "Match songs",
            "Which songs of a list are in the library. One 'Artist - Title' per line (also '\"Title\" by Artist', list numbering is fine). Each line: found (with the best version first), close (suggestions to confirm) or not_found. Use before create_playlist.",
            true, false, json!({
                "songs": { "type": ["string", "array"], "items": { "type": "string" }, "description": "The list: a string with one song per line, or a list of strings" },
            }), &["songs"]),
        tool("list_playlists", "List playlists",
            "The user's playlists on the music server, then those shared with them and public ones (yours=false: read only).",
            true, false, json!({}), &[]),
        tool("get_playlist", "Show a playlist", "A playlist and its songs in order (positions from 1).",
            true, false, json!({ "playlist": playlist }), &["playlist"]),
        tool("create_playlist", "Create a playlist",
            "A new playlist of the user's with these songs in this order. It appears on all their devices.",
            false, false, json!({ "name": { "type": "string" }, "song_ids": song_ids, "comment": { "type": "string" } }), &["name", "song_ids"]),
        tool("add_to_playlist", "Add to a playlist", "Adds songs at the end of a playlist.",
            false, false, json!({ "playlist": playlist, "song_ids": song_ids }), &["playlist", "song_ids"]),
        tool("remove_from_playlist", "Remove from a playlist", "Takes songs out by position (1 = the first, as get_playlist shows).",
            false, true, json!({ "playlist": playlist, "positions": { "type": "array", "items": { "type": "integer" } } }), &["playlist", "positions"]),
        tool("set_playlist_songs", "Set a playlist's songs",
            "Replaces all of a playlist's songs with these, in this order (to reorder: get_playlist, then send the IDs in the new order).",
            false, true, json!({ "playlist": playlist, "song_ids": song_ids }), &["playlist", "song_ids"]),
        tool("rename_playlist", "Rename a playlist", "Changes a playlist's name and/or comment.",
            false, false, json!({ "playlist": playlist, "name": { "type": "string" }, "comment": { "type": "string" } }), &["playlist"]),
        tool("share_playlist", "Share a playlist",
            "Shares a playlist with other users of the server (they can play it, not change it; the list replaces the old one, [] stops sharing), and/or makes it public (everyone).",
            false, false, json!({ "playlist": playlist, "users": { "type": "array", "items": { "type": "string" } }, "public": { "type": "boolean" } }), &["playlist"]),
        tool("delete_playlist", "Delete a playlist", "Deletes one of the user's playlists from all their devices. Ask the user first.",
            false, true, json!({ "playlist": playlist }), &["playlist"]),
        tool("list_users", "List users", "The users of the music server (to share a playlist with).",
            true, false, json!({}), &[]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(s: &mut Server, msg: Value) -> Value {
        serde_json::from_str(&s.handle_line(&msg.to_string()).expect("a reply")).unwrap()
    }

    #[test]
    fn protocol() {
        let mut s = Server::default();
        let r = reply(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}));
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "localbeat");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let r = reply(&mut s, json!({"jsonrpc": "2.0", "id": "x", "method": "tools/list"}));
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 14);
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            for req in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(t["inputSchema"]["properties"].get(req.as_str().unwrap()).is_some(), "{} needs {req}", t["name"]);
            }
        }
        assert_eq!(reply(&mut s, json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))["result"], json!({}));
        assert_eq!(reply(&mut s, json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}))["error"]["code"], -32601);
        assert_eq!(serde_json::from_str::<Value>(&s.handle_line("{nope").unwrap()).unwrap()["error"]["code"], -32700);
        // A tool error is a result with isError, not a protocol error.
        let r = reply(&mut s, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "no_such", "arguments": {}}}));
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("no tool called"));
    }

    #[test]
    fn decades() {
        assert_eq!(decade(&json!(1975)).unwrap(), Some(1970));
        assert_eq!(decade(&json!("1980s")).unwrap(), Some(1980));
        assert_eq!(decade(&json!("'70s")).unwrap(), Some(1970));
        assert_eq!(decade(&json!("00s")).unwrap(), Some(2000));
        assert_eq!(decade(&Value::Null).unwrap(), None);
        assert!(decade(&json!("soon")).is_err());
    }
}
