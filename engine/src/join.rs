//! Joining the LocalBeat server (docs/joining.md), for Studio's Settings page
//! and `localbeat join`: ask, wait for the admin, then set this computer up
//! (tunnel, music login and, for an admin, the key and settings that send
//! rips, with the server's host key pinned).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{self, Pending, Role, Status, WgKeys};

/// The writer key's setting (`~` works on every OS: `ServerPrefs::key_path`).
pub const WRITER_KEY: &str = "~/.ssh/localbeat-studio";

/// The SSH key that sends rips, made if missing: ed25519, no passphrase
/// (Studio pushes on its own). The server limits it to rsync inside the
/// music folder from the home network (`localbeat-writers`).
pub fn writer_key() -> Result<(PathBuf, String)> {
    let path = dirs::home_dir().context("no home folder")?.join(".ssh").join("localbeat-studio");
    if !path.is_file() {
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let out = localbeat_platform::command("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "localbeat-studio", "-f"])
            .arg(&path)
            .output()
            .context("running ssh-keygen (OpenSSH isn't installed?)")?;
        if !out.status.success() {
            bail!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let public = path.with_extension("pub");
    let text = std::fs::read_to_string(&public).with_context(|| format!("reading {}", public.display()))?;
    if !text.starts_with("ssh-ed25519 ") {
        bail!("{} isn't an ed25519 key: move it aside and try again", public.display());
    }
    Ok((path, text.trim().to_string()))
}

/// Sends the request and keeps it in `<config>/join.json` while it waits.
pub fn ask(s: &Settings, server: &str, name: &str, role: Role) -> Result<Pending> {
    let keys = WgKeys::generate()?;
    let ssh = if role == Role::Admin { Some(writer_key()?) } else { None };
    let p = join::request(&join::admin_url(server), name, role, keys, ssh.as_ref().map(|(p, k)| (p.as_path(), k.as_str())))?;
    p.save(&s.config_dir)?;
    Ok(p)
}

/// How the request stands; an approval is kept with the request, so a
/// failed setup can be retried without asking the admin again.
pub fn check(s: &Settings, p: &mut Pending) -> Result<Status> {
    if let Some(a) = &p.approval {
        return Ok(Status::Approved(Box::new(a.clone())));
    }
    let st = join::status(p)?;
    if let Status::Approved(a) = &st {
        p.approval = Some((**a).clone());
        p.save(&s.config_dir)?;
    }
    Ok(st)
}

/// Sets this computer up from the approval, saving `prefs`. Returns what
/// was done, for the log.
pub fn finish(s: &Settings, p: &Pending, prefs: &mut Prefs) -> Result<Vec<String>> {
    let a = p.approval.as_ref().context("not approved yet")?;
    let mut done = Vec::new();
    let config = p.wireguard_config().context("no tunnel in the approval")?;
    localbeat_platform::tunnel::install(&config).context("setting up the tunnel")?;
    done.push(format!("Tunnel to the server set up ({}, through {})", a.tunnel.address, a.tunnel.endpoint));
    prefs.server.url = a.music.server.clone();
    prefs.server.login = a.music.login.clone();
    prefs.server.secret = a.music.secret.clone();
    done.push(format!("Music server {} with the login {}", a.music.server, a.music.login));
    if let Some(w) = &a.writer {
        crate::push::save_host_key(s, &format!("{}\n", w.known_hosts.trim())).context("pinning the server's SSH key")?;
        prefs.server.host = w.host.clone();
        prefs.server.user = w.user.clone();
        prefs.server.key = WRITER_KEY.into();
        done.push(format!("Rips go to {}@{} (home network only), server key pinned", w.user, w.host));
    }
    prefs.role = Some(a.role);
    prefs.save(s)?;
    if let Err(e) = join::close(p) {
        done.push(format!("(Couldn't tell the server the setup is saved: {e:#}. It forgets it within the hour.)"));
    }
    Pending::remove(&s.config_dir);
    done.push(format!("Joined as {} ({})", a.name, a.role.as_str()));
    Ok(done)
}

/// Withdraws a request (or forgets a finished one).
pub fn cancel(s: &Settings, p: &Pending) {
    let _ = join::close(p);
    Pending::remove(&s.config_dir);
}
