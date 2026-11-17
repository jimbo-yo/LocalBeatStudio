//! `localbeat player …`: the LocalBeat app on the phone, live, over adb
//! (USB or wireless debugging): what's playing, play/pause/skip/seek, and
//! playing a playlist, album or search. LocalBeat docs/control.md, step 7; the
//! phone side is its `ControlProvider` (adb only).

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use localbeat_platform::phone::control_call;
use serde_json::Value;

#[derive(Subcommand)]
pub enum PlayerAction {
    /// What's playing (the default)
    Status,
    /// Play: with nothing, carry on; else a playlist (name or ID), or words to
    /// search for ("Thunder Road", "Springsteen"), or a media ID (album:…)
    Play { what: Vec<String> },
    Pause,
    /// Play or pause
    Toggle,
    Next,
    Prev,
    /// Jump to a time in the song: 1:30, 90
    Seek { to: String },
    /// on, off (nothing: switch)
    Shuffle { on: Option<String> },
    /// off, all, one
    Repeat { mode: String },
    /// The songs queued, and which is playing
    Queue,
    /// The playlists on the phone (its own and the music server's)
    Playlists,
    /// The app's version and what it can do
    Version,
}

/// One call to the app; its JSON, or the error it gave.
pub fn call(method: &str, arg: Option<&str>) -> Result<Value> {
    let text = control_call(method, arg)?;
    let v: Value = serde_json::from_str(&text).map_err(|e| anyhow!("the phone's answer isn't JSON ({e}): {text}"))?;
    if let Some(e) = v["error"].as_str()
        && v.get("state").is_none()
    {
        bail!("LocalBeat on the phone: {e}");
    }
    Ok(v)
}

/// What to give `play`: a playlist found by name (or ID) becomes its media ID;
/// a media ID ("album:…") stays; anything else is searched for.
pub fn play_target(what: &str) -> Result<String> {
    let w = what.trim();
    if ["playlist:", "album:", "artist:", "t:", "genre:", "decade:", "search:"].iter().any(|p| w.starts_with(p)) {
        return Ok(w.to_string());
    }
    let lists = call("playlists", None)?;
    let all = lists["playlists"].as_array().cloned().unwrap_or_default();
    let found = all
        .iter()
        .find(|p| p["id"].as_str() == Some(w))
        .or_else(|| all.iter().find(|p| p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(w))));
    Ok(match found {
        Some(p) => p["media_id"].as_str().unwrap_or_default().to_string(),
        None => format!("search:{w}"),
    })
}

fn mmss(ms: i64) -> String {
    if ms < 0 {
        return "?".into();
    }
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn print_status(v: &Value) {
    let state = v["state"].as_str().unwrap_or("?");
    match v.get("song") {
        Some(s) => println!(
            "{state}: {} – {} ({})  {} / {}",
            s["artist"].as_str().unwrap_or(""),
            s["title"].as_str().unwrap_or(""),
            s["album"].as_str().unwrap_or(""),
            mmss(v["position_ms"].as_i64().unwrap_or(-1)),
            mmss(v["duration_ms"].as_i64().unwrap_or(-1)),
        ),
        None => println!("{state}: nothing queued"),
    }
    let size = v["queue_size"].as_i64().unwrap_or(0);
    if size > 0 {
        println!(
            "song {} of {size} · shuffle {} · repeat {}",
            v["queue_index"].as_i64().unwrap_or(-1) + 1,
            if v["shuffle"].as_bool() == Some(true) { "on" } else { "off" },
            v["repeat"].as_str().unwrap_or("off"),
        );
    }
    if let Some(e) = v["error"].as_str() {
        println!("player error: {e}");
    }
}

pub fn run(action: Option<PlayerAction>, json_out: bool) -> Result<bool> {
    let (v, kind) = match action.unwrap_or(PlayerAction::Status) {
        PlayerAction::Status => (call("status", None)?, "status"),
        PlayerAction::Play { what } => {
            let w = what.join(" ");
            let target = if w.trim().is_empty() { None } else { Some(play_target(&w)?) };
            (call("play", target.as_deref())?, "status")
        }
        PlayerAction::Pause => (call("pause", None)?, "status"),
        PlayerAction::Toggle => (call("toggle", None)?, "status"),
        PlayerAction::Next => (call("next", None)?, "status"),
        PlayerAction::Prev => (call("prev", None)?, "status"),
        PlayerAction::Seek { to } => (call("seek", Some(&to))?, "status"),
        PlayerAction::Shuffle { on } => (call("shuffle", on.as_deref())?, "status"),
        PlayerAction::Repeat { mode } => (call("repeat", Some(&mode))?, "status"),
        PlayerAction::Queue => (call("queue", None)?, "queue"),
        PlayerAction::Playlists => (call("playlists", None)?, "playlists"),
        PlayerAction::Version => (call("version", None)?, "version"),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(true);
    }
    match kind {
        "status" => print_status(&v),
        "queue" => {
            let now = v["index"].as_i64().unwrap_or(-1);
            for s in v["items"].as_array().into_iter().flatten() {
                let i = s["index"].as_i64().unwrap_or(0);
                println!(
                    "{}{:>4}. {} – {}",
                    if i == now { "▶" } else { " " },
                    i + 1,
                    s["artist"].as_str().unwrap_or(""),
                    s["title"].as_str().unwrap_or("")
                );
            }
        }
        "playlists" => {
            for p in v["playlists"].as_array().into_iter().flatten() {
                println!(
                    "{}  {}  ({} songs{})",
                    p["id"].as_str().unwrap_or(""),
                    p["name"].as_str().unwrap_or(""),
                    p["songs"],
                    if p["on_server"].as_bool() == Some(true) { ", music server" } else { ", on the phone" }
                );
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(&v)?),
    }
    Ok(true)
}
