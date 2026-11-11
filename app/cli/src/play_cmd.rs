//! `localbeat play`: the player engine from the command line (for testing it
//! on real files and speakers before the window has a player).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use localbeat_player::{Player, State};

const EXTS: [&str; 8] = ["flac", "mp3", "m4a", "ogg", "wav", "aif", "aiff", "m4p"];

fn expand(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(p)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .filter(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str())))
                .collect();
            files.sort();
            out.extend(files);
        } else {
            out.push(p.clone());
        }
    }
    out
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn run(paths: Vec<PathBuf>, seek: Option<f64>, volume: Option<f32>) -> Result<bool> {
    let queue = expand(&paths);
    if queue.is_empty() {
        bail!("nothing to play");
    }
    println!("{} song(s). Keys (then Enter): p pause/resume, n next, b back, f +10 s, r -10 s, q quit", queue.len());
    let player = Player::new();
    if let Some(v) = volume {
        player.set_volume(v);
    }
    player.load(queue, 0);
    if let Some(s) = seek {
        player.seek(s);
    }
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
            if tx.send(line.trim().to_string()).is_err() {
                break;
            }
        }
    });
    let mut started = false;
    let mut last_index = None;
    let mut last_error = None;
    loop {
        while let Ok(k) = rx.try_recv() {
            let pos = player.status().position;
            match k.as_str() {
                "p" => player.toggle(),
                "n" => player.next(),
                "b" => player.prev(),
                "f" => player.seek(pos + 10.0),
                "r" => player.seek((pos - 10.0).max(0.0)),
                "q" => {
                    println!();
                    return Ok(true);
                }
                _ => {}
            }
        }
        let s = player.status();
        if s.error != last_error {
            if let Some(e) = &s.error {
                println!("\n  ! {e}");
            }
            last_error = s.error.clone();
        }
        if s.state != State::Stopped {
            started = true;
        } else if started {
            println!("\nEnd of the queue.");
            return Ok(last_error.is_none());
        }
        if s.index != last_index && s.index.is_some() {
            let name = s.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let fmt = s.format.as_ref().map(|f| match f.bits {
                Some(b) => format!("{} {}/{:.1} kHz", f.codec, b, f.sample_rate as f64 / 1000.0),
                None => format!("{} {:.1} kHz", f.codec, f.sample_rate as f64 / 1000.0),
            });
            println!("\n▶ {}/{}  {name}  [{}]", s.index.unwrap() + 1, s.queue_len, fmt.unwrap_or_default());
            last_index = s.index;
        }
        let state = match s.state {
            State::Playing => "playing",
            State::Paused => "paused ",
            State::Stopped => "stopped",
        };
        print!("\r  {state}  {} / {}   ", mmss(s.position), s.duration.map(mmss).unwrap_or_else(|| "?".into()));
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(250));
    }
}
