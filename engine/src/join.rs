//! Joining the LocalBeat server via one-time invite links (docs/onboarding.md),
//! plus admin invite management over the embedded WireGuard tunnel (or direct
//! loopback for a co-located server).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{
    self, ADMIN_TUNNEL_PORT, AdminSession, CLAIM_PORT, Invite, InviteSummary, Role, WgKeys,
};
use localbeat_core::tunnel::{self, OwnedBridge};

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

// ---------------------------------------------------------------------------
// Invite-based onboarding & admin invite helpers (docs/onboarding.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClaimOutcome {
    pub name: String,
    pub user: String,
    pub role: Role,
    pub log: Vec<String>,
}

/// Claims a `localbeat://join/...` invite (accepts raw URI or pasted SMS/chat
/// text): generates permanent local keys, dials the server over an ephemeral
/// bootstrap WireGuard tunnel, claims idempotently, saves `prefs` to disk, and
/// sends `DELETE /claim` so the server immediately burns the bootstrap peer.
pub fn claim(s: &Settings, invite_text: &str, prefs: &mut Prefs) -> Result<ClaimOutcome> {
    let invite = Invite::parse(invite_text)?;
    let keys = WgKeys::generate()?;
    let ssh = if invite.role == Role::Admin { Some(writer_key()?) } else { None };
    let ssh_pub = ssh.as_ref().map(|(_, pub_key)| pub_key.as_str());

    let bridge = OwnedBridge::start_ephemeral(&invite.bootstrap_tunnel())
        .context("starting temporary WireGuard tunnel for invite claim")?;
    let claim_base = bridge.base_url_for(CLAIM_PORT);

    // Retry once on transient connection error in case the initial WAN WireGuard
    // handshake takes a moment to complete.
    let claimed = match join::claim_invite(&claim_base, &invite.token, &keys.public, ssh_pub) {
        Ok(c) => c,
        Err(first_err) => {
            let msg = format!("{first_err:#}");
            if msg.contains("expired") || msg.contains("already used") || msg.contains("admin computer") {
                return Err(first_err);
            }
            std::thread::sleep(std::time::Duration::from_millis(800));
            join::claim_invite(&claim_base, &invite.token, &keys.public, ssh_pub)?
        }
    };

    let embedded = claimed.into_embedded_tunnel(keys.private);
    let mut log = Vec::new();
    let endpoints: Vec<&str> = [embedded.endpoint_lan.as_deref(), embedded.endpoint_public.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    log.push(format!(
        "Built-in WireGuard tunnel saved ({}, via {})",
        embedded.address,
        endpoints.join(" / ")
    ));

    prefs.server.url = claimed.music.server.clone();
    prefs.server.login = claimed.music.login.clone();
    prefs.server.secret = claimed.music.secret.clone();
    prefs.server.tunnel = Some(embedded);
    log.push(format!("Music server {} with login {}", claimed.music.server, claimed.music.login));

    if let Some(w) = &claimed.writer {
        crate::push::save_host_key(s, &format!("{}\n", w.known_hosts.trim()))
            .context("pinning the server's SSH key")?;
        prefs.server.host = w.host.clone();
        prefs.server.user = w.user.clone();
        prefs.server.key = WRITER_KEY.into();
        log.push(format!("Rips go to {}@{} (home network only), server key pinned", w.user, w.host));
    } else {
        prefs.server.host.clear();
        prefs.server.user.clear();
        prefs.server.key.clear();
    }

    prefs.role = Some(claimed.role);
    prefs.save(s)?;
    tunnel::reset_active_tunnel();

    if let Err(e) = join::ack_claim(&claim_base, &invite.token) {
        log.push(format!("(Couldn't confirm invite burn: {e:#}. The server cleans it up within 5 minutes.)"));
    }
    drop(bridge);

    let who = if claimed.user.is_empty() {
        claimed.name.clone()
    } else {
        format!("{} (user {})", claimed.name, claimed.user)
    };
    log.push(format!("Joined as {who} ({})", claimed.role.as_str()));

    Ok(ClaimOutcome {
        name: claimed.name,
        user: claimed.user,
        role: claimed.role,
        log,
    })
}

/// Resolves the admin invite API base URL (via embedded WireGuard bridge or
/// direct `127.0.0.1` for a co-located local server).
pub fn admin_base_url(s: &Settings, prefs: &Prefs) -> Result<String> {
    if !prefs.is_admin() {
        bail!("only an admin computer can manage invites");
    }
    if prefs.server.is_local_loopback() {
        return Ok(format!("http://127.0.0.1:{ADMIN_TUNNEL_PORT}"));
    }
    if prefs.server.tunnel.is_none() {
        bail!("this computer doesn't have an embedded WireGuard tunnel yet (re-join with an invite first)");
    }
    tunnel::resolve_base_url(&prefs.server, Some(ADMIN_TUNNEL_PORT), Some(&s.cache_dir))
}

pub fn admin_login(s: &Settings, prefs: &Prefs, password: &str) -> Result<AdminSession> {
    let base = admin_base_url(s, prefs)?;
    join::admin_login(&base, password)
}

pub fn create_invite(s: &Settings, prefs: &Prefs, session_token: &str, user: &str, role: Role) -> Result<InviteSummary> {
    let base = admin_base_url(s, prefs)?;
    join::create_invite(&base, session_token, user, role)
}

pub fn list_invites(s: &Settings, prefs: &Prefs, session_token: &str) -> Result<Vec<InviteSummary>> {
    let base = admin_base_url(s, prefs)?;
    join::list_invites(&base, session_token)
}

pub fn revoke_invite(s: &Settings, prefs: &Prefs, session_token: &str, id: &str) -> Result<()> {
    let base = admin_base_url(s, prefs)?;
    join::revoke_invite(&base, session_token, id)
}

pub fn list_users(s: &Settings, prefs: &Prefs, session_token: &str) -> Result<Vec<String>> {
    let base = admin_base_url(s, prefs)?;
    join::list_users(&base, session_token)
}
