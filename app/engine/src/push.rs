//! Sending ripped albums to the LocalBeat server (docs/server-library.md):
//! rsync over SSH to the server's home-network address with this computer's
//! writer key, then a checksum comparison, then a scan request. Writing only
//! works from the home network because the server only lets SSH in from
//! there; away from home the server simply can't be reached, and albums wait
//! in the outbox.

use std::ffi::OsString;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use localbeat_core::config::{ServerPrefs, Settings};
use localbeat_core::lock;
use localbeat_core::outbox::{self, Outbox};

/// Where albums go inside the server's music folder (the writer key's root).
const REMOTE_FLAC: &str = "FLAC";

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// On the server, checked.
    Sent,
    /// The server can't be reached: not on the home network (or the server
    /// is off). Not an error: tried again later.
    NotHome(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum PushEvent {
    Sending(String),
    Sent(String),
    Failed { album: String, error: String },
    /// Nothing sent: the server can't be reached right now.
    NotHome { waiting: usize, why: String },
    /// After the pushes: the server was asked to scan (or couldn't be).
    Scan(Result<(), String>),
    /// An album skipped this time: a disc is being ripped into it.
    Busy(String),
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub sent: usize,
    pub failed: usize,
    pub waiting: usize,
    /// Another push (another window, the command line) is running.
    pub already_running: bool,
}

/// Can we reach the server's SSH right now? A quick TCP connect, so being away
/// from home costs a few seconds, not an SSH timeout per album.
pub fn reachable(sp: &ServerPrefs) -> Result<(), String> {
    let addrs = (sp.host.as_str(), 22).to_socket_addrs().map_err(|e| format!("{}: {e}", sp.host))?;
    let mut last = String::from("no address");
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("{} can't be reached ({last}): not on the home network?", sp.host))
}

/// The ssh command rsync runs: this key only, never ask anything (no
/// password, no unknown host key: the server's key must already be known).
fn ssh_command(sp: &ServerPrefs) -> String {
    let key = sp.key_path().to_string_lossy().replace('\'', r"'\''");
    format!("ssh -i '{key}' -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10")
}

fn rsync_args(sp: &ServerPrefs, local: &Path, album: &str, check: bool) -> Vec<OsString> {
    let mut src = local.as_os_str().to_owned();
    src.push("/");
    let mut a: Vec<OsString> = ["-a", "--mkpath", "--partial", "--chmod=D755,F644", "--exclude=*.part", "--exclude=*.new"]
        .iter()
        .map(OsString::from)
        .collect();
    if check {
        a.extend(["--checksum", "--dry-run", "--itemize-changes"].map(OsString::from));
    }
    a.push("-e".into());
    a.push(ssh_command(sp).into());
    a.push(src);
    a.push(format!("{}@{}:{REMOTE_FLAC}/{album}/", sp.user, sp.host).into());
    a
}

fn rsync(args: &[OsString]) -> Result<Output> {
    Command::new("rsync").args(args).output().context("running rsync (is it installed?)")
}

/// What rsync's failure means: away from home, or a real problem.
fn classify(out: &Output) -> Outcome {
    let err = String::from_utf8_lossy(&out.stderr);
    let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    let code = out.status.code().unwrap_or(-1);
    if err.contains("Host key verification failed") || err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Outcome::Failed(
            "the server's SSH key isn't the one this computer knows: something else may be answering at that address. \
             Check before trusting it (ssh to it once by hand)"
                .into(),
        );
    }
    if err.contains("Permission denied") {
        return Outcome::Failed(
            "the server refused this computer's key: add it with localbeat-writers on the server".into(),
        );
    }
    let away = ["Connection timed out", "No route to host", "Network is unreachable", "Connection refused", "Could not resolve hostname"];
    if code == 255 && away.iter().any(|w| err.contains(w)) {
        return Outcome::NotHome(first);
    }
    Outcome::Failed(if first.is_empty() { format!("rsync failed (exit {code})") } else { first })
}

/// Sends one album and checks it arrived intact.
pub fn push_album(s: &Settings, sp: &ServerPrefs, album: &str) -> Outcome {
    let local = outbox::album_dir(s, album);
    if !local.is_dir() {
        return Outcome::Failed(format!("{} isn't on this computer any more", local.display()));
    }
    let out = match rsync(&rsync_args(sp, &local, album, false)) {
        Ok(o) => o,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    if !out.status.success() {
        return classify(&out);
    }
    // Check: comparing checksums, rsync must find nothing left to send.
    let out = match rsync(&rsync_args(sp, &local, album, true)) {
        Ok(o) => o,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    if !out.status.success() {
        return classify(&out);
    }
    let differ: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if !differ.is_empty() {
        return Outcome::Failed(format!(
            "sent, but {} item(s) differ on the server when checked ({}): sent again next time",
            differ.len(),
            differ.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        ));
    }
    Outcome::Sent
}

/// Asks the music server to look for new albums now (else it finds them
/// within its 10-minute rescan). Needs a login in the server settings.
pub fn start_scan(sp: &ServerPrefs) -> Result<()> {
    if sp.url.is_empty() || sp.login.is_empty() || sp.secret.is_empty() {
        bail!("no music server login set: it finds the new albums within 10 minutes");
    }
    let pw: String = sp.secret.bytes().map(|b| format!("{b:02x}")).collect();
    let url = format!(
        "{}/rest/startScan?u={}&p=enc:{pw}&v=1.16.1&c=LocalBeatStudio&f=json",
        sp.url.trim_end_matches('/'),
        url_encode(&sp.login)
    );
    let body: serde_json::Value = localbeat_core::http::agent()
        .get(&url)
        .call()
        .context("asking the server to scan")?
        .body_mut()
        .read_json()
        .context("the server's answer")?;
    let r = &body["subsonic-response"];
    if r["status"] != "ok" {
        bail!("the server refused: {}", r["error"]["message"].as_str().unwrap_or("unknown error"));
    }
    Ok(())
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Sends everything waiting in the outbox. One push at a time across
/// processes; albums a disc is being ripped into are left for next time.
pub fn push_pending(s: &Settings, sp: &ServerPrefs, ev: &mut dyn FnMut(PushEvent)) -> Result<Summary> {
    let mut sum = Summary::default();
    if !sp.enabled() {
        bail!("no server set up (localbeat server set ...)");
    }
    let Some(_lock) = lock::try_lock(&s.cache_dir, "server-push")? else {
        sum.already_running = true;
        return Ok(sum);
    };
    let pending = Outbox::load(s).pending();
    sum.waiting = pending.len();
    if pending.is_empty() {
        return Ok(sum);
    }
    if let Err(why) = reachable(sp) {
        ev(PushEvent::NotHome { waiting: pending.len(), why });
        return Ok(sum);
    }
    for album in pending {
        // A disc being ripped into it: next time.
        let Some(_ripping) = lock::try_lock(&s.cache_dir, &outbox::ripping_lock_name(&album))? else {
            ev(PushEvent::Busy(album));
            continue;
        };
        ev(PushEvent::Sending(album.clone()));
        match push_album(s, sp, &album) {
            Outcome::Sent => {
                Outbox::update(s, |o| o.sent(&album))?;
                sum.sent += 1;
                sum.waiting -= 1;
                ev(PushEvent::Sent(album));
            }
            Outcome::NotHome(why) => {
                // Lost the network part way: the rest waits too.
                ev(PushEvent::NotHome { waiting: sum.waiting, why });
                break;
            }
            Outcome::Failed(error) => {
                Outbox::update(s, |o| o.failed(&album, &error))?;
                sum.failed += 1;
                ev(PushEvent::Failed { album, error });
            }
        }
    }
    if sum.sent > 0 {
        ev(PushEvent::Scan(start_scan(sp).map_err(|e| format!("{e:#}"))));
    }
    Ok(sum)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn out(code: i32, stderr: &str) -> Output {
        Output { status: ExitStatus::from_raw(code << 8), stdout: vec![], stderr: stderr.as_bytes().to_vec() }
    }

    fn prefs() -> ServerPrefs {
        ServerPrefs { host: "192.168.1.10".into(), user: "musicuser".into(), key: "/home/me/.ssh/it's".into(), ..Default::default() }
    }

    #[test]
    fn away_from_home_is_not_an_error() {
        let o = classify(&out(255, "ssh: connect to host 192.168.1.10 port 22: Connection timed out\nrsync: connection unexpectedly closed"));
        assert!(matches!(o, Outcome::NotHome(_)), "{o:?}");
        assert!(matches!(classify(&out(255, "ssh: connect to host x port 22: No route to host")), Outcome::NotHome(_)));
    }

    #[test]
    fn a_strange_server_or_a_refused_key_is() {
        let o = classify(&out(255, "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed."));
        assert!(matches!(&o, Outcome::Failed(m) if m.contains("SSH key")), "{o:?}");
        let o = classify(&out(255, "musicuser@192.168.1.10: Permission denied (publickey)."));
        assert!(matches!(&o, Outcome::Failed(m) if m.contains("localbeat-writers")), "{o:?}");
        let o = classify(&out(23, "rsync: [Receiver] mkstemp failed: No space left on device (28)"));
        assert!(matches!(&o, Outcome::Failed(m) if m.contains("No space")), "{o:?}");
    }

    #[test]
    fn rsync_arguments() {
        let a = rsync_args(&prefs(), Path::new("/m/FLAC/Bob Dylan/Blood on the Tracks"), "Bob Dylan/Blood on the Tracks", false);
        let a: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(a.contains(&"--mkpath".to_string()) && a.contains(&"--exclude=*.part".to_string()));
        assert!(!a.iter().any(|x| x.contains("--delete")), "never deletes (the key couldn't anyway)");
        assert_eq!(a[a.len() - 2], "/m/FLAC/Bob Dylan/Blood on the Tracks/");
        assert_eq!(a[a.len() - 1], "musicuser@192.168.1.10:FLAC/Bob Dylan/Blood on the Tracks/");
        let ssh = &a[a.len() - 3];
        assert!(ssh.contains(r"-i '/home/me/.ssh/it'\''s'") && ssh.contains("BatchMode=yes") && ssh.contains("StrictHostKeyChecking=yes"), "{ssh}");
        let c = rsync_args(&prefs(), Path::new("/m/A/B"), "A/B", true);
        assert!(c.iter().any(|x| x == "--checksum") && c.iter().any(|x| x == "--dry-run"));
    }

    #[test]
    fn scan_needs_a_login() {
        assert!(start_scan(&prefs()).is_err());
        assert_eq!(url_encode("jim laptop/ü"), "jim%20laptop%2F%C3%BC");
    }
}
