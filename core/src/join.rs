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
/// The home server's usual address.
pub const DEFAULT_SERVER: &str = "192.168.1.10";

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
    Approved(Approval),
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
pub fn request(admin_url: &str, name: &str, role: Role, keys: WgKeys, ssh_key: Option<(&Path, &str)>) -> Result<Pending> {
    if !valid_name(name) {
        bail!("name: lowercase letters, digits and -, e.g. my-laptop");
    }
    let mut body = serde_json::json!({
        "name": name,
        "role": role.as_str(),
        "computer": computer_label(),
        "wireguard_key": keys.public,
    });
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
        Some("approved") => Ok(Status::Approved(serde_json::from_value(v).context("the approval")?)),
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
        assert!(valid_name("my-laptop") && valid_name("a") && !valid_name("-x") && !valid_name("Jim") && !valid_name(""));
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
            admin_url: admin_url(DEFAULT_SERVER),
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
}
