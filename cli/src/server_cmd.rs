//! `localbeat server`, `localbeat outbox`, `localbeat push`: the LocalBeat
//! server that holds the library, and the albums waiting to go to it
//! (docs/server-library.md).

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Subcommand;
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::outbox::{self, Outbox, State};
use localbeat_core::subsonic::Subsonic;
use localbeat_engine::push::{self, PushEvent};

#[derive(Subcommand)]
pub enum ServerAction {
    /// Set where rips go (only the options given change)
    Set {
        /// The server's address on the home network, e.g. 192.168.1.10
        #[arg(long)]
        host: Option<String>,
        /// The SSH user that owns the music, e.g. musicuser
        #[arg(long)]
        user: Option<String>,
        /// This computer's writer key, e.g. ~/.ssh/localbeat-studio
        #[arg(long)]
        key: Option<String>,
        /// The music server, e.g. http://192.168.1.10:4533 (for scanning after a push)
        #[arg(long)]
        url: Option<String>,
        /// This computer's login on the music server
        #[arg(long)]
        login: Option<String>,
        /// Its secret
        #[arg(long)]
        secret: Option<String>,
    },
    /// Stop sending rips anywhere (the outbox is kept)
    Off,
    /// Check that the server can be reached and written to
    Check,
    /// Read the server's SSH host key and pin it (at home, once)
    Trust,
}

#[derive(Subcommand)]
pub enum OutboxAction {
    /// Queue albums already in the FLAC library (their folders)
    Add {
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
    },
}

pub fn server(action: Option<ServerAction>) -> Result<bool> {
    let s = Settings::load();
    let mut prefs = Prefs::load(&s);
    match action {
        None => {}
        Some(ServerAction::Set { host, user, key, url, login, secret }) => {
            let sv = &mut prefs.server;
            for (field, value) in [(&mut sv.host, host), (&mut sv.user, user), (&mut sv.key, key), (&mut sv.url, url), (&mut sv.login, login), (&mut sv.secret, secret)] {
                if let Some(v) = value {
                    *field = v.trim().to_string();
                }
            }
            prefs.save(&s)?;
        }
        Some(ServerAction::Off) => {
            prefs.server.host.clear();
            prefs.save(&s)?;
            println!("Rips stay on this computer now.");
            return Ok(true);
        }
        Some(ServerAction::Check) => return check(&s, &prefs),
        Some(ServerAction::Trust) => return trust(&s, &prefs),
    }
    let sv = &prefs.server;
    if !sv.enabled() {
        println!("No server: rips stay on this computer.");
        println!("Set one: localbeat server set --host 192.168.1.10 --user musicuser --key ~/.ssh/localbeat-studio");
        return Ok(true);
    }
    println!("Rips go to {}@{}:{} (key {})", sv.user, sv.host, "FLAC/", sv.key);
    if !sv.key_path().is_file() {
        println!("  but the key {} doesn't exist", sv.key_path().display());
    }
    match (sv.url.is_empty(), sv.login.is_empty() || sv.secret.is_empty()) {
        (false, false) => println!("Music server {} (login {}): asked to scan after each push", sv.url, sv.login),
        _ => println!("No music server login: it finds new albums within 10 minutes"),
    }
    Ok(true)
}

fn check(s: &Settings, prefs: &Prefs) -> Result<bool> {
    let sv = &prefs.server;
    if !sv.enabled() {
        bail!("no server set up (localbeat server set ...)");
    }
    if let Err(why) = push::reachable(sv) {
        println!("Not reachable: {why}");
        return Ok(false);
    }
    println!("{}: reachable", sv.host);
    // An empty, dry-run push of an empty folder proves the key works.
    let empty = s.cache_dir.join("server-check");
    std::fs::create_dir_all(&empty)?;
    let rs = push::Rsync::find()?;
    let out = rs
        .command()
        .args(["-a", "--dry-run", "-e"])
        .arg(push::ssh_command(s, sv, &rs))
        .arg(format!("{}/", rs.path(&empty)))
        .arg(format!("{}@{}:FLAC/", sv.user, push::host_port(sv).0))
        .output()?;
    if out.status.success() {
        println!("Writing: allowed (key {})", sv.key);
    } else {
        println!("Writing: refused: {}", String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or(""));
        return Ok(false);
    }
    match Subsonic::new(sv).map(|c| c.ping()) {
        Some(Ok(())) => println!("Music server: login works ({})", sv.url),
        Some(Err(e)) => println!("Music server: {e:#}"),
        None => println!("Music server: no login set (it finds new albums within 10 minutes)"),
    }
    Ok(true)
}

fn trust(s: &Settings, prefs: &Prefs) -> Result<bool> {
    let sv = &prefs.server;
    if sv.host.is_empty() {
        bail!("no server set up (localbeat server set --host ...)");
    }
    let (lines, fp) = push::scan_host_key(s, sv)?;
    println!("{} answers with this host key:\n  {fp}", sv.host);
    println!("Compare it with the Mac's own (on the Mac: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub).");
    print!("Same? Pin it [y/N]: ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        println!("Not pinned.");
        return Ok(false);
    }
    push::save_host_key(s, &lines)?;
    println!("Pinned in {}.", push::known_hosts(s).display());
    Ok(true)
}

pub fn outbox(action: Option<OutboxAction>) -> Result<bool> {
    let s = Settings::load();
    if let Some(OutboxAction::Add { dirs }) = action {
        let mut names = Vec::new();
        for d in dirs {
            let d = std::fs::canonicalize(&d).unwrap_or(d);
            let lib = std::fs::canonicalize(&s.flac_dir).unwrap_or(s.flac_dir.clone());
            let mut s2 = s.clone();
            s2.flac_dir = lib;
            names.push(outbox::album_name(&s2, &d)?);
        }
        Outbox::update(&s, |o| names.iter().for_each(|n| o.add(n)))?;
        for n in &names {
            println!("queued: {n}");
        }
        return Ok(true);
    }
    let o = Outbox::load(&s);
    if o.albums.is_empty() {
        println!("The outbox is empty.");
        return Ok(true);
    }
    for e in &o.albums {
        match &e.state {
            State::Waiting => println!("waiting  {}  (since {})", e.album, e.added),
            State::Failed { at, error } => println!("failed   {}  ({at}: {error})", e.album),
            State::Sent { at } => println!("sent     {}  ({at})", e.album),
        }
    }
    println!("{} waiting", o.pending().len());
    Ok(true)
}

pub fn push() -> Result<bool> {
    let s = Settings::load();
    let prefs = Prefs::load(&s);
    let sum = push::push_pending(&s, &prefs.server, &mut |e| match e {
        PushEvent::Sending(a) => println!("sending  {a}"),
        PushEvent::Sent(a) => println!("sent     {a}"),
        PushEvent::Failed { album, error } => println!("FAILED   {album}: {error}"),
        PushEvent::NotHome { waiting, why } => println!("{waiting} waiting: {why}"),
        PushEvent::Busy(a) => println!("later    {a} (a disc is being ripped into it)"),
        PushEvent::Scan(Ok(())) => println!("The server has scanned for the new albums."),
        PushEvent::Scan(Err(e)) => println!("Scan: {e}"),
    })?;
    if sum.already_running {
        println!("Another push is running.");
    } else if sum.waiting == 0 && sum.sent == 0 && sum.failed == 0 {
        println!("Nothing waiting.");
    }
    Ok(sum.failed == 0)
}
