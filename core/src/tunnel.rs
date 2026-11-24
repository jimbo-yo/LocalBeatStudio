//! Embedded userspace WireGuard tunnel + local loopback HTTP bridge
//! (docs/onboarding.md).
//!
//! Runs a pure-Rust WireGuard (`boringtun`) + IPv4/TCP (`smoltcp`) stack
//! inside the process and exposes a local HTTP bridge on `127.0.0.1:<port>`
//! authenticated by a per-run bearer token in the path:
//!
//!   `http://127.0.0.1:<port>/p/<token>/<target_port>/<path...>`
//!
//! Concurrent local processes (`localbeat-studio`, `localbeat` CLI,
//! `localbeat mcp`) coordinate via `<cache>/lock-tunnel` and
//! `<cache>/tunnel-proxy.json` so only one process owns the WireGuard UDP
//! socket at a time — avoiding peer-roaming conflicts on the server.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey as WgPublicKey, StaticSecret as WgStaticSecret};
use serde::{Deserialize, Serialize};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{self, Socket as TcpSocket, SocketBuffer};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use crate::config::{ServerPrefs, Settings};
use crate::join::{EmbeddedTunnel, SERVER_TUNNEL_IP};
use crate::lock::{self, Lock};

/// WireGuard MTU (safe for IPv4/IPv6 over UDP without fragmentation).
const WG_MTU: usize = 1280;
/// Happy-Eyeballs head start for `endpoint_lan` before also probing `endpoint_public`.
const LAN_HEAD_START: Duration = Duration::from_millis(150);
/// If no valid WireGuard packet arrives for this long while active, re-probe both endpoints.
const STALL_REPROBE_AFTER: Duration = Duration::from_secs(3);
/// If no handshake succeeds after this long, report `LinkState::Stalled`.
const STALL_REPORT_AFTER: Duration = Duration::from_secs(20);
/// TCP receive/send ring buffer size per proxied stream (64 KiB for smooth FLAC downloads).
const TCP_BUF_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Lan,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Connected(EndpointKind),
    /// No handshake completed after >20 s of trying (offline or device revoked).
    Stalled,
}

impl LinkState {
    fn encode(self) -> u8 {
        match self {
            LinkState::Connecting => 0,
            LinkState::Connected(EndpointKind::Lan) => 1,
            LinkState::Connected(EndpointKind::Public) => 2,
            LinkState::Stalled => 3,
        }
    }

    fn decode(v: u8) -> Self {
        match v {
            1 => LinkState::Connected(EndpointKind::Lan),
            2 => LinkState::Connected(EndpointKind::Public),
            3 => LinkState::Stalled,
            _ => LinkState::Connecting,
        }
    }
}

/// Runtime metadata for the shared local loopback bridge (`<cache>/tunnel-proxy.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub port: u16,
    pub pid: u32,
    pub token: String,
    pub fingerprint: String,
}

impl ProxyRecord {
    fn file(cache_dir: &Path) -> PathBuf {
        cache_dir.join("tunnel-proxy.json")
    }

    pub fn base_url_for(&self, target_port: u16) -> String {
        format!("http://127.0.0.1:{}/p/{}/{target_port}", self.port, self.token)
    }

    fn load(cache_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::file(cache_dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self, cache_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
        let f = Self::file(cache_dir);
        let tmp = f.with_extension("new");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::rename(&tmp, &f).with_context(|| format!("replacing {}", f.display()))
    }
}

/// Stable fingerprint of a tunnel config so a proxy for an old invite/device
/// isn't reused after re-joining.
pub fn config_fingerprint(cfg: &EmbeddedTunnel) -> String {
    let raw = format!(
        "{}|{}|{}|{}|{}",
        cfg.address,
        cfg.server_key,
        cfg.endpoint_public.as_deref().unwrap_or(""),
        cfg.endpoint_lan.as_deref().unwrap_or(""),
        cfg.allowed_ips
    );
    format!("{:x}", md5::compute(raw.as_bytes()))
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 18];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no randomness from the OS: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_key_32(b64: &str, label: &str) -> Result<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .with_context(|| format!("invalid base64 for {label}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("{label} must be 32 bytes"))
}

fn parse_ipv4_cidr(s: &str) -> Result<(Ipv4Addr, u8)> {
    let s = s.trim();
    let (ip_str, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, p.parse::<u8>().context("invalid CIDR prefix")?),
        None => (s, 32),
    };
    let ip: Ipv4Addr = ip_str.parse().with_context(|| format!("invalid tunnel IPv4 {ip_str}"))?;
    Ok((ip, prefix))
}

/// Checks if a local loopback bridge is alive and returns its `LinkState`.
fn probe_local_proxy(rec: &ProxyRecord) -> Option<LinkState> {
    let addr = SocketAddr::from(([127, 0, 0, 1], rec.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(250))).ok();
    stream.set_write_timeout(Some(Duration::from_millis(250))).ok();
    let req = format!(
        "GET /health/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        rec.token
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).ok()?;
    let text = std::str::from_utf8(&buf[..n]).ok()?;
    if !text.starts_with("HTTP/1.1 200") {
        return None;
    }
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
    Some(match body {
        "lan" => LinkState::Connected(EndpointKind::Lan),
        "public" => LinkState::Connected(EndpointKind::Public),
        "stalled" => LinkState::Stalled,
        _ => LinkState::Connecting,
    })
}

/// An in-process WireGuard + loopback HTTP bridge owned by this process.
pub struct OwnedBridge {
    record: ProxyRecord,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    reprobe_flag: Arc<AtomicBool>,
    _lock: Option<Lock>,
    proxy_file: Option<PathBuf>,
    listener_thread: Option<JoinHandle<()>>,
    engine_thread: Option<JoinHandle<()>>,
}

impl OwnedBridge {
    /// Starts an ephemeral tunnel (for `POST /claim` / `DELETE /claim`).
    /// Does not touch `<cache>/lock-tunnel` or `tunnel-proxy.json`.
    pub fn start_ephemeral(cfg: &EmbeddedTunnel) -> Result<Self> {
        Self::start_inner(cfg, None, None)
    }

    fn start_shared(cache_dir: &Path, lock: Lock, cfg: &EmbeddedTunnel) -> Result<Self> {
        let proxy_file = ProxyRecord::file(cache_dir);
        let bridge = Self::start_inner(cfg, Some(lock), Some(proxy_file))?;
        bridge.record.save(cache_dir)?;
        Ok(bridge)
    }

    fn start_inner(
        cfg: &EmbeddedTunnel,
        lock: Option<Lock>,
        proxy_file: Option<PathBuf>,
    ) -> Result<Self> {
        if !cfg.has_endpoint() {
            bail!("tunnel config has neither a LAN nor a public server endpoint");
        }
        let priv_bytes = decode_key_32(&cfg.private_key, "tunnel private_key")?;
        let srv_bytes = decode_key_32(&cfg.server_key, "tunnel server_key")?;
        let psk_bytes = if cfg.preshared_key.trim().is_empty() {
            None
        } else {
            Some(decode_key_32(&cfg.preshared_key, "tunnel preshared_key")?)
        };
        let (local_ip, prefix) = parse_ipv4_cidr(&cfg.address)?;
        let server_ip: Ipv4Addr = SERVER_TUNNEL_IP.parse().expect("valid SERVER_TUNNEL_IP");

        let tunn = Tunn::new(
            WgStaticSecret::from(priv_bytes),
            WgPublicKey::from(srv_bytes),
            psk_bytes,
            Some(25),
            0,
            None,
        );

        let udp = UdpSocket::bind("0.0.0.0:0").context("binding WireGuard UDP socket")?;
        udp.set_nonblocking(true).context("setting UDP non-blocking")?;

        let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback proxy")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true).context("setting listener non-blocking")?;

        let token = random_token()?;
        let record = ProxyRecord {
            port,
            pid: std::process::id(),
            token: token.clone(),
            fingerprint: config_fingerprint(cfg),
        };

        let state = Arc::new(AtomicU8::new(LinkState::Connecting.encode()));
        let stop = Arc::new(AtomicBool::new(false));
        let reprobe_flag = Arc::new(AtomicBool::new(false));
        let (conn_tx, conn_rx) = mpsc::channel::<NewConn>();

        let engine_state = Arc::clone(&state);
        let engine_stop = Arc::clone(&stop);
        let engine_reprobe = Arc::clone(&reprobe_flag);
        let endpoint_lan = cfg.endpoint_lan.clone();
        let endpoint_public = cfg.endpoint_public.clone();

        let engine_thread = thread::Builder::new()
            .name("localbeat-wg".into())
            .spawn(move || {
                run_engine(
                    tunn,
                    udp,
                    local_ip,
                    prefix,
                    server_ip,
                    endpoint_lan,
                    endpoint_public,
                    conn_rx,
                    engine_state,
                    engine_stop,
                    engine_reprobe,
                );
            })
            .context("spawning WireGuard engine thread")?;

        let listener_state = Arc::clone(&state);
        let listener_stop = Arc::clone(&stop);
        let listener_thread = thread::Builder::new()
            .name("localbeat-proxy".into())
            .spawn(move || {
                run_listener(listener, token, conn_tx, listener_state, listener_stop);
            })
            .context("spawning loopback proxy thread")?;

        Ok(OwnedBridge {
            record,
            state,
            stop,
            reprobe_flag,
            _lock: lock,
            proxy_file,
            listener_thread: Some(listener_thread),
            engine_thread: Some(engine_thread),
        })
    }

    pub fn base_url_for(&self, target_port: u16) -> String {
        self.record.base_url_for(target_port)
    }

    pub fn state(&self) -> LinkState {
        LinkState::decode(self.state.load(Ordering::Relaxed))
    }

    pub fn reprobe(&self) {
        self.reprobe_flag.store(true, Ordering::Relaxed);
    }

    pub fn record(&self) -> &ProxyRecord {
        &self.record
    }
}

impl Drop for OwnedBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the listener if needed (it polls every 50 ms on non-blocking accept).
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.record.port)),
            Duration::from_millis(50),
        );
        if let Some(h) = self.listener_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.engine_thread.take() {
            let _ = h.join();
        }
        if let Some(f) = &self.proxy_file {
            // Only remove if it still points to our port/pid.
            if let Ok(text) = std::fs::read_to_string(f)
                && let Ok(rec) = serde_json::from_str::<ProxyRecord>(&text)
                && rec.pid == self.record.pid
                && rec.port == self.record.port
            {
                let _ = std::fs::remove_file(f);
            }
        }
    }
}

/// Handle to either an in-process `OwnedBridge` or another process's shared
/// loopback bridge.
pub enum TunnelHandle {
    Owned(OwnedBridge),
    Shared(ProxyRecord),
}

impl TunnelHandle {
    pub fn base_url_for(&self, target_port: u16) -> String {
        match self {
            TunnelHandle::Owned(b) => b.base_url_for(target_port),
            TunnelHandle::Shared(r) => r.base_url_for(target_port),
        }
    }

    pub fn state(&self) -> LinkState {
        match self {
            TunnelHandle::Owned(b) => b.state(),
            TunnelHandle::Shared(r) => probe_local_proxy(r).unwrap_or(LinkState::Connecting),
        }
    }

    pub fn is_alive(&self) -> bool {
        match self {
            TunnelHandle::Owned(_) => true,
            TunnelHandle::Shared(r) => probe_local_proxy(r).is_some(),
        }
    }

    pub fn fingerprint(&self) -> &str {
        match self {
            TunnelHandle::Owned(b) => &b.record.fingerprint,
            TunnelHandle::Shared(r) => &r.fingerprint,
        }
    }
}

/// Ensures a shared local loopback bridge is running for `cfg`:
/// - If no other LocalBeat process owns `<cache>/lock-tunnel`, starts our own.
/// - Otherwise reuses the running process's bridge if healthy & matching `cfg`.
pub fn ensure_shared_tunnel(cache_dir: &Path, cfg: &EmbeddedTunnel) -> Result<TunnelHandle> {
    let want_fp = config_fingerprint(cfg);
    for attempt in 0..5 {
        if let Some(lock) = lock::try_lock(cache_dir, "tunnel")? {
            let bridge = OwnedBridge::start_shared(cache_dir, lock, cfg)?;
            return Ok(TunnelHandle::Owned(bridge));
        }
        if let Some(rec) = ProxyRecord::load(cache_dir)
            && rec.fingerprint == want_fp
            && probe_local_proxy(&rec).is_some()
        {
            return Ok(TunnelHandle::Shared(rec));
        }
        // Owner may be starting up or shutting down; wait briefly and retry.
        thread::sleep(Duration::from_millis(80 * (attempt + 1)));
    }
    // Last attempt: if another process holds the lock for a *different* config
    // (e.g. during a re-join), fall back to an ephemeral bridge so this call
    // still works cleanly.
    Ok(TunnelHandle::Owned(OwnedBridge::start_ephemeral(cfg)?))
}

// ---------------------------------------------------------------------------
// Process-wide cached tunnel for Subsonic / admin API callers
// ---------------------------------------------------------------------------

static ACTIVE_TUNNEL: Mutex<Option<TunnelHandle>> = Mutex::new(None);

/// Drops any cached process-wide tunnel (call after re-joining with a new invite).
pub fn reset_active_tunnel() {
    if let Ok(mut guard) = ACTIVE_TUNNEL.lock() {
        *guard = None;
    }
}

/// Returns the current link state of the process-wide embedded tunnel, if active.
pub fn active_link_state() -> Option<LinkState> {
    let guard = ACTIVE_TUNNEL.lock().ok()?;
    guard.as_ref().map(TunnelHandle::state)
}

/// Extracts the TCP port from a URL like `http://10.8.0.1:4533` (default 80).
fn url_port(url: &str) -> u16 {
    let rest = url
        .trim()
        .strip_prefix("http://")
        .or_else(|| url.trim().strip_prefix("https://"))
        .unwrap_or(url.trim());
    let host_port = rest.split('/').next().unwrap_or("");
    if let Some((_, p)) = host_port.rsplit_once(':')
        && let Ok(port) = p.parse::<u16>()
    {
        return port;
    }
    if url.trim().starts_with("https://") { 443 } else { 80 }
}

/// Resolves the HTTP base URL for `Subsonic` (or any port on the server):
/// - If `prefs.tunnel` is `None`: returns `prefs.url` directly (co-located
///   loopback `http://127.0.0.1:4533` or legacy OS tunnel).
/// - If `prefs.tunnel` is `Some(cfg)`: ensures the shared embedded WireGuard
///   loopback bridge is running and returns `http://127.0.0.1:<port>/p/<token>/<target_port>`.
pub fn resolve_base_url(prefs: &ServerPrefs, target_port: Option<u16>, cache_dir: Option<&Path>) -> Result<String> {
    let Some(cfg) = &prefs.tunnel else {
        return Ok(prefs.url.trim().trim_end_matches('/').to_string());
    };
    let port = target_port.unwrap_or_else(|| url_port(&prefs.url));
    let want_fp = config_fingerprint(cfg);
    let mut guard = ACTIVE_TUNNEL.lock().map_err(|_| anyhow!("tunnel lock poisoned"))?;
    let reuse = guard
        .as_ref()
        .is_some_and(|h| h.fingerprint() == want_fp && h.is_alive());
    if !reuse {
        let dir = match cache_dir {
            Some(d) => d.to_path_buf(),
            None => Settings::load().cache_dir,
        };
        *guard = Some(ensure_shared_tunnel(&dir, cfg)?);
    }
    Ok(guard.as_ref().unwrap().base_url_for(port))
}

// ---------------------------------------------------------------------------
// Loopback HTTP listener -> TunnelEngine handoff
// ---------------------------------------------------------------------------

struct NewConn {
    target_port: u16,
    initial_bytes: Vec<u8>,
    stream: TcpStream,
}

fn run_listener(
    listener: TcpListener,
    token: String,
    conn_tx: mpsc::Sender<NewConn>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let token = token.clone();
                let conn_tx = conn_tx.clone();
                let state = Arc::clone(&state);
                let _ = thread::Builder::new().name("localbeat-proxy-req".into()).spawn(move || {
                    handle_loopback_conn(stream, &token, &conn_tx, &state);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn write_simple_http(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// Reads the HTTP request head from a loopback connection, checks the bearer
/// token, handles `/health/<token>`, or rewrites `/p/<token>/<port>/<path>`
/// and hands the stream off to `TunnelEngine`.
fn handle_loopback_conn(
    mut stream: TcpStream,
    token: &str,
    conn_tx: &mpsc::Sender<NewConn>,
    state: &AtomicU8,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 64 * 1024 {
                    write_simple_http(&mut stream, "431 Request Header Fields Too Large", "headers too large");
                    return;
                }
            }
            Err(_) => return,
        }
    };

    let head = match std::str::from_utf8(&buf[..header_end]) {
        Ok(s) => s,
        Err(_) => {
            write_simple_http(&mut stream, "400 Bad Request", "invalid utf-8 headers");
            return;
        }
    };
    let mut lines = head.split("\r\n");
    let Some(req_line) = lines.next() else {
        return;
    };
    let mut parts = req_line.split_whitespace();
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        write_simple_http(&mut stream, "400 Bad Request", "bad request line");
        return;
    };

    // Health check: GET /health/<token>
    if let Some(tok) = path.strip_prefix("/health/") {
        if tok != token {
            write_simple_http(&mut stream, "403 Forbidden", "forbidden");
            return;
        }
        let body = match LinkState::decode(state.load(Ordering::Relaxed)) {
            LinkState::Connecting => "connecting",
            LinkState::Connected(EndpointKind::Lan) => "lan",
            LinkState::Connected(EndpointKind::Public) => "public",
            LinkState::Stalled => "stalled",
        };
        write_simple_http(&mut stream, "200 OK", body);
        return;
    }

    // Forwarded request: /p/<token>/<target_port>/<rest...>
    let Some((target_port, forwarded_path)) = parse_proxy_path(path, token) else {
        write_simple_http(&mut stream, "403 Forbidden", "forbidden");
        return;
    };

    let mut rewritten = String::with_capacity(header_end + 64);
    rewritten.push_str(&format!("{method} {forwarded_path} {version}\r\n"));
    for line in lines {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("host:") || lower.starts_with("connection:") {
            continue;
        }
        rewritten.push_str(line);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str(&format!("Host: {SERVER_TUNNEL_IP}:{target_port}\r\n"));
    rewritten.push_str("Connection: close\r\n\r\n");

    let mut initial_bytes = rewritten.into_bytes();
    initial_bytes.extend_from_slice(&buf[header_end..]);

    let _ = stream.set_read_timeout(None);
    let _ = stream.set_nonblocking(true);
    let _ = conn_tx.send(NewConn { target_port, initial_bytes, stream });
}

fn parse_proxy_path<'a>(path: &'a str, expected_token: &str) -> Option<(u16, &'a str)> {
    let rest = path.strip_prefix("/p/")?;
    let (tok, rest) = rest.split_once('/')?;
    if tok != expected_token {
        return None;
    }
    let (port_str, tail) = match rest.split_once('/') {
        Some((p, t)) => (p, Some(t)),
        None => (rest, None),
    };
    let port: u16 = port_str.parse().ok()?;
    let forwarded = match tail {
        Some(t) => &path[path.len() - t.len() - 1..],
        None => "/",
    };
    Some((port, forwarded))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// smoltcp IP device + Happy-Eyeballs endpoint selector + WireGuard engine
// ---------------------------------------------------------------------------

struct WgIpDevice {
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
}

impl WgIpDevice {
    fn new() -> Self {
        WgIpDevice { rx_queue: VecDeque::new(), tx_queue: VecDeque::new() }
    }
}

struct WgRxToken(Vec<u8>);
impl RxToken for WgRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct WgTxToken<'a>(&'a mut VecDeque<Vec<u8>>);
impl<'a> TxToken for WgTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let res = f(&mut buf);
        self.0.push_back(buf);
        res
    }
}

impl Device for WgIpDevice {
    type RxToken<'a> = WgRxToken where Self: 'a;
    type TxToken<'a> = WgTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let pkt = self.rx_queue.pop_front()?;
        Some((WgRxToken(pkt), WgTxToken(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(WgTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = WG_MTU;
        caps
    }
}

/// Happy-Eyeballs dual-endpoint selector (`endpoint_lan` at t=0,
/// `endpoint_public` at t=150 ms).
struct EndpointSelector {
    lan_spec: Option<String>,
    pub_spec: Option<String>,
    lan_addr: Option<SocketAddr>,
    pub_addr: Option<SocketAddr>,
    active: Option<(EndpointKind, SocketAddr)>,
    probe_started_at: Option<Instant>,
    pub_probed: bool,
    last_rx_at: Option<Instant>,
    first_unanswered_at: Option<Instant>,
}

impl EndpointSelector {
    fn new(lan_spec: Option<String>, pub_spec: Option<String>) -> Self {
        let mut s = EndpointSelector {
            lan_spec,
            pub_spec,
            lan_addr: None,
            pub_addr: None,
            active: None,
            probe_started_at: Some(Instant::now()),
            pub_probed: false,
            last_rx_at: None,
            first_unanswered_at: None,
        };
        s.resolve();
        s
    }

    fn resolve(&mut self) {
        self.lan_addr = self.lan_spec.as_deref().and_then(resolve_udp_addr);
        self.pub_addr = self.pub_spec.as_deref().and_then(resolve_udp_addr);
    }

    fn start_reprobe(&mut self, now: Instant) {
        self.resolve();
        self.active = None;
        self.probe_started_at = Some(now);
        self.pub_probed = false;
    }

    /// Sends an outbound WireGuard UDP packet according to current state.
    fn send_packet(&mut self, udp: &UdpSocket, pkt: &[u8], now: Instant) {
        if self.first_unanswered_at.is_none() && self.last_rx_at.is_none() {
            self.first_unanswered_at = Some(now);
        }
        if let Some((_, addr)) = self.active {
            let _ = udp.send_to(pkt, addr);
            return;
        }
        // Probing: LAN first at t=0, Public after LAN_HEAD_START (or immediately if no LAN).
        let elapsed = self.probe_started_at.map(|t| now.saturating_duration_since(t)).unwrap_or_default();
        if let Some(lan) = self.lan_addr {
            let _ = udp.send_to(pkt, lan);
        }
        if let Some(pub_a) = self.pub_addr
            && (self.lan_addr.is_none() || elapsed >= LAN_HEAD_START)
        {
            let _ = udp.send_to(pkt, pub_a);
            self.pub_probed = true;
        }
    }

    /// Records a valid decapsulated WireGuard packet from `src`.
    fn on_valid_rx(&mut self, src: SocketAddr, now: Instant) -> LinkState {
        self.last_rx_at = Some(now);
        self.first_unanswered_at = None;
        let kind = if self.lan_addr == Some(src) {
            EndpointKind::Lan
        } else if self.pub_addr == Some(src) {
            EndpointKind::Public
        } else if let Some((k, _)) = self.active {
            k
        } else {
            EndpointKind::Public
        };
        self.active = Some((kind, src));
        self.probe_started_at = None;
        LinkState::Connected(kind)
    }

    /// Checks for stall / reprobe conditions and returns current `LinkState`.
    fn check_health(&mut self, has_traffic: bool, now: Instant) -> (bool, LinkState) {
        let mut need_probe_kick = false;
        if let Some(probe_t) = self.probe_started_at
            && !self.pub_probed
            && self.lan_addr.is_some()
            && self.pub_addr.is_some()
            && now.saturating_duration_since(probe_t) >= LAN_HEAD_START
        {
            self.pub_probed = true;
            need_probe_kick = true;
        }
        if has_traffic {
            if let Some(last) = self.last_rx_at
                && now.saturating_duration_since(last) >= STALL_REPROBE_AFTER
                && self.probe_started_at.is_none()
            {
                self.first_unanswered_at.get_or_insert(now);
                self.start_reprobe(now);
                need_probe_kick = true;
            }
        }
        let state = if let Some(t) = self.first_unanswered_at
            && now.saturating_duration_since(t) >= STALL_REPORT_AFTER
        {
            LinkState::Stalled
        } else if let Some((kind, _)) = self.active {
            LinkState::Connected(kind)
        } else {
            LinkState::Connecting
        };
        (need_probe_kick, state)
    }
}

fn resolve_udp_addr(spec: &str) -> Option<SocketAddr> {
    spec.trim().to_socket_addrs().ok()?.find(|a| a.is_ipv4()).or_else(|| spec.trim().to_socket_addrs().ok()?.next())
}

struct StreamBridge {
    stream: TcpStream,
    to_tunnel: VecDeque<u8>,
    to_loopback: VecDeque<u8>,
    loopback_eof: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_engine(
    mut tunn: Tunn,
    udp: UdpSocket,
    local_ip: Ipv4Addr,
    prefix: u8,
    server_ip: Ipv4Addr,
    endpoint_lan: Option<String>,
    endpoint_public: Option<String>,
    conn_rx: mpsc::Receiver<NewConn>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    reprobe_flag: Arc<AtomicBool>,
) {
    let start = Instant::now();
    let mut device = WgIpDevice::new();
    let mut iface = Interface::new(
        IfaceConfig::new(HardwareAddress::Ip),
        &mut device,
        SmolInstant::from_millis(0),
    );
    let oct = local_ip.octets();
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::v4(oct[0], oct[1], oct[2], oct[3]), prefix));
    });
    let srv_oct = server_ip.octets();
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        srv_oct[0], srv_oct[1], srv_oct[2], srv_oct[3],
    ));

    let mut sockets = SocketSet::new(vec![]);
    let mut bridges: HashMap<SocketHandle, StreamBridge> = HashMap::new();
    let mut next_ephemeral_port: u16 = 40_000;

    let mut endpoints = EndpointSelector::new(endpoint_lan, endpoint_public);
    let mut udp_buf = [0u8; 4096];
    let mut wg_out = [0u8; 4096];
    let mut last_timer = Instant::now();

    // Kick initial handshake immediately.
    if let TunnResult::WriteToNetwork(pkt) = tunn.format_handshake_initiation(&mut wg_out, false) {
        endpoints.send_packet(&udp, pkt, Instant::now());
    }

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let smol_now = SmolInstant::from_millis(now.duration_since(start).as_millis() as i64);

        if reprobe_flag.swap(false, Ordering::Relaxed) {
            endpoints.start_reprobe(now);
            if let TunnResult::WriteToNetwork(pkt) = tunn.format_handshake_initiation(&mut wg_out, true) {
                endpoints.send_packet(&udp, pkt, now);
            }
        }

        // 1. Accept any newly handed-off loopback connections.
        while let Ok(nc) = conn_rx.try_recv() {
            let rx_buf = SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
            let tx_buf = SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
            let mut sock = TcpSocket::new(rx_buf, tx_buf);
            let local_port = next_ephemeral_port;
            next_ephemeral_port = if next_ephemeral_port >= 60_000 { 40_000 } else { next_ephemeral_port + 1 };
            let remote = (IpAddress::v4(srv_oct[0], srv_oct[1], srv_oct[2], srv_oct[3]), nc.target_port);
            if sock.connect(iface.context(), remote, local_port).is_ok() {
                let handle = sockets.add(sock);
                bridges.insert(
                    handle,
                    StreamBridge {
                        stream: nc.stream,
                        to_tunnel: nc.initial_bytes.into(),
                        to_loopback: VecDeque::new(),
                        loopback_eof: false,
                    },
                );
            }
        }

        // 2. Drain inbound WireGuard UDP packets.
        loop {
            match udp.recv_from(&mut udp_buf) {
                Ok((n, src)) => {
                    let mut res = tunn.decapsulate(Some(src.ip()), &udp_buf[..n], &mut wg_out);
                    loop {
                        match res {
                            TunnResult::Done => break,
                            TunnResult::Err(_) => break,
                            TunnResult::WriteToNetwork(pkt) => {
                                let st = endpoints.on_valid_rx(src, now);
                                state.store(st.encode(), Ordering::Relaxed);
                                let _ = udp.send_to(pkt, src);
                                // Flush any queued packets after handshake completes.
                                res = tunn.decapsulate(Some(src.ip()), &[], &mut wg_out);
                            }
                            TunnResult::WriteToTunnelV4(ip_pkt, _) => {
                                let st = endpoints.on_valid_rx(src, now);
                                state.store(st.encode(), Ordering::Relaxed);
                                device.rx_queue.push_back(ip_pkt.to_vec());
                                break;
                            }
                            TunnResult::WriteToTunnelV6(_, _) => break,
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // 3. Poll smoltcp IP/TCP stack.
        iface.poll(smol_now, &mut device, &mut sockets);

        // 4. Pump bytes between loopback TcpStreams and smoltcp TcpSockets.
        let mut finished = Vec::new();
        let mut tmp = [0u8; 16 * 1024];
        for (&handle, br) in &mut bridges {
            let sock = sockets.get_mut::<TcpSocket>(handle);

            // Loopback -> tunnel
            if !br.loopback_eof && br.to_tunnel.len() < TCP_BUF_SIZE {
                match br.stream.read(&mut tmp) {
                    Ok(0) => br.loopback_eof = true,
                    Ok(n) => br.to_tunnel.extend(&tmp[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => br.loopback_eof = true,
                }
            }
            if sock.can_send() && !br.to_tunnel.is_empty() {
                let (a, _) = br.to_tunnel.as_slices();
                if let Ok(sent) = sock.send_slice(a) {
                    br.to_tunnel.drain(..sent);
                }
            }
            if br.loopback_eof && br.to_tunnel.is_empty() && sock.state() == tcp::State::Established {
                sock.close();
            }

            // Tunnel -> loopback
            if sock.can_recv() && br.to_loopback.len() < TCP_BUF_SIZE {
                let _ = sock.recv(|data| {
                    br.to_loopback.extend(&*data);
                    (data.len(), ())
                });
            }
            while !br.to_loopback.is_empty() {
                let (a, _) = br.to_loopback.as_slices();
                match br.stream.write(a) {
                    Ok(0) => {
                        br.to_loopback.clear();
                        sock.abort();
                        break;
                    }
                    Ok(n) => {
                        br.to_loopback.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        br.to_loopback.clear();
                        sock.abort();
                        break;
                    }
                }
            }

            if (sock.state() == tcp::State::Closed || sock.state() == tcp::State::TimeWait)
                && br.to_loopback.is_empty()
            {
                finished.push(handle);
            }
        }
        for h in finished {
            bridges.remove(&h);
            sockets.remove(h);
        }

        // 5. Poll again if sockets generated outbound TCP segments, then encapsulate via WireGuard.
        iface.poll(smol_now, &mut device, &mut sockets);
        while let Some(ip_pkt) = device.tx_queue.pop_front() {
            if let TunnResult::WriteToNetwork(pkt) = tunn.encapsulate(&ip_pkt, &mut wg_out) {
                endpoints.send_packet(&udp, pkt, now);
            }
        }

        // 6. WireGuard timers + Happy-Eyeballs reprobe check.
        let (need_kick, link_st) = endpoints.check_health(!bridges.is_empty(), now);
        state.store(link_st.encode(), Ordering::Relaxed);
        if need_kick
            && let TunnResult::WriteToNetwork(pkt) = tunn.format_handshake_initiation(&mut wg_out, false)
        {
            endpoints.send_packet(&udp, pkt, now);
        }
        if now.duration_since(last_timer) >= Duration::from_millis(250) {
            last_timer = now;
            if let TunnResult::WriteToNetwork(pkt) = tunn.update_timers(&mut wg_out) {
                endpoints.send_packet(&udp, pkt, now);
            }
        }

        // 7. Adaptive sleep: snappy when streaming, quiet when idle.
        let sleep_dur = if bridges.is_empty() {
            Duration::from_millis(25)
        } else {
            let delay = iface
                .poll_delay(smol_now, &sockets)
                .map(|d| Duration::from_millis(d.total_millis()))
                .unwrap_or(Duration::from_millis(5));
            delay.clamp(Duration::from_millis(1), Duration::from_millis(10))
        };
        thread::sleep(sleep_dur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::join::WgKeys;

    #[test]
    fn proxy_path_parsing_checks_token_and_port() {
        assert_eq!(parse_proxy_path("/p/secret/4533/rest/ping?u=a", "secret"), Some((4533, "/rest/ping?u=a")));
        assert_eq!(parse_proxy_path("/p/secret/8444/claim", "secret"), Some((8444, "/claim")));
        assert_eq!(parse_proxy_path("/p/secret/4533", "secret"), Some((4533, "/")));
        assert_eq!(parse_proxy_path("/p/wrong/4533/rest/ping", "secret"), None);
        assert_eq!(parse_proxy_path("/p/secret/notaport/x", "secret"), None);
    }

    #[test]
    fn shared_tunnel_lock_reuses_live_proxy_across_callers() {
        let dir = std::env::temp_dir().join(format!("localbeat-tun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let client_keys = WgKeys::generate().unwrap();
        let server_keys = WgKeys::generate().unwrap();
        let cfg = EmbeddedTunnel {
            private_key: client_keys.private,
            address: "10.8.0.12/32".into(),
            server_key: server_keys.public,
            preshared_key: String::new(),
            endpoint_public: None,
            endpoint_lan: Some("127.0.0.1:51829".into()),
            allowed_ips: "10.8.0.1/32".into(),
        };

        // First caller becomes owner.
        let h1 = ensure_shared_tunnel(&dir, &cfg).unwrap();
        assert!(matches!(h1, TunnelHandle::Owned(_)));
        assert!(h1.is_alive());

        // Second caller reuses the same loopback bridge without stealing the WireGuard key.
        let h2 = ensure_shared_tunnel(&dir, &cfg).unwrap();
        assert!(matches!(h2, TunnelHandle::Shared(_)));
        assert_eq!(h1.base_url_for(4533), h2.base_url_for(4533));
        assert!(h2.is_alive());

        // Dropping owner shuts down the bridge; next caller cleanly takes over.
        drop(h1);
        assert!(!h2.is_alive());
        let h3 = ensure_shared_tunnel(&dir, &cfg).unwrap();
        assert!(matches!(h3, TunnelHandle::Owned(_)));
        drop(h3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_selector_prefers_lan_then_falls_back_to_public() {
        let mut sel = EndpointSelector::new(
            Some("127.0.0.1:51821".into()),
            Some("127.0.0.1:51822".into()),
        );
        let t0 = Instant::now();
        // Before 150 ms, no public kick yet.
        let (kick, st) = sel.check_health(true, t0 + Duration::from_millis(50));
        assert!(!kick);
        assert_eq!(st, LinkState::Connecting);
        // After 150 ms, triggers public probe kick.
        let (kick, _) = sel.check_health(true, t0 + Duration::from_millis(200));
        assert!(kick);
        // Valid RX from LAN locks onto LAN.
        let st = sel.on_valid_rx("127.0.0.1:51821".parse().unwrap(), t0 + Duration::from_millis(210));
        assert_eq!(st, LinkState::Connected(EndpointKind::Lan));
        // Valid RX from Public locks onto Public.
        let st = sel.on_valid_rx("127.0.0.1:51822".parse().unwrap(), t0 + Duration::from_millis(220));
        assert_eq!(st, LinkState::Connected(EndpointKind::Public));
    }

    #[test]
    fn end_to_end_http_over_embedded_wireguard_and_smoltcp() {
        let client_keys = WgKeys::generate().unwrap();
        let server_keys = WgKeys::generate().unwrap();
        let psk_bytes = [0x42u8; 32];
        let psk_b64 = base64::engine::general_purpose::STANDARD.encode(psk_bytes);

        let srv_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let srv_addr = srv_udp.local_addr().unwrap();
        srv_udp.set_nonblocking(true).unwrap();

        let srv_stop = Arc::new(AtomicBool::new(false));
        let srv_stop_clone = Arc::clone(&srv_stop);
        let srv_priv = decode_key_32(&server_keys.private, "srv").unwrap();
        let cli_pub = decode_key_32(&client_keys.public, "cli").unwrap();

        // Mock WireGuard + smoltcp HTTP server at 10.8.0.1:4533.
        let server_thread = thread::spawn(move || {
            let mut tunn = Tunn::new(
                WgStaticSecret::from(srv_priv),
                WgPublicKey::from(cli_pub),
                Some(psk_bytes),
                None,
                1,
                None,
            );
            let mut device = WgIpDevice::new();
            let start = Instant::now();
            let mut iface = Interface::new(
                IfaceConfig::new(HardwareAddress::Ip),
                &mut device,
                SmolInstant::from_millis(0),
            );
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::new(IpAddress::v4(10, 8, 0, 1), 24));
            });
            let mut sockets = SocketSet::new(vec![]);
            let mut sock = TcpSocket::new(
                SocketBuffer::new(vec![0u8; 8192]),
                SocketBuffer::new(vec![0u8; 8192]),
            );
            sock.listen(4533).unwrap();
            let handle = sockets.add(sock);

            let mut peer: Option<SocketAddr> = None;
            let mut udp_buf = [0u8; 4096];
            let mut wg_out = [0u8; 4096];
            let mut req_buf = Vec::new();
            let mut replied = false;

            while !srv_stop_clone.load(Ordering::Relaxed) {
                let smol_now = SmolInstant::from_millis(start.elapsed().as_millis() as i64);
                while let Ok((n, src)) = srv_udp.recv_from(&mut udp_buf) {
                    peer = Some(src);
                    let mut res = tunn.decapsulate(Some(src.ip()), &udp_buf[..n], &mut wg_out);
                    loop {
                        match res {
                            TunnResult::Done | TunnResult::Err(_) => break,
                            TunnResult::WriteToNetwork(pkt) => {
                                let _ = srv_udp.send_to(pkt, src);
                                res = tunn.decapsulate(Some(src.ip()), &[], &mut wg_out);
                            }
                            TunnResult::WriteToTunnelV4(ip_pkt, _) => {
                                device.rx_queue.push_back(ip_pkt.to_vec());
                                break;
                            }
                            TunnResult::WriteToTunnelV6(_, _) => break,
                        }
                    }
                }
                iface.poll(smol_now, &mut device, &mut sockets);
                let s = sockets.get_mut::<TcpSocket>(handle);
                if s.can_recv() {
                    let _ = s.recv(|d| {
                        req_buf.extend_from_slice(d);
                        (d.len(), ())
                    });
                }
                if !replied && find_subslice(&req_buf, b"\r\n\r\n").is_some() && s.can_send() {
                    let req_text = String::from_utf8_lossy(&req_buf);
                    assert!(req_text.starts_with("GET /rest/ping HTTP/1.1"), "unexpected request: {req_text}");
                    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nlocalbeat!";
                    let _ = s.send_slice(resp);
                    s.close();
                    replied = true;
                }
                iface.poll(smol_now, &mut device, &mut sockets);
                while let Some(ip_pkt) = device.tx_queue.pop_front() {
                    if let (Some(p), TunnResult::WriteToNetwork(pkt)) = (peer, tunn.encapsulate(&ip_pkt, &mut wg_out)) {
                        let _ = srv_udp.send_to(pkt, p);
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
        });

        let cfg = EmbeddedTunnel {
            private_key: client_keys.private,
            address: "10.8.0.12/32".into(),
            server_key: server_keys.public,
            preshared_key: psk_b64,
            endpoint_public: None,
            endpoint_lan: Some(srv_addr.to_string()),
            allowed_ips: "10.8.0.1/32".into(),
        };
        let bridge = OwnedBridge::start_ephemeral(&cfg).unwrap();
        let url = format!("{}/rest/ping", bridge.base_url_for(4533));
        let mut resp = ureq::get(&url).call().expect("HTTP over embedded WireGuard should succeed");
        let body = resp.body_mut().read_to_string().unwrap();
        assert_eq!(body, "localbeat!");
        assert_eq!(bridge.state(), LinkState::Connected(EndpointKind::Lan));

        drop(bridge);
        srv_stop.store(true, Ordering::Relaxed);
        server_thread.join().unwrap();
    }
}
