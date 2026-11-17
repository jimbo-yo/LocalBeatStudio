# Joining the server from Studio (2026-11-16)

A computer gets its tunnel, music login and (for an admin) its key for
sending rips from Studio's **Settings** page, approved by the admin on the
server's admin page. This replaces running `localbeat-people` /
`localbeat-writers` over SSH by hand (`tools/import-tunnel.py` still works).

## Decisions (user, 2026-11-16)

- **Request + approve.** Studio sends a request from the home network and
  shows a short code (`K7Q-4XM`). The admin page (`https://192.168.1.10:8443`)
  lists waiting requests with the same code: Approve / Approve as listener /
  Deny. Studio then finishes on its own.
- **Keys are made on the computer.** Studio makes its WireGuard key pair
  (x25519, `core/join.rs`) and, for an admin, `~/.ssh/localbeat-studio`
  (ed25519, `ssh-keygen`). Only public keys are sent; no private key
  crosses the network or exists on the Mac.
- **Two roles.** Every computer asks as a **listener** (tunnel + music
  login: plays the server's library). A deliberately quiet "Admin access"
  link asks as an **admin** instead (also a writer key: rips CDs and sends
  them). Only an approved admin sees the CD Ripper, Server, Review tags and
  Repairs pages and the CD drives in Settings, and only an admin starts the
  rip watcher. A settings file from before roles existed counts as admin.
- **Settings opens by itself on a new install** (no `localbeat.json`); it is
  also the sidebar's Settings item. `tools/install-windows.ps1` starts Studio
  at the end. Sections: the server (join / joined / waiting), music folders
  (saved in `localbeat.json` `folders`, read by `Settings::load`; env vars
  still win; used after a restart), phone (adb, wireless-debugging pairing),
  what this computer needs (WireGuard, adb; admins: rsync, OpenSSH; Install
  buttons on Windows through winget / MSYS2), CD drives (admins: offsets,
  checked against the known values).

## Protocol (LocalBeatServer `host/localbeat-admin`)

No admin password for these; the page is firewalled to the home network.

```
POST /join   Content-Type: application/json
     {"name", "role": "listener"|"admin", "computer",
      "wireguard_key": <public>, "ssh_key": <public, admins only>,
      "user": <optional: whose device, a suggestion>}
  -> 201 {"id", "token", "code"}       400 bad field, 409 name taken or
                                       already waiting, 429 five waiting
GET /join/ID     Authorization: Bearer TOKEN
  -> {"state": "waiting"} | {"state": "denied"}
   | {"state": "approved", "name", "role", "user",
      "tunnel": {"address", "server_key", "preshared_key", "endpoint", "allowed_ips"},
      "music": {"server", "login", "secret"},
      "writer": {"host", "user", "known_hosts"}}      (writer: admins only)
DELETE /join/ID  (same token): saved here, forget it   -> 204
```

- Requests are files in `/etc/localbeat/requests/` (root, 600; the token is
  kept as its SHA-256). Waiting ones last an hour, approved answers an hour
  or until Studio confirms with DELETE.
- Approve runs `localbeat-people add NAME --public-key KEY --json` (new
  option: no private key made; the JSON now has a `tunnel` object) and, for
  an admin, `localbeat-writers add NAME` as the music's owner (`setpriv`),
  then answers with the server's SSH host key for pinning. All or nothing:
  a failed writer step removes the person again. Removing a person on the
  page also removes their writer key.
- Studio doesn't check the page's self-made certificate: the code compared
  by the admin ties the request to the computer, and the page is home-only.
  Passive listeners on the LAN see nothing (TLS); an active man-in-the-middle
  at home could hand Studio a fake server. Pinning the page's certificate is
  a possible later step.

## On the computer

- `core/join.rs`: protocol, keys, the WireGuard config; `Pending` in
  `<config>/join.json` (holds the private key until the tunnel is
  installed; the approval is kept there too, so a failed setup is retried
  with Try again, not a new request).
- `platform/tunnel.rs`: the "localbeat" tunnel. Linux: NetworkManager import
  (autoconnect), like import-tunnel.py. Windows: WireGuard for Windows'
  tunnel service. Tested 2026-11-16: `/installtunnelservice` keeps reading
  the config from the given path at every start (deleting it breaks the
  service), so the config goes to `%ProgramData%\LocalBeat\localbeat.conf`
  (SYSTEM + Administrators only, like NetworkManager's root-only file),
  through one UAC prompt (an encoded PowerShell script run with RunAs).
- `engine/join.rs`: ask / check / finish / cancel. Finish installs the
  tunnel, saves `server.url/login/secret` and the role, and for an admin
  `server.host/user/key` (`~/.ssh/localbeat-studio`) and the pinned host key
  (`server_known_hosts`).
- `localbeat join [NAME] [--admin] [--server IP] [--cancel]` and
  `tools/import-tunnel.ps1` (a wrapper) do the same from a terminal.

## Not done / open

- Becoming an admin later: the admin removes the listener, then the
  computer asks again as admin (the name is taken otherwise).
- The Mac must run the new `host/setup.sh` (admin service sandbox: the
  music owner's `~/.ssh`, CAP_SETUID/SETGID; requests folder) before Studio
  can join.
- Tested 2026-11-16 (Surface, Windows side): `host/test-admin.sh` in WSL
  Fedora (71 checks pass); end to end, `localbeat join` as listener and as
  admin against the new admin page running in WSL with scratch files: code
  shown on both sides, approve, tunnel service installed through UAC
  (config in ProgramData not readable by the user), login and role saved,
  writer key on the stand-in's authorized_keys = `~/.ssh/localbeat-studio`,
  host key pinned, request deleted after confirm; Studio then shows the admin
  pages and the joined Settings.
- Tested 2026-11-16 against the Mac (after `host/setup.sh` ran there):
  `localbeat join my-windows-pc --admin` from the Surface's Windows side. Code
  shown, approved on the admin page (admin password needed to log in),
  tunnel service installed through UAC: 10.8.0.4, endpoint
  `vpn.example.com:51820`, 10.8.0.1 answers. `localbeat server check`:
  192.168.1.10 reachable, writing allowed with `~/.ssh/localbeat-studio`,
  music login works. Not yet: a real album pushed with that key, the join
  from Studio's Settings page against the Mac (the CLI and the page share
  `engine/join.rs`), Linux (NetworkManager import), away from home.

## Whose device (added 2026-11-17)

Each joined computer or phone is a *device* that belongs to a *user*; all of
a user's devices share their playlists (LocalBeat `docs/control.md`). Studio's
join form has an optional "Whose computer" box (`localbeat join --user jim`),
sent as `user`; the admin page's Approve has a "whose device?" box filled
with it, which the admin may change (empty: the device is a user of its
own). Approve runs `localbeat-people add NAME --user USER`; the people table
can change it later (`localbeat-people user NAME USER`). The users file is
`/etc/localbeat/users`, read by the music server with the logins.
