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
use localbeat_core::subsonic::Subsonic;

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
    /// After the pushes: the server scanned (or couldn't be asked to).
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

/// The host and SSH port: `host` may say `host:port` (22 if not).
pub fn host_port(sp: &ServerPrefs) -> (&str, u16) {
    match sp.host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse().unwrap_or(22)),
        _ => (sp.host.as_str(), 22),
    }
}

/// Where `localbeat server trust` (or the Server page) pins the server's
/// host key; read as well as `~/.ssh/known_hosts`, where a key accepted by
/// sshing in by hand once is.
pub fn known_hosts(s: &Settings) -> std::path::PathBuf {
    s.config_dir.join("server_known_hosts")
}

/// Can we reach the server's SSH right now? A quick TCP connect, so being away
/// from home costs a few seconds, not an SSH timeout per album.
pub fn reachable(sp: &ServerPrefs) -> Result<(), String> {
    let addrs = host_port(sp).to_socket_addrs().map_err(|e| format!("{}: {e}", sp.host))?;
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
/// password, no unknown host key: the server's key must already be known,
/// in `~/.ssh/known_hosts` or pinned in [`known_hosts`]).
pub fn ssh_command(s: &Settings, sp: &ServerPrefs) -> String {
    let key = sp.key_path().to_string_lossy().replace('\'', r"'\''");
    let home = dirs::home_dir().unwrap_or_default().join(".ssh").join("known_hosts");
    // One -o with both files, each in double quotes for ssh (rsync keeps
    // double quotes inside single quotes as they are).
    let files = format!("UserKnownHostsFile=\"{}\" \"{}\"", home.display(), known_hosts(s).display());
    format!(
        "ssh -p {} -i '{key}' -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o '{files}' -o ConnectTimeout=10",
        host_port(sp).1
    )
}

/// Reads the server's host key over the network, for pinning: the
/// known_hosts lines and their fingerprint, to compare with the Mac's own
/// (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` there).
pub fn scan_host_key(s: &Settings, sp: &ServerPrefs) -> Result<(String, String)> {
    let (host, port) = host_port(sp);
    let out = Command::new("ssh-keyscan")
        .args(["-T", "5", "-t", "ed25519", "-p", &port.to_string(), host])
        .output()
        .context("running ssh-keyscan (is openssh-clients installed?)")?;
    let lines: String = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    if lines.is_empty() {
        bail!("no SSH answer from {} (on the home network?)", sp.host);
    }
    let tmp = known_hosts(s).with_extension("scan");
    std::fs::create_dir_all(&s.config_dir).with_context(|| format!("creating {}", s.config_dir.display()))?;
    std::fs::write(&tmp, &lines).with_context(|| format!("writing {}", tmp.display()))?;
    let fp = Command::new("ssh-keygen").arg("-lf").arg(&tmp).output();
    std::fs::remove_file(&tmp).ok();
    let fp = fp.context("running ssh-keygen")?;
    Ok((lines, String::from_utf8_lossy(&fp.stdout).trim().to_string()))
}

/// Pins the lines from [`scan_host_key`].
pub fn save_host_key(s: &Settings, lines: &str) -> Result<()> {
    let f = known_hosts(s);
    std::fs::write(&f, lines).with_context(|| format!("writing {}", f.display()))
}

fn rsync_args(s: &Settings, sp: &ServerPrefs, local: &Path, album: &str, check: bool) -> Vec<OsString> {
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
    a.push(ssh_command(s, sp).into());
    a.push(src);
    a.push(format!("{}@{}:{REMOTE_FLAC}/{album}/", sp.user, host_port(sp).0).into());
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
             Check before trusting it (localbeat server trust shows its fingerprint to compare with the Mac's)"
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
    let out = match rsync(&rsync_args(s, sp, &local, album, false)) {
        Ok(o) => o,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    if !out.status.success() {
        return classify(&out);
    }
    // Check: comparing checksums, rsync must find nothing left to send.
    let out = match rsync(&rsync_args(s, sp, &local, album, true)) {
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

/// The album's name and artist as the server's catalogue shows them (from
/// its first FLAC).
fn album_tags(dir: &Path) -> Option<(String, String)> {
    let mut flacs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("flac")))
        .collect();
    flacs.sort();
    let f = localbeat_core::flacfile::read_tags(flacs.first()?).ok()?;
    let album = f.tag("ALBUM")?.to_string();
    let artist = f.tag("ALBUMARTIST").or_else(|| f.tag("ARTIST")).unwrap_or("").to_string();
    Some((artist, album))
}

/// Asks the music server to scan now (else it finds new albums within its
/// 10-minute rescan) and waits for the scan to finish (up to 10 minutes).
pub fn scan(sub: &Subsonic) -> Result<()> {
    sub.start_scan()?;
    let start = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    while sub.scanning()? {
        if start.elapsed() > Duration::from_secs(600) {
            bail!("the server's scan didn't finish within 10 minutes");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// After a scan: is `album` in the server's catalogue? `Ok(None)` if this
/// album's tags can't tell (then the checksum comparison has to do).
fn in_catalogue(s: &Settings, sub: &Subsonic, album: &str) -> Result<Option<bool>> {
    let Some((artist, name)) = album_tags(&outbox::album_dir(s, album)) else { return Ok(None) };
    // The catalogue can lag the scan's end by a moment.
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        if sub.has_album(&artist, &name)? {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
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
    let mut sent = Vec::new();
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
                ev(PushEvent::Sent(album.clone()));
                sent.push(album);
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
    if !sent.is_empty() {
        scan_and_check(s, sp, &sent, &mut sum, ev);
    }
    Ok(sum)
}

/// Scans, then checks each sent album is in the catalogue: one that isn't is
/// marked failed and sent (a no-op) and checked again next time.
fn scan_and_check(s: &Settings, sp: &ServerPrefs, sent: &[String], sum: &mut Summary, ev: &mut dyn FnMut(PushEvent)) {
    let Some(sub) = Subsonic::new(sp) else {
        ev(PushEvent::Scan(Err("no music server login set: it finds the new albums within 10 minutes".into())));
        return;
    };
    if let Err(e) = scan(&sub) {
        ev(PushEvent::Scan(Err(format!("{e:#}"))));
        return;
    }
    ev(PushEvent::Scan(Ok(())));
    for album in sent {
        let error = match in_catalogue(s, &sub, album) {
            Ok(Some(true)) | Ok(None) => continue,
            Ok(Some(false)) => "on the server, but not in its catalogue after the scan: checked again next time".to_string(),
            Err(e) => format!("on the server, but checking its catalogue failed: {e:#}"),
        };
        if Outbox::update(s, |o| o.failed(album, &error)).is_ok() {
            sum.sent -= 1;
            sum.failed += 1;
            sum.waiting += 1;
        }
        ev(PushEvent::Failed { album: album.clone(), error });
    }
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
        let s = Settings::load();
        let a = rsync_args(&s, &prefs(), Path::new("/m/FLAC/Bob Dylan/Blood on the Tracks"), "Bob Dylan/Blood on the Tracks", false);
        let a: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(a.contains(&"--mkpath".to_string()) && a.contains(&"--exclude=*.part".to_string()));
        assert!(!a.iter().any(|x| x.contains("--delete")), "never deletes (the key couldn't anyway)");
        assert_eq!(a[a.len() - 2], "/m/FLAC/Bob Dylan/Blood on the Tracks/");
        assert_eq!(a[a.len() - 1], "musicuser@192.168.1.10:FLAC/Bob Dylan/Blood on the Tracks/");
        let ssh = &a[a.len() - 3];
        assert!(ssh.contains(r"-i '/home/me/.ssh/it'\''s'") && ssh.contains("BatchMode=yes") && ssh.contains("StrictHostKeyChecking=yes"), "{ssh}");
        let c = rsync_args(&s, &prefs(), Path::new("/m/A/B"), "A/B", true);
        assert!(ssh.contains("-p 22 ") && ssh.contains("server_known_hosts"), "{ssh}");
        let other = ServerPrefs { host: "127.0.0.1:2222".into(), ..prefs() };
        assert_eq!(host_port(&other), ("127.0.0.1", 2222));
        let o = rsync_args(&s, &other, Path::new("/m/A/B"), "A/B", false);
        assert_eq!(o[o.len() - 1].to_string_lossy(), "musicuser@127.0.0.1:FLAC/A/B/");
        assert!(c.iter().any(|x| x == "--checksum") && c.iter().any(|x| x == "--dry-run"));
    }

    #[test]
    fn scan_needs_a_login() {
        assert!(Subsonic::new(&prefs()).is_none());
        let with = ServerPrefs { url: "http://h:4533".into(), login: "my-laptop".into(), secret: "x".into(), ..prefs() };
        assert!(Subsonic::new(&with).is_some());
    }
}
