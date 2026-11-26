# Onboarding: phones and LocalBeat Studio (2026-11-24, rev 2)

Design for onboarding new phones (`LocalBeat`) and desktop installs
(`LocalBeatStudio`) to `LocalBeatServer`. Replaces the LAN-only
request-and-approve flow (`docs/joining.md`) and removes OS-level WireGuard
setup from client devices.

*Revision 2 incorporates design-review fixes: shared local loopback proxy
for concurrent GUI + CLI/MCP use, idempotent `POST /claim` with explicit
ACK, Happy-Eyeballs dual-endpoint roaming, admin-password gating for remote
invites, smart clipboard extraction, already-joined guard, and staged
cutover.*

## Goals and decisions

- **Effortless for non-technical people.** No server IPs, no separate
  WireGuard app or installer, no UAC / NetworkManager prompts, no Android
  VPN permission slot, no browser certificate warnings, and zero form fields
  for the recipient to fill out.
- **Works at home and away from home.** An admin can hand someone a QR code
  in person or text them an invite link remotely.
- **Admin-initiated invites for everything.** The admin creates a single-use
  invite (expires in 7 days, revocable anytime) from either the server's
  admin web page or an admin Studio's UI.
- **WireGuard-only server, embedded userspace WireGuard in both apps.**
  The server exposes no public HTTP port. Both `LocalBeat` (Android) and
  `LocalBeatStudio` / `localbeat` CLI embed a userspace WireGuard tunnel
  exposed internally as a local loopback HTTP bridge (`127.0.0.1`), carrying
  only LocalBeat's own traffic to the server.
- **One-time bootstrap invite + automatic key rotation (crash-safe).** The
  invite carries a short-lived, isolated bootstrap WireGuard key and a
  one-time claim token. When claimed, the client generates its own permanent
  private keys locally, dials the server over its embedded WireGuard tunnel,
  claims the invite idempotently, saves config to disk, and sends an
  explicit ACK (`DELETE /claim`) that immediately burns the bootstrap peer
  and invite.
- **Dual-endpoint Happy-Eyeballs roaming.** Both the invite and the saved
  tunnel config carry the server's LAN endpoint and public endpoint.
  Embedded WireGuard races LAN first (150 ms head start) against public,
  re-probes immediately on OS network changes, and refreshes endpoints from
  the server automatically.
- **Zero-field recipient flow.** The admin picks only the **user** (who the
  device belongs to, for shared playlists) and **role** (`listener` by
  default; optional `admin`). The device auto-names itself from its phone
  model or computer hostname during claim; the server deduplicates if needed.
- **Dedicated Welcome screen on first launch.** Both apps open to a clean
  Welcome screen (Scan QR / Paste invite, plus a quiet "Use without a
  server" option for local-only use) and load the server's library
  immediately after claiming — no app restart needed.
- **Admin security preserved.** Creating or revoking invites from an admin
  Studio over the tunnel requires the admin password (cached in memory for
  the session only, never written to `localbeat.json`). Admin rip pushing
  stays rsync-over-SSH on the home network using system `ssh-keygen`.
- **Clean final codebase, staged cutover.** Because embedded WireGuard
  clients are ordinary WireGuard peers to the server, we develop and test
  embedded WG alongside existing OS-WG devices first, then drop OS-WG code
  and the old `/join` flow once embedded WG is proven solid.

---

## Architecture

```
  Admin (web page or admin Studio + admin password)
     │  POST /invites {user, role}
     ▼
  LocalBeatServer
     │  Creates isolated bootstrap WG peer (10.8.250.x/32) + claim token (7d TTL)
     │  Returns localbeat://join/<payload> + QR code
     ▼
  Recipient (Phone or Studio — at home OR away)
     │  1. Scans QR or pastes text containing localbeat://join/<payload>
     │     (if already joined, asks confirmation before replacing)
     │  2. Generates permanent WG keypair (+ SSH key via ssh-keygen if admin)
     │  3. Brings up embedded userspace WG with bootstrap key
     │     (races LAN at t=0, public at t=150 ms)
     │  4. Calls POST http://10.8.0.1:<claim-port>/claim inside the tunnel
     ▼
  LocalBeatServer
     │  5. Verifies token + bootstrap peer IP (idempotent for same WG key)
     │  6. Auto-names & deduplicates device, creates permanent WG peer +
     │     Subsonic login (+ writer authorized_keys entry if admin)
     │  7. Caches 200 OK response for 5 min (for retry safety) and returns it
     ▼
  Recipient (Phone or Studio)
     │  8. Saves permanent config to disk
     │  9. Sends DELETE http://10.8.0.1:<claim-port>/claim (explicit ACK)
     │     -> Server immediately removes bootstrap WG peer + burns invite
     │ 10. Switches embedded WG to permanent keypair; loads library live
```

### Embedded userspace WireGuard & local loopback bridge

- **Shared loopback HTTP bridge architecture (both platforms):**
  Instead of wiring custom userspace TCP sockets into every HTTP client
  (`ureq`, `player`, `OkHttp`, `ExoPlayer/Media3`, `Coil`, Android Auto),
  the embedded WireGuard engine runs an **in-process loopback HTTP bridge**
  bound to `127.0.0.1:<ephemeral-port>` (authenticated by a per-process
  random bearer token) that forwards requests to `10.8.0.1` over userspace
  WireGuard + TCP (`boringtun`/`gotatun` + `smoltcp` in Rust; `wireguard-go`
  `netstack` or shared Rust crate via JNI/UniFFI on Android).
  - Event-driven timers (no busy polling) to preserve battery.
  - On Android, background sync tasks (such as keeping playlist songs
    locally) run inside WorkManager expedited/foreground workers so Android
    Doze does not freeze the UDP socket mid-transfer.
- **Desktop GUI + CLI / MCP concurrency (no WireGuard peer conflict):**
  Two processes using the same WireGuard private key simultaneously would
  steal each other's UDP session on the server. To prevent this:
  - Whichever local process starts first (`localbeat-studio`, `localbeat
    mcp`, or a CLI command) acquires `<cache>/lock-tunnel`, starts the
    embedded WireGuard + loopback HTTP bridge, and writes
    `<cache>/tunnel-proxy.json` (`{ "port", "pid", "token" }`, mode `0600`
    on Unix).
  - Any concurrent local process (`localbeat playlist`, `localbeat mcp`, or
    `localbeat-studio` starting while `mcp` is running) sees the live proxy
    in `tunnel-proxy.json` and routes its HTTP calls through it — **zero
    peer fighting and 0 ms handshake latency** for CLI commands while Studio
    is open.
  - If the proxy owner exits while another process is active, the next
    request detects the closed port, acquires `lock-tunnel`, and takes over
    automatically.
- **Dual-endpoint Happy-Eyeballs roaming:**
  - Every tunnel config stores `endpoint_lan` (optional, e.g.
    `192.168.1.10:51820`) and `endpoint_public` (e.g.
    `vpn.example.com:51820`).
  - On handshake, network change, or stall (>3 s with no response), send a
    WireGuard handshake to `endpoint_lan` at `t=0` and to `endpoint_public`
    at `t=150 ms`; lock onto whichever completes first. This gives LAN
    speed at home with at most 150 ms overhead on an unfamiliar Wi-Fi.
  - On Android, hook `ConnectivityManager.NetworkCallback` to trigger an
    immediate re-probe on Wi-Fi ↔ cellular switches (avoiding 10–15 s
    blackouts when leaving home).
  - Clients refresh `endpoint_lan` / `endpoint_public` automatically from
    the server's status/ping response so a changing LAN IP or DDNS never
    bricks existing devices.

---

## Invite URI format

Invites are encoded as a `localbeat://` URI:

```
localbeat://join/<base64url-no-pad>
```

Compact UTF-8 JSON payload (~260–320 bytes before base64url, fits in a
Version 11–13 QR code with Medium error correction):

```json
{
  "v": 1,
  "t": "<claim-token-24-bytes-b64url>",
  "k": "<bootstrap-wg-private-key-b64>",
  "p": "<bootstrap-wg-psk-b64>",
  "a": "10.8.250.2/32",
  "s": "<server-wg-public-key-b64>",
  "e": "vpn.example.com:51820",
  "l": "192.168.1.10:51820",
  "r": "listener"
}
```

- `v`: schema version (`1`).
- `t`: 192-bit random one-time claim token (stored on server as SHA-256).
- `k`, `p`, `a`: bootstrap WireGuard private key, preshared key, and IP in
  `10.8.250.0/24`.
- `s`: server WireGuard public key.
- `e`, `l`: server public and LAN WireGuard endpoints (`host:port`). Both
  are optional, as long as at least one is present (supports LAN-only
  desktop servers that haven't configured DDNS / port forwarding yet).
- `r`: intended role (`"listener"` or `"admin"`).

**Smart paste extraction:** Because many SMS/chat apps do not linkify
`localbeat://` and users often copy an entire message (*"Hey Mom, paste
this: localbeat://join/eyJ..."*), both apps extract
`localbeat://join/[A-Za-z0-9_-]+` from anywhere inside pasted/clipboard
text. (A future optional static `https://.../join#<payload>` landing page
can wrap this same payload for clickable SMS links + app download buttons
without any server changes.)

---

## Server protocol (`LocalBeatServer`)

### 1. Bootstrap peer isolation

- Permanent devices: `10.8.0.2`–`10.8.249.254`.
- Bootstrap invite peers: `10.8.250.2`–`10.8.250.254`.
- Bootstrap peers in `10.8.250.0/24` must be isolated so they can reach
  **only** TCP `10.8.0.1:<claim-port>` (all other ports `:4533`, `:22`,
  `:8443`, LAN dropped). On a Linux server using kernel `wg0` this is
  enforced via host firewall rules; on a future co-located Windows/desktop
  server using an in-process userspace WireGuard server, the same rule is
  enforced in-process without OS firewall changes.
- Periodic cleanup removes expired invites (7-day TTL) and un-ACKed claimed
  invites (5-minute grace TTL) along with their bootstrap WireGuard peers,
  recycling IPs.

### 2. Idempotent claim endpoint (bootstrap peers only)

Listening on `10.8.0.1:<claim-port>` (plain HTTP inside WireGuard):

```
POST /claim
Content-Type: application/json

{
  "token": "<claim-token>",
  "wireguard_key": "<client-generated-permanent-public-key-b64>",
  "device_kind": "phone" | "studio",
  "device_label": "Pixel 9" | "MY-LAPTOP (Windows)",
  "suggested_name": "pixel-9",
  "ssh_key": "ssh-ed25519 AAAA..."   // required iff invite role == "admin"
}
```

Behavior:
1. Verify source IP is in `10.8.250.0/24` and matches the invite for
   `SHA-256(token)`.
2. If already claimed within the 5-minute grace window:
   - Same `wireguard_key`: return the cached `200 OK` response (safe retry
     on dropped packets / app crash).
   - Different `wireguard_key`: return `410 Gone`.
3. If expired or unknown: return `404` / `410`.
4. If invite role is `"admin"` and `device_kind != "studio"` (or `ssh_key`
   is missing/invalid): return `400 {"error": "This invite is for an admin computer (LocalBeat Studio)."}` without burning the invite.
5. Derive a unique device name (slugify `suggested_name`, prefix/combine
   cleanly with `user` if appropriate, auto-append `-2`, `-3`, … on
   collision).
6. Atomically create the permanent WireGuard peer + Subsonic login (and
   writer key + admin role if `admin`). Roll back on failure, leaving the
   invite intact.
7. Cache the `200 OK` response with a 5-minute grace expiry and return it:

```json
{
  "name": "sarah-pixel-9",
  "user": "sarah",
  "role": "listener",
  "tunnel": {
    "address": "10.8.0.12/32",
    "server_key": "<server-wg-public-key>",
    "preshared_key": "<permanent-psk>",
    "endpoint_public": "vpn.example.com:51820",
    "endpoint_lan": "192.168.1.10:51820",
    "allowed_ips": "10.8.0.1/32"
  },
  "music": {
    "server": "http://10.8.0.1:4533",
    "login": "sarah-pixel-9",
    "secret": "<subsonic-password>"
  },
  "writer": {
    "host": "192.168.1.10",
    "user": "musicuser",
    "known_hosts": "192.168.1.10 ssh-ed25519 AAAA..."
  }
}
```
(`writer` is omitted for `listener`.)

8. **Explicit ACK:** Once the client has durably saved its config to disk,
   it sends:
   ```
   DELETE /claim
   Authorization: Bearer <claim-token>
   ```
   Server immediately removes the bootstrap WireGuard peer and deletes the
   invite + cached response (`204 No Content`). If the ACK is lost, the
   5-minute grace timer cleans it up automatically.

### 3. Admin invite API (web UI + admin Studio over tunnel)

Accessible to:
- The server's admin web page (`localbeat-admin`, home network).
- Admin Studio peers over the WireGuard tunnel (`10.8.0.1:<admin-tunnel-port>`),
  requiring **both** an active `admin` WireGuard peer IP **and** a valid
  admin session token obtained by entering the **admin password** (kept in
  memory only on Studio — never written to `localbeat.json`).

Endpoints:
- `POST /admin/login` — `{ "password": "..." }` → `200 { "session_token", "expires_at" }`
- `POST /invites` — `{ "user": "sarah", "role": "listener" | "admin" }` →
  `201 { "id", "user", "role", "expires_at", "uri": "localbeat://join/..." }`
- `GET /invites` — lists active unclaimed invites.
- `DELETE /invites/:id` — revokes an unclaimed invite and removes its
  bootstrap WireGuard peer (`204`).
- `GET /users` — lists existing user names for Studio's invite dropdown.

---

## First-run and invite UX

### LocalBeat (Android)

1. **First launch (no server configured, no local-only flag):**
   - Dedicated **Welcome** screen:
     - **Scan invite QR** (built-in camera scanner).
     - **Paste invite** (1-tap clipboard read with smart URI extraction, or
       paste box).
     - Registers `localbeat://join` Android intent filter.
     - Quiet **"Use without a server"** link for local-only phones.
2. **Already-joined guard:** Opening or pasting an invite on an already
   configured device prompts *"Replace your current server connection
   (<device-name>)?"* before claiming.
3. **Claiming:** Single spinner → generates permanent WG keypair → dials
   bootstrap tunnel → `POST /claim` → saves config → `DELETE /claim` →
   switches to permanent tunnel → loads Library immediately (no restart).
4. **Connection / revocation feedback:** Shows Home / Away / Offline; if
   both endpoints fail to handshake for >20 s while the device has internet,
   surfaces *"Can't reach the music server — if this device was removed,
   ask for a new invite"* with a quick re-join button.

### LocalBeat Studio (Desktop) & CLI

1. **First launch (no `localbeat.json`):**
   - Dedicated **Welcome** screen (replaces auto-opening Settings):
     - **Paste invite** (with 1-tap **"Paste from clipboard"** when a
       `localbeat://join/` URI is detected anywhere in clipboard text).
     - Registers `localbeat://` URI handler on Windows and Linux.
     - Quiet **"Use without a server"** link (saves local-only mode and
       opens the Library).
2. **Claiming:**
   - Generates permanent WG keypair locally (and if `r == "admin"`, uses
     system `ssh-keygen` to create `~/.ssh/localbeat-studio` if missing).
   - Claims over bootstrap tunnel, saves `localbeat.json` (mode `0600` on
     Unix), sends `DELETE /claim`, starts the permanent loopback bridge, and
     loads the server library live (no restart; if `admin`, reveals Rip /
     Server / Review / Repairs tabs immediately).
   - Headless/CLI claiming: `localbeat join <invite-uri>`.
3. **Admin Studio — "Invite a device" UI & CLI:**
   - Available on the Server / Settings page for admin Studios (and via
     `localbeat invite create --user <user> [--admin]`, `localbeat invite
     list`, `localbeat invite revoke <id>`).
   - Prompts once per session for the admin password (held in memory only).
   - Admin picks an existing user (or types a new one) + optional "Admin
     access" checkbox → displays a **large QR code** + **"Copy invite
     link"** button, plus a list of active invites with Revoke buttons.
4. **Settings cleanup (after cutover):**
   - Remove the WireGuard prerequisite row, `platform/tunnel.rs` UAC /
     NetworkManager installer, `tools/import-tunnel.*`, and old `/join`
     code.
   - Show connection state (Home / Away / Offline), device/user info, and
     "Join with a new invite…".
   - Only show `rsync` / `adb` prerequisites to users who need them.

---

## Rollout order (staged, no big-bang breakage)

1. **`LocalBeatServer` (additive first):**
   - Add `10.8.250.0/24` bootstrap subnet + firewall rules allowing only
     `10.8.0.1:<claim-port>`.
   - Implement invite storage (7-day TTL, 5-minute claim grace window,
     cleanup), idempotent `POST /claim` + `DELETE /claim`, and
     password-gated admin invite API (`/admin/login`, `/invites`, `/users`).
   - Return current `endpoint_lan` / `endpoint_public` in server status so
     clients stay fresh.
   - Keep existing `wg0` peers working untouched during client rollout.
2. **`LocalBeatStudio` (prove on dev machine first):**
   - Implement embedded userspace WireGuard + shared loopback HTTP bridge
     (`tunnel-proxy.json` + `lock-tunnel`) + Happy-Eyeballs dual-endpoint
     roaming in `localbeat-core`.
   - Implement invite URI extraction/parsing, idempotent `POST /claim` +
     `DELETE /claim`, Welcome screen, live post-claim library load, Admin
     "Invite a device" UI (with in-memory admin password prompt), and
     `localbeat join` / `localbeat invite` CLI commands.
   - Verify concurrent `localbeat-studio` + `localbeat mcp` + `localbeat
     playlist` on Windows and Linux.
3. **`LocalBeat` (Android):**
   - Integrate embedded userspace WireGuard loopback bridge +
     `ConnectivityManager.NetworkCallback` roaming + WorkManager foreground
     handling for background downloads.
   - Build Welcome screen (in-app QR scanner, 1-tap smart Paste,
     `localbeat://join` intent filter, already-joined guard).
4. **Cutover & cleanup:**
  - Re-onboard existing phones and computers with fresh invites and verify
    daily use at home and away.
  - Remove legacy OS WireGuard tunnels from devices, and delete
    `platform/tunnel.rs`, `tools/import-tunnel.*`, and old `/join`
    endpoints from `LocalBeatServer` and `LocalBeatStudio`.

---

## Future-proofing: co-located local server (desktop daemon / Windows service)

Most end users will not have a dedicated home server; a future Studio
installer option can install `LocalBeatServer` as a local daemon (Linux
systemd) or service (Windows) on the same desktop, serving the user's local
`FLAC` and `Miscellaneous` folders directly.

To ensure nothing in this design prevents that later, keep five contract
invariants open:

1. **Direct loopback transport (`tunnel: None`):** When Studio connects to
   a co-located server at `http://127.0.0.1:4533`, it skips embedded
   WireGuard and talks straight to loopback. This adds no OS-WireGuard
   complexity — `tunnel` is simply `Option<EmbeddedTunnel>`.
2. **Both `endpoint_lan` and `endpoint_public` optional (at least one):**
   A home desktop without DDNS or port-forwarding can immediately onboard
   phones over home Wi-Fi using `endpoint_lan` alone; if remote access is
   configured later, `endpoint_public` propagates via automatic endpoint
   refresh.
3. **In-process bootstrap isolation allowed:** Specifying `10.8.250.0/24`
   isolation as a protocol invariant (rather than Linux-only `iptables`)
   lets a Windows service run an in-process userspace WireGuard server with
   zero kernel drivers or firewall scripts, while speaking the exact same
   invite & claim protocol to phones and remote Studios.
4. **Admin role decoupled from remote SSH/`rsync`:** A co-located Admin
   Studio shares the music folders directly with the local server — no
   `outbox`, `rsync`, `MSYS2`, or `ssh-keygen` needed (just trigger a local
   library scan after ripping/removing). Treat `writer` as optional
   (`RemoteSsh` vs `LocalDirect`) rather than mandatory for `role == Admin`.
5. **Same admin API & Welcome bypass:** A co-located server exposes the
   exact same `/admin/login`, `/invites`, and `/users` API on loopback, so
   Studio's "Invite a device" QR/copy UI works identically. On first launch
   after an installer sets up a co-located server, Studio skips the Welcome
   invite screen and opens straight into the Library as Admin, ready to
   invite the user's phone.
