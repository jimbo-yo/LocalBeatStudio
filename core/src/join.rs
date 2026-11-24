//! Joining the LocalBeat server from this computer (docs/joining.md): ask on
//! the home network, wait for the admin to approve, get the tunnel and login.
//!
//! The server's admin page (LocalBeatServer `host/localbeat-admin`, HTTPS on
//! port 8443, home network only) takes a request with this computer's public
//! keys and shows it with a short code; the admin approves it there. This
//! computer makes its own WireGuard key pair (and, for an admin, its SSH key
//! for sending rips, in the platform/engine layer): private keys never leave
//! it. No OS code here: installing the tunnel is `localbeat_platform::tunnel`.
//!
//! The page's certificate is self-made, so it isn't checked: the admin's code
//! comparison is what ties the request to this computer, and the page is on
//! the home network only.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use ureq::Agent;

/// The admin page's port on the server (`localbeat-admin`).
pub const ADMIN_PORT: u16 = 8443;
/// Default server address (empty: user enters their server's home IP/host).
pub const DEFAULT_SERVER: &str = "";

/// Invite URI prefix (`localbeat://join/<base64url-no-pad>`).
pub const INVITE_URI_PREFIX: &str = "localbeat://join/";
/// Current invite payload schema version.
pub const INVITE_SCHEMA_VERSION: u8 = 1;
/// Fixed server IP inside the WireGuard tunnel.
pub const SERVER_TUNNEL_IP: &str = "10.8.0.1";
/// Plain HTTP claim port inside the WireGuard tunnel (bootstrap peers only).
pub const CLAIM_PORT: u16 = 8444;
/// Plain HTTP admin invite API port inside the WireGuard tunnel (admin peers only).
pub const ADMIN_TUNNEL_PORT: u16 = 8445;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Plays the server's library (tunnel + music login).
    Listener,
    /// Also rips CDs and sends them to the server (plus a writer key).
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Listener => "listener",
            Role::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "listener" => Some(Role::Listener),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

/// A WireGuard key pair (base64, as in wg's configs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgKeys {
    pub private: String,
    pub public: String,
}

impl WgKeys {
    pub fn generate() -> Result<WgKeys> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness from the OS: {e}"))?;
        let secret = x25519_dalek::StaticSecret::from(bytes);
        let public = x25519_dalek::PublicKey::from(&secret);
        let b64 = base64::engine::general_purpose::STANDARD;
        Ok(WgKeys { private: b64.encode(secret.to_bytes()), public: b64.encode(public.as_bytes()) })
    }
}

/// The tunnel's details from the server (`localbeat-people --json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tunnel {
    pub address: String,
    pub server_key: String,
    pub preshared_key: String,
    pub endpoint: String,
    pub allowed_ips: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Music {
    /// `http://10.8.0.1:4533`: through the tunnel, at home too.
    pub server: String,
    pub login: String,
    pub secret: String,
}

/// For an admin: where rips go (rsync over SSH, home network only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Writer {
    pub host: String,
    pub user: String,
    /// The server's SSH host key as a known_hosts line, to pin.
    pub known_hosts: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub name: String,
    pub role: Role,
    pub tunnel: Tunnel,
    pub music: Music,
    #[serde(default)]
    pub writer: Option<Writer>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Waiting,
    Denied,
    Approved(Box<Approval>),
}

/// A request sent and not finished yet: kept in `<config>/join.json` (it
/// holds the WireGuard private key until the tunnel is installed), so
/// closing Studio while waiting doesn't lose it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    /// `https://192.168.1.10:8443`
    pub admin_url: String,
    pub name: String,
    pub role: Role,
    pub id: String,
    pub token: String,
    pub code: String,
    pub keys: WgKeys,
    /// For an admin: the SSH key that will send rips.
    #[serde(default)]
    pub ssh_key: Option<PathBuf>,
    /// Kept once approved, so a failed tunnel install can be retried
    /// without asking the admin again.
    #[serde(default)]
    pub approval: Option<Approval>,
}

impl Pending {
    pub fn file(config_dir: &Path) -> PathBuf {
        config_dir.join("join.json")
    }

    pub fn load(config_dir: &Path) -> Option<Pending> {
        serde_json::from_str(&std::fs::read_to_string(Self::file(config_dir)).ok()?).ok()
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(config_dir).with_context(|| format!("creating {}", config_dir.display()))?;
        let f = Self::file(config_dir);
        let tmp = f.with_extension("new");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::rename(&tmp, &f).with_context(|| format!("replacing {}", f.display()))
    }

    pub fn remove(config_dir: &Path) {
        let _ = std::fs::remove_file(Self::file(config_dir));
    }

    /// The WireGuard config for this computer (wg-quick format), once approved.
    pub fn wireguard_config(&self) -> Option<String> {
        let t = &self.approval.as_ref()?.tunnel;
        Some(format!(
            "[Interface]\nPrivateKey = {}\nAddress = {}\n\n[Peer]\nPublicKey = {}\nPresharedKey = {}\nEndpoint = {}\nAllowedIPs = {}\n",
            self.keys.private, t.address, t.server_key, t.preshared_key, t.endpoint, t.allowed_ips
        ))
    }
}

pub fn admin_url(server: &str) -> String {
    let host = server.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    if host.contains(':') { format!("https://{host}") } else { format!("https://{host}:{ADMIN_PORT}") }
}

/// The page's certificate is self-made: not checked (see the top).
fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .user_agent(crate::http::USER_AGENT)
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
            .build()
            .into()
    })
}

/// The server's `{"error": ...}`, or the status.
fn error_text(status: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("the server answered HTTP {status}"))
}

fn unreachable(admin_url: &str, e: ureq::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "can't reach the server's admin page at {admin_url} ({e}): joining works from the home network only"
    )
}

/// A human name for this computer, for the admin page.
pub fn computer_label() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let os = if cfg!(windows) { "Windows" } else if cfg!(target_os = "macos") { "macOS" } else { "Linux" };
    if host.is_empty() { os.to_string() } else { format!("{host} ({os})") }
}

/// A suggested name: the computer's, made fit for the server
/// (lowercase letters, digits and -).
pub fn suggested_name() -> String {
    let host = computer_label();
    let host = host.split(" (").next().unwrap_or("");
    let mut n: String = host
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while n.contains("--") {
        n = n.replace("--", "-");
    }
    let n = n.trim_matches('-');
    let n: String = n.chars().take(31).collect();
    if n.is_empty() { "my-computer".into() } else { n }
}

pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 31
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Sends the request: the admin page shows it with the returned code.
/// `user`: whose computer this is (their playlists are shared by all their
/// devices), as a suggestion the admin may change; empty for none.
pub fn request(admin_url: &str, name: &str, user: &str, role: Role, keys: WgKeys, ssh_key: Option<(&Path, &str)>) -> Result<Pending> {
    if !valid_name(name) {
        bail!("name: lowercase letters, digits and -, e.g. my-laptop");
    }
    if !user.is_empty() && !valid_name(user) {
        bail!("user: lowercase letters, digits and -, e.g. alex");
    }
    let mut body = serde_json::json!({
        "name": name,
        "role": role.as_str(),
        "computer": computer_label(),
        "wireguard_key": keys.public,
    });
    if !user.is_empty() {
        body["user"] = user.into();
    }
    if let Some((_, public)) = ssh_key {
        body["ssh_key"] = public.trim().into();
    }
    let mut resp = agent().post(format!("{admin_url}/join")).send_json(&body).map_err(|e| unreachable(admin_url, e))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if status != 201 {
        bail!("{}", error_text(status, &text));
    }
    let v: serde_json::Value = serde_json::from_str(&text).context("the server's answer")?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string).with_context(|| format!("no {k} in the answer"));
    Ok(Pending {
        admin_url: admin_url.to_string(),
        name: name.to_string(),
        role,
        id: s("id")?,
        token: s("token")?,
        code: s("code")?,
        keys,
        ssh_key: ssh_key.map(|(p, _)| p.to_path_buf()),
        approval: None,
    })
}

/// Asks how the request stands.
pub fn status(p: &Pending) -> Result<Status> {
    let mut resp = agent()
        .get(format!("{}/join/{}", p.admin_url, p.id))
        .header("Authorization", format!("Bearer {}", p.token))
        .call()
        .map_err(|e| unreachable(&p.admin_url, e))?;
    let code = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if code == 404 {
        bail!("the server no longer knows this request (requests wait an hour): ask again");
    }
    if code != 200 {
        bail!("{}", error_text(code, &text));
    }
    let v: serde_json::Value = serde_json::from_str(&text).context("the server's answer")?;
    match v.get("state").and_then(|s| s.as_str()) {
        Some("waiting") => Ok(Status::Waiting),
        Some("denied") => Ok(Status::Denied),
        Some("approved") => Ok(Status::Approved(Box::new(serde_json::from_value(v).context("the approval")?))),
        other => bail!("unexpected answer from the server ({other:?})"),
    }
}

/// Tells the server the answer is saved here (it then forgets it), or
/// withdraws a waiting request.
pub fn close(p: &Pending) -> Result<()> {
    let resp = agent()
        .delete(format!("{}/join/{}", p.admin_url, p.id))
        .header("Authorization", format!("Bearer {}", p.token))
        .call()
        .map_err(|e| unreachable(&p.admin_url, e))?;
    match resp.status().as_u16() {
        204 | 404 => Ok(()),
        c => bail!("the server answered HTTP {c}"),
    }
}

// ---------------------------------------------------------------------------
// Invite-based onboarding (docs/onboarding.md)
// ---------------------------------------------------------------------------

/// Saved embedded userspace WireGuard tunnel configuration.
///
/// Both `endpoint_public` and `endpoint_lan` are optional so LAN-only desktop
/// servers (without DDNS/port-forwarding) and remote-only servers both work,
/// as long as at least one endpoint is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedTunnel {
    pub private_key: String,
    pub address: String,
    pub server_key: String,
    pub preshared_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_public: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_lan: Option<String>,
    pub allowed_ips: String,
}

impl EmbeddedTunnel {
    pub fn has_endpoint(&self) -> bool {
        self.endpoint_public.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.endpoint_lan.as_deref().is_some_and(|s| !s.trim().is_empty())
    }
}

/// Decoded one-time invite from `localbeat://join/<base64url-no-pad>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    #[serde(rename = "v")]
    pub version: u8,
    #[serde(rename = "t")]
    pub token: String,
    #[serde(rename = "k")]
    pub bootstrap_private_key: String,
    #[serde(rename = "p")]
    pub bootstrap_preshared_key: String,
    #[serde(rename = "a")]
    pub bootstrap_address: String,
    #[serde(rename = "s")]
    pub server_key: String,
    #[serde(rename = "e", default, skip_serializing_if = "Option::is_none")]
    pub endpoint_public: Option<String>,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub endpoint_lan: Option<String>,
    #[serde(rename = "r")]
    pub role: Role,
}

/// Finds the first `localbeat://join/<base64url>` URI inside arbitrary pasted
/// text (e.g. a full SMS/chat message). Scheme/prefix matching is ASCII
/// case-insensitive; trailing punctuation and whitespace are ignored.
pub fn extract_invite_uri(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find(INVITE_URI_PREFIX)?;
    let payload_start = start + INVITE_URI_PREFIX.len();
    let payload_len = text[payload_start..]
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
        .count();
    if payload_len == 0 {
        return None;
    }
    Some(&text[start..payload_start + payload_len])
}

impl Invite {
    /// Extracts and decodes an invite from a URI or arbitrary pasted text.
    pub fn parse(input: &str) -> Result<Invite> {
        let uri = extract_invite_uri(input)
            .context("no LocalBeat invite found (expected a localbeat://join/... link)")?;
        let payload = &uri[INVITE_URI_PREFIX.len()..];
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .context("invalid invite link (corrupted encoding)")?;
        let mut invite: Invite = serde_json::from_slice(&bytes)
            .context("invalid invite link (corrupted payload)")?;
        if invite.version != INVITE_SCHEMA_VERSION {
            bail!(
                "unsupported invite version {} (update LocalBeat Studio and try again)",
                invite.version
            );
        }
        invite.endpoint_public = invite.endpoint_public.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        invite.endpoint_lan = invite.endpoint_lan.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        for (field, val) in [
            ("token", invite.token.as_str()),
            ("bootstrap key", invite.bootstrap_private_key.as_str()),
            ("bootstrap preshared key", invite.bootstrap_preshared_key.as_str()),
            ("bootstrap address", invite.bootstrap_address.as_str()),
            ("server key", invite.server_key.as_str()),
        ] {
            if val.trim().is_empty() {
                bail!("invalid invite link (missing {field})");
            }
        }
        if invite.endpoint_public.is_none() && invite.endpoint_lan.is_none() {
            bail!("invalid invite link (missing server endpoint)");
        }
        Ok(invite)
    }

    /// Encodes this invite as a `localbeat://join/<base64url-no-pad>` URI.
    pub fn to_uri(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        Ok(format!("{INVITE_URI_PREFIX}{encoded}"))
    }

    /// Builds the short-lived bootstrap WireGuard tunnel config used to dial
    /// `POST /claim` and `DELETE /claim`.
    pub fn bootstrap_tunnel(&self) -> EmbeddedTunnel {
        EmbeddedTunnel {
            private_key: self.bootstrap_private_key.clone(),
            address: self.bootstrap_address.clone(),
            server_key: self.server_key.clone(),
            preshared_key: self.bootstrap_preshared_key.clone(),
            endpoint_public: self.endpoint_public.clone(),
            endpoint_lan: self.endpoint_lan.clone(),
            allowed_ips: format!("{SERVER_TUNNEL_IP}/32"),
        }
    }
}

/// Permanent tunnel details returned by `POST /claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimTunnel {
    pub address: String,
    pub server_key: String,
    pub preshared_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_public: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_lan: Option<String>,
    pub allowed_ips: String,
}

/// Response from `POST /claim`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub name: String,
    #[serde(default)]
    pub user: String,
    pub role: Role,
    pub tunnel: ClaimTunnel,
    pub music: Music,
    #[serde(default)]
    pub writer: Option<Writer>,
}

impl ClaimResponse {
    pub fn into_embedded_tunnel(&self, private_key: String) -> EmbeddedTunnel {
        EmbeddedTunnel {
            private_key,
            address: self.tunnel.address.clone(),
            server_key: self.tunnel.server_key.clone(),
            preshared_key: self.tunnel.preshared_key.clone(),
            endpoint_public: self.tunnel.endpoint_public.clone(),
            endpoint_lan: self.tunnel.endpoint_lan.clone(),
            allowed_ips: self.tunnel.allowed_ips.clone(),
        }
    }
}

/// Plain HTTP client for in-tunnel / loopback onboarding and admin calls.
fn tunnel_http_agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .user_agent(crate::http::USER_AGENT)
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// Claims a one-time invite over the bootstrap WireGuard tunnel (or loopback
/// bridge). Idempotent for the same `wg_public_key` during the server's
/// 5-minute grace window.
pub fn claim_invite(
    base_url: &str,
    token: &str,
    wg_public_key: &str,
    ssh_public_key: Option<&str>,
) -> Result<ClaimResponse> {
    let base = base_url.trim_end_matches('/');
    let mut body = serde_json::json!({
        "token": token,
        "wireguard_key": wg_public_key,
        "device_kind": "studio",
        "device_label": computer_label(),
        "suggested_name": suggested_name(),
    });
    if let Some(ssh) = ssh_public_key {
        body["ssh_key"] = ssh.trim().into();
    }
    let mut resp = tunnel_http_agent()
        .post(format!("{base}/claim"))
        .send_json(&body)
        .map_err(|e| anyhow::anyhow!("can't reach the music server to claim this invite ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => serde_json::from_str(&text).context("reading the server's claim answer"),
        404 | 410 => bail!("this invite has expired or was already used: ask the admin for a new one"),
        _ => bail!("{}", error_text(status, &text)),
    }
}

/// Confirms to the server that the claimed config is safely saved on disk so
/// the server immediately burns the bootstrap peer and invite.
pub fn ack_claim(base_url: &str, token: &str) -> Result<()> {
    let base = base_url.trim_end_matches('/');
    let resp = tunnel_http_agent()
        .delete(format!("{base}/claim"))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("confirming invite claim ({e})"))?;
    match resp.status().as_u16() {
        204 | 404 => Ok(()),
        c => bail!("confirming invite claim: server answered HTTP {c}"),
    }
}

/// Short-lived in-memory admin session from `POST /admin/login`.
/// Never written to `localbeat.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSession {
    pub session_token: String,
    pub expires_at: String,
}

/// Active (unclaimed, unexpired) invite returned by the admin invite API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteSummary {
    pub id: String,
    pub user: String,
    pub role: Role,
    #[serde(default)]
    pub created_at: String,
    pub expires_at: String,
    pub uri: String,
}

/// Authenticates an admin Studio session using the admin password.
pub fn admin_login(base_url: &str, password: &str) -> Result<AdminSession> {
    let base = base_url.trim_end_matches('/');
    let mut resp = tunnel_http_agent()
        .post(format!("{base}/admin/login"))
        .send_json(&serde_json::json!({ "password": password }))
        .map_err(|e| anyhow::anyhow!("can't reach the server's admin API ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => serde_json::from_str(&text).context("reading admin login answer"),
        401 | 403 => bail!("wrong admin password"),
        _ => bail!("{}", error_text(status, &text)),
    }
}

/// Creates a new 7-day single-use invite for `user` with `role`.
pub fn create_invite(base_url: &str, session_token: &str, user: &str, role: Role) -> Result<InviteSummary> {
    let user = user.trim();
    if !user.is_empty() && !valid_name(user) {
        bail!("user: lowercase letters, digits and -, e.g. sarah");
    }
    let base = base_url.trim_end_matches('/');
    let mut resp = tunnel_http_agent()
        .post(format!("{base}/invites"))
        .header("Authorization", format!("Bearer {session_token}"))
        .send_json(&serde_json::json!({ "user": user, "role": role.as_str() }))
        .map_err(|e| anyhow::anyhow!("creating invite ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 | 201 => serde_json::from_str(&text).context("reading created invite"),
        401 => bail!("admin session expired: enter the admin password again"),
        _ => bail!("{}", error_text(status, &text)),
    }
}

/// Lists active unclaimed invites on the server.
pub fn list_invites(base_url: &str, session_token: &str) -> Result<Vec<InviteSummary>> {
    let base = base_url.trim_end_matches('/');
    let mut resp = tunnel_http_agent()
        .get(format!("{base}/invites"))
        .header("Authorization", format!("Bearer {session_token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("listing invites ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => serde_json::from_str(&text).context("reading invites list"),
        401 => bail!("admin session expired: enter the admin password again"),
        _ => bail!("{}", error_text(status, &text)),
    }
}

/// Revokes an unclaimed invite and removes its bootstrap WireGuard peer.
pub fn revoke_invite(base_url: &str, session_token: &str, id: &str) -> Result<()> {
    let base = base_url.trim_end_matches('/');
    let resp = tunnel_http_agent()
        .delete(format!("{base}/invites/{id}"))
        .header("Authorization", format!("Bearer {session_token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("revoking invite ({e})"))?;
    match resp.status().as_u16() {
        204 | 404 => Ok(()),
        401 => bail!("admin session expired: enter the admin password again"),
        c => bail!("revoking invite: server answered HTTP {c}"),
    }
}

/// Lists existing user names on the server (for Studio's invite dropdown).
pub fn list_users(base_url: &str, session_token: &str) -> Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    let mut resp = tunnel_http_agent()
        .get(format!("{base}/users"))
        .header("Authorization", format!("Bearer {session_token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("listing users ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => serde_json::from_str(&text).context("reading users list"),
        401 => bail!("admin session expired: enter the admin password again"),
        _ => bail!("{}", error_text(status, &text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_a_wireguard_pair() {
        let k = WgKeys::generate().unwrap();
        assert_eq!(k.private.len(), 44);
        assert_eq!(k.public.len(), 44);
        // RFC 7748 section 6.1 (Alice): private -> public.
        let hex = |s: &str| -> [u8; 32] {
            (0..32).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect::<Vec<u8>>().try_into().unwrap()
        };
        let private = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(private));
        assert_eq!(public.as_bytes(), &hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
        // Different every time.
        assert_ne!(WgKeys::generate().unwrap().private, k.private);
    }

    #[test]
    fn names() {
        assert!(valid_name("my-laptop") && valid_name("a") && !valid_name("-x") && !valid_name("My") && !valid_name(""));
        assert!(valid_name(&suggested_name()));
        assert_eq!(admin_url("192.168.1.10"), "https://192.168.1.10:8443");
        assert_eq!(admin_url("https://h:9000/"), "https://h:9000");
    }

    #[test]
    fn wireguard_config_and_answer() {
        let answer = r#"{"state": "approved", "name": "pc", "role": "admin",
            "tunnel": {"address": "10.8.0.4/32", "server_key": "S", "preshared_key": "P",
                       "endpoint": "vpn.example.com:51820", "allowed_ips": "10.8.0.1/32"},
            "music": {"server": "http://10.8.0.1:4533", "login": "pc", "secret": "x"},
            "writer": {"host": "192.168.1.10", "user": "musicuser", "known_hosts": "192.168.1.10 ssh-ed25519 AAAA"}}"#;
        let a: Approval = serde_json::from_str(answer).unwrap();
        assert_eq!(a.role, Role::Admin);
        assert_eq!(a.writer.as_ref().unwrap().user, "musicuser");
        let p = Pending {
            admin_url: admin_url("192.168.1.10"),
            name: "pc".into(),
            role: Role::Admin,
            id: "0123456789abcdef".into(),
            token: "t".into(),
            code: "ABC-DEF".into(),
            keys: WgKeys { private: "PRIV".into(), public: "PUB".into() },
            ssh_key: None,
            approval: Some(a),
        };
        let c = p.wireguard_config().unwrap();
        assert!(c.contains("PrivateKey = PRIV\nAddress = 10.8.0.4/32\n"));
        assert!(c.contains("Endpoint = vpn.example.com:51820\nAllowedIPs = 10.8.0.1/32\n"));
        let mut v: serde_json::Value = serde_json::from_str(answer).unwrap();
        v["role"] = "listener".into();
        v.as_object_mut().unwrap().remove("writer");
        let listener: Approval = serde_json::from_value(v).unwrap();
        assert!(listener.writer.is_none() && listener.role == Role::Listener);
    }

    fn sample_invite() -> Invite {
        Invite {
            version: INVITE_SCHEMA_VERSION,
            token: "tok1234567890abcdef".into(),
            bootstrap_private_key: "BPRIV==".into(),
            bootstrap_preshared_key: "BPSK==".into(),
            bootstrap_address: "10.8.250.2/32".into(),
            server_key: "SPUB==".into(),
            endpoint_public: Some("vpn.example.com:51820".into()),
            endpoint_lan: Some("192.168.1.10:51820".into()),
            role: Role::Listener,
        }
    }

    #[test]
    fn invite_uri_round_trip_and_smart_extraction() {
        let inv = sample_invite();
        let uri = inv.to_uri().unwrap();
        assert!(uri.starts_with(INVITE_URI_PREFIX));
        assert_eq!(Invite::parse(&uri).unwrap(), inv);

        // Embedded inside a messy SMS message with punctuation and mixed case scheme.
        let sms = format!("Hey Mom! Tap or paste this: {}! Love you.", uri.replacen("localbeat://join/", "LocalBeat://JOIN/", 1));
        assert!(extract_invite_uri(&sms).is_some());
        assert_eq!(Invite::parse(&sms).unwrap(), inv);

        // LAN-only invite (no DDNS / port-forwarding on a desktop server).
        let mut lan_only = sample_invite();
        lan_only.endpoint_public = None;
        let parsed = Invite::parse(&lan_only.to_uri().unwrap()).unwrap();
        assert_eq!(parsed.endpoint_public, None);
        assert_eq!(parsed.endpoint_lan.as_deref(), Some("192.168.1.10:51820"));
        assert!(parsed.bootstrap_tunnel().has_endpoint());

        // Public-only invite.
        let mut pub_only = sample_invite();
        pub_only.endpoint_lan = None;
        let parsed = Invite::parse(&pub_only.to_uri().unwrap()).unwrap();
        assert_eq!(parsed.endpoint_lan, None);
        assert_eq!(parsed.endpoint_public.as_deref(), Some("vpn.example.com:51820"));
    }

    #[test]
    fn invite_validation_rejects_bad_inputs() {
        assert!(Invite::parse("nothing here").is_err());
        assert!(Invite::parse("localbeat://join/").is_err());
        assert!(Invite::parse("localbeat://join/not_valid_json").is_err());

        // Unsupported version.
        let mut bad_ver = sample_invite();
        bad_ver.version = 99;
        assert!(Invite::parse(&bad_ver.to_uri().unwrap()).unwrap_err().to_string().contains("version 99"));

        // Missing both endpoints.
        let mut no_ep = sample_invite();
        no_ep.endpoint_public = None;
        no_ep.endpoint_lan = None;
        assert!(Invite::parse(&no_ep.to_uri().unwrap()).unwrap_err().to_string().contains("endpoint"));

        // Missing token.
        let mut no_tok = sample_invite();
        no_tok.token.clear();
        assert!(Invite::parse(&no_tok.to_uri().unwrap()).unwrap_err().to_string().contains("token"));
    }

    #[test]
    fn claim_and_admin_http_clients_against_mock_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let server = std::thread::spawn(move || {
            for _ in 0..7 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let (status, body) = if first_line.starts_with("POST /claim ") {
                    (
                        "200 OK",
                        r#"{"name":"sarah-laptop","user":"sarah","role":"listener","tunnel":{"address":"10.8.0.12/32","server_key":"SRV","preshared_key":"PSK","endpoint_public":"vpn.example.com:51820","endpoint_lan":"192.168.1.10:51820","allowed_ips":"10.8.0.1/32"},"music":{"server":"http://10.8.0.1:4533","login":"sarah-laptop","secret":"sec"}}"#,
                    )
                } else if first_line.starts_with("DELETE /claim ") {
                    ("204 No Content", "")
                } else if first_line.starts_with("POST /admin/login ") {
                    ("200 OK", r#"{"session_token":"sess123","expires_at":"2026-11-24T12:00:00Z"}"#)
                } else if first_line.starts_with("POST /invites ") {
                    ("201 Created", r#"{"id":"inv1","user":"sarah","role":"listener","created_at":"2026-11-24T11:00:00Z","expires_at":"2026-12-01T11:00:00Z","uri":"localbeat://join/abc"}"#)
                } else if first_line.starts_with("GET /invites ") {
                    ("200 OK", r#"[{"id":"inv1","user":"sarah","role":"listener","created_at":"2026-11-24T11:00:00Z","expires_at":"2026-12-01T11:00:00Z","uri":"localbeat://join/abc"}]"#)
                } else if first_line.starts_with("DELETE /invites/inv1 ") {
                    ("204 No Content", "")
                } else if first_line.starts_with("GET /users ") {
                    ("200 OK", r#"["jim","sarah"]"#)
                } else {
                    ("404 Not Found", r#"{"error":"nope"}"#)
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        let claimed = claim_invite(&base, "tok1", "PUB1", None).unwrap();
        assert_eq!(claimed.name, "sarah-laptop");
        assert_eq!(claimed.user, "sarah");
        let tun = claimed.into_embedded_tunnel("PRIV1".into());
        assert_eq!(tun.private_key, "PRIV1");
        assert_eq!(tun.endpoint_lan.as_deref(), Some("192.168.1.10:51820"));

        ack_claim(&base, "tok1").unwrap();

        let sess = admin_login(&base, "pw").unwrap();
        assert_eq!(sess.session_token, "sess123");

        let created = create_invite(&base, &sess.session_token, "sarah", Role::Listener).unwrap();
        assert_eq!(created.id, "inv1");

        let listed = list_invites(&base, &sess.session_token).unwrap();
        assert_eq!(listed.len(), 1);

        revoke_invite(&base, &sess.session_token, "inv1").unwrap();

        let users = list_users(&base, &sess.session_token).unwrap();
        assert_eq!(users, vec!["jim", "sarah"]);

        server.join().unwrap();
    }
}
