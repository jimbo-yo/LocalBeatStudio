# The server as the library: design note (2026-11-14)

Status: decided, nothing built. LocalBeat Studio plays from the LocalBeat
server and writes new rips to it. Server side: `~/development/LocalBeatServer`
(the Mac Mini, `/srv/music/FLAC` + `/srv/music/Miscellaneous`).

## Decisions (user, 2026-11-14)

- **The server holds the one master copy.** Rips go to the server and leave
  the laptop once they're safely there. Every computer (Fedora, Ubuntu,
  Windows) plays the same library.
- **Play from anywhere, write from the home network only.**
- **Ripped away from home:** the rip finishes locally, waits in an outbox, and
  is pushed automatically the next time the laptop is on the home network.
- **One merged library in Studio:** local and server albums together, each
  album once (a local copy plays when there is one), server-only albums
  marked. On a laptop without music it's simply the server's library.

## Before anything leaves the laptop: backups

Today there are two copies (laptop and Mac). Server-only means **one copy on
one SSD**. Until the Mac's library has a backup (LocalBeatServer's open
question; M8), Studio pushes rips but **keeps the laptop copy** ("Remove from
this laptop after sending" stays off and says why). This is the gate for
the "server only" part, not for the rest.

## Writing: home network only, enforced by the server

The server enforces this. Studio doesn't have to:

- **Channel:** rsync over SSH to the Mac's LAN address `192.168.1.10`.
  The firewall already allows SSH only from `192.168.1.0/24`. Through
  WireGuard (10.8.0.x), even at home, SSH is refused.
- **A key just for Studio**, in the Mac's `authorized_keys`:
  `restrict,from="192.168.1.0/24",command="/usr/bin/rrsync -munge -no-del /srv/music" ssh-ed25519 …`
  It can only rsync inside `/srv/music`: no shell, no port forwarding,
  nowhere else, no deleting. One key per computer, so a lost laptop is one
  line to remove. **Built 2026-11-14:** LocalBeatServer
  `host/localbeat-writers`; this laptop is `my-laptop` with
  `~/.ssh/localbeat-studio`. Pushes need `--mkpath` (new folders) and
  `--chmod=D755,F644`; checked on the Mac (LocalBeatServer CLAUDE.md,
  "Writers").
- **"Am I home?":** Studio tries `192.168.1.10:22` with a short timeout and
  checks the pinned host key. If that works, it's the Mac on the home
  network. Nothing else is needed.
- **New albums show up in seconds:** after a push Studio calls the standard
  Subsonic `startScan` (to add to the server; today it rescans every 10
  minutes).
- **Deletes:** a re-rip that replaces an album, or the old Miscellaneous
  copy, needs files removed. rrsync allows `--delete`, but Studio only ever
  uses it inside the one album folder it is sending. Better still (to decide):
  the server moves replaced files to a trash folder kept for 30 days.

## The outbox (Rip → server)

1. A disc finishes as today (tags, cover, enrich, repair) in `~/Music/FLAC`,
   which becomes the **outbox and local cache** rather than the master.
2. The album is listed in `<data>/outbox.json`: "waiting for the server".
3. On the home network: `rsync -a --partial` the album to
   `/srv/music/FLAC/Artist/Album/`, then **verify**: a second
   `rsync --checksum --dry-run` must list nothing. Then `startScan`, and
   wait until the album is in the server's catalogue.
4. Only then is it marked sent, and removed locally if that's allowed
   (see Backups). A failure at any step leaves it in the outbox and says why.
5. Studio shows the outbox: on the Rip page per disc ("sent to the server" /
   "waiting for the home network") and as a sidebar count.

## Playing from the server (anywhere)

- **The laptop is a WireGuard peer** like a phone:
  `sudo localbeat-people add my-laptop` on the Mac, then the config is
  imported into NetworkManager (split tunnel, `AllowedIPs = 10.8.0.1/32`).
  At home the router loops the tunnel back, so one address works
  everywhere: `http://10.8.0.1:4533`.
- **Studio settings:** server address, user, secret. Pasting the
  `localbeat://connect?…` link fills them in.
- **A Subsonic client in core** (`core/subsonic.rs`, the calls LocalBeat
  uses: ping, getAlbumList2, getAlbum, search3, getCoverArt, download,
  startScan). The catalogue is cached on disk, so the library shows at once.
- **Audio: the original FLAC** (`download`), about 1 Mbps; the home upload
  is 200 Mbps. Studio's player (Symphonia) can't decode Opus, so streaming the
  server's Opus would need libopus. Maybe later, for hotspots.
- **Playback from a local file:** each song is downloaded into
  `<cache>/server-songs/` (size-limited, least recently played goes first)
  and played from the file as it fills, with the next songs fetched ahead.
  Seeking works, and the player stays file-based.
- **Not connected:** like the phone app (LocalBeat 0.9.4), Studio is
  connected or not. While it isn't, only albums on the laptop show. Keeping
  chosen albums on the laptop for offline listening: later.

## Things that change the library move to the server

All of these happen on the home network only, over the same channel:

| Today (on `~/Music`) | With the server as the library |
|---|---|
| `localbeat enrich` (tags) | Pull the album's FLACs, change tags, push back (or run on the Mac) |
| Repairs writing fixed tracks | Same: the fixed track goes to the outbox |
| Superseded albums (Misc copy of a re-rip) | Removed on the server (trash, see Deletes) |
| Phone sync (`localbeat sync`) | **Opus from the server** (`stream?format=opus`, already converted with the same settings), not encoded on the laptop; library list from the server catalogue |

## Moving over (once)

1. Check that the laptop's `~/Music/FLAC` + `Miscellaneous` match the Mac's
   (`rsync -a --checksum --dry-run`). Anything only on the laptop goes to the
   outbox.
2. Backups in place (see above).
3. Turn on "Remove from this laptop after sending" and clear what's already
   on the server, or keep it as a local cache. Your call at the time.

## Order of work

Each step works on its own:

1. **Server** (**done** 2026-11-14): Studio's write key
   (`localbeat-writers`, rrsync, LAN only), `startScan`/`getScanStatus`,
   and a check on the Mac that the key can't do anything else.
2. **The outbox:** push, verify, rescan after each rip; copies kept
   locally. Rips reach the server from now on.
3. **Laptop tunnel and login** (`localbeat-people add my-laptop`,
   NetworkManager import).
4. **Studio as a server client:** settings, catalogue, merged library,
   playing via the song cache, connected/not connected.
5. **Library edits through the server:** enrich, repairs, superseded.
6. **Phone sync from the server's Opus.**
7. **Backups**, then removing laptop copies (the switch, and the move-over
   check).

## Open

- Deleting on the server: plain `--delete` inside one album, or a trash
  folder on the Mac (recommended).
- Backups of `/srv/music`: what and where (LocalBeatServer).
- Opus playback in Studio (libopus) for slow connections.
- The Ubuntu and Windows laptops: same plan (a tunnel each, a write key
  each); Windows needs an SSH client and rsync. Leave until the Fedora
  laptop works.
