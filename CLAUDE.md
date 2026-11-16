# LocalBeat Studio

Desktop app (Rust, Linux + Windows) for a personal music library: rips CDs to
verified FLAC (AccurateRip / CUETools DB, repair of damaged discs), tags them
from MusicBrainz with cover art, plays the library, fixes tags on older files
(MusicBrainz + AcoustID), and syncs Opus copies to an Android phone.

Repo history: started 2026-11-04 as "CDRipPipeline", a set of shell/Python
scripts (whipper, then cyanrip). The Rust app (first called "ripshelf")
replaced them all; the scripts were deleted 2026-11-14 and are kept at git tag
`scripts-final`. The old day-by-day notes, with test results per disc and the
reasons behind most rules below, are in **`docs/history.md`**.

## The LocalBeat suite (change contracts together)

| Repo | What |
|---|---|
| this one | LocalBeat Studio (`localbeat-studio`) + CLI (`localbeat`) |
| `../LocalBeat` | Android player app (Pixel 9). Reads the phone folder Studio syncs |
| `../LocalBeatServer` | Mac Mini server (`/srv/music/FLAC`, `/srv/music/Miscellaneous`), streams Opus on demand |

Each repo's CLAUDE.md records the library contract below. If you change
folder names, tags, the phone record, or Opus settings, update all three
repos. The LocalBeat mark (`gui/assets/localbeat.svg`, gold disc #F2B33D +
navy #1B2A3A note) is also shared (`docs/brand/` in the other two).

## Machines

| Machine | OS | Drives |
|---|---|---|
| Lenovo laptop (original) | Fedora 44, KDE Plasma, Intel Haswell | sr0 built-in GU90N (broken), Apple SuperDrive, hp GTA0N |
| Ubuntu laptop | Ubuntu 26.04, GNOME | sr0 built-in GU90N, hp GTA0N |
| Windows laptop | Windows, Rust MSVC | hp GTA0N on D: |
| Microsoft Surface laptop (added 2026-11-14) | Fedora 45, KDE Plasma | none built in (USB drives only) |

- Check `/etc/os-release` and the hardware to tell the machines apart (two
  run Fedora)
- Repo in `~/development/<repo>`; `~/Development/<repo>` (capital D) on the
  Surface. Editor preference: **vi** (say vi, not nano, in instructions)
- Surface setup (2026-11-14): fresh install, nothing built or installed yet;
  needs the Fedora build packages below
- The music libraries are NOT synced between machines (the server will become
  the one copy: `docs/server-library.md`)

## Layout

Cargo workspace at the repo root (edition 2024):

- `core/` (`localbeat-core`): no OS code, no UI. Rip rules (`rip.rs`,
  `secure_read.rs`, `vote.rs`), AccurateRip (`accuraterip.rs`), CUETools DB
  (`ctdb.rs`, `ctdb_repair.rs`, `repair_disc.rs`), MusicBrainz
  (`musicbrainz.rs`), covers (`cover.rs`, `covers.rs`), FLAC write/read
  (`flac_encode.rs` = libFLAC, `flacfile.rs`), other formats (`audio_source.rs`,
  Symphonia), Opus copies (`phone_copy.rs`), phone sync plan (`phone_sync.rs`),
  library scan (`library.rs`), tag enrichment (`enrich.rs`, `enrich_review.rs`,
  `acoustid.rs`), prefs/offsets/server settings (`config.rs`), server outbox
  (`outbox.rs`), Subsonic client (`subsonic.rs`), naming, locks, HTTP (1
  req/s, 503 retries)
- `platform/` (`localbeat-platform`): CD drives (`drive.rs`, `linux.rs`,
  `windows.rs`; reads via cd-da-reader), phone transports (`phone.rs`: adb,
  KDE Connect mount, folder), phone notifications (`phone_notify.rs`)
- `engine/` (`localbeat-engine`): the rip session (rip_loop per drive,
  `Event`s, `Frontend` trait), notifications summary, sending the outbox to
  the server (`push.rs`: rsync over SSH, scan, catalogue check)
- `player/` (`localbeat-player`): playback engine, no UI (Symphonia → cpal,
  gapless, exact seek)
- `cli/` (`localbeat`): `rip [--loop]`, `check`, `drives`, `toc`,
  `read-track`, `identify`, `offset`, `sync`, `phone`, `enrich`,
  `fingerprint`, `superseded`, `repair`, `ctdb`, `library`, `play`, `notify`,
  `eject`, `server [set|off|check|trust]`, `outbox [add]`, `push`
- `gui/` (`localbeat-studio`, eframe/egui 0.37): pages Library, Rip, Phone,
  Server (`server_tab.rs`; sender thread `server_push.rs`), Review tags,
  Repairs; MPRIS (`media_keys.rs`); design system "Prism" in
  `theme.rs` (`docs/ui-redesign.md`)
- `tools/`: `install-linux.sh`, `cmake-libdir-lib` (Fedora build fix)
- `docs/`: design notes (below) and `history.md`

## Build, test, install

- Fedora: `sudo dnf install rust cargo rustfmt clippy cmake gcc gcc-c++
  alsa-lib-devel`. Ubuntu: `sudo apt install build-essential cmake
  libasound2-dev` + rustup or distro Rust. Windows: `winget install
  Kitware.CMake` (a new shell may need `C:\Program Files\CMake\bin` on PATH)
- `cargo test --workspace`; online tests: `cargo test -p localbeat-core --
  --ignored`
- Install on Linux: `tools/install-linux.sh` (release build, ~10-13 min the
  first time) → `~/.local/bin/localbeat{,-studio}`, menu entry, icons. It uses
  `install` (new inode), so a running copy is undisturbed
- Screenshots / layout work without a disc: `LOCALBEAT_DEMO=1` (also `phone`,
  `review`, `sr2`, `choice`, `alert`, `repairs`)
- Build quirks:
  - Fedora: `.cargo/config.toml` points `CMAKE_x86_64_unknown_linux_gnu` at
    `tools/cmake-libdir-lib` (adds `-DCMAKE_INSTALL_LIBDIR=lib` and turns off
    CMake IPO for opus-static-sys) and sets `-ffat-lto-objects` for
    libopusenc. After changing the wrapper, delete
    `target/*/build/opus-static-sys-*` (CMake caches it). Harmless on Ubuntu
  - Windows/MSVC: opus symbols get lost with LTCG; `core/build.rs` links
    opus.lib directly. Dependencies are built optimised even in dev (debug CRT
    mismatch otherwise, and debug Symphonia is very slow)
  - GUI on Linux forces `WGPU_BACKEND=gl` (Vulkan garbled all text on the
    Haswell laptop)
  - egui 0.37 API: `fn ui(&mut self, ui, frame)`, `egui::Panel::top/bottom/
    left(..).show(ui, ..)`; TopBottomPanel/SidePanel/update() are gone. A
    bottom Panel reserves ~10 px less than it draws
  - A hidden/minimised window gets no frames on KDE Wayland: anything that
    must keep running (MPRIS, repeat) lives off the UI thread

## Names kept on purpose

Renamed ripshelf → LocalBeat Studio on 2026-11-11 (app id `localbeat-studio`,
MPRIS `org.mpris.MediaPlayer2.localbeat`, prefs `localbeat.json`; old
`ripshelf.json` still read). Unchanged on purpose: `~/.config/rip-cd`,
`~/.cache/rip-cd` and the phone's `Music/.ripshelf-sync.json` (the LocalBeat
app reads it: rename only together with that repo).

## Files and folders

- Library: `~/Music/FLAC` (`FLAC_DIR`), plus Miscellaneous (`MISC_DIR` /
  Settings, old AAC/MP3/.m4p kept as they are). `~/Music/Superseded`: Misc
  albums that duplicate a FLAC album (`localbeat superseded --move`)
- Config `~/.config/rip-cd/` (Windows `%APPDATA%\rip-cd\`): `offsets`,
  `localbeat.json` (prefs, AcoustID application key, `server` settings with
  the music server secret: never in git), `outbox.json` (albums for the
  server), `server_known_hosts` (the server's pinned SSH host key),
  `player.json` (playback session), `enrich/` (state + undo backups),
  `superseded.json`
- Cache `~/.cache/rip-cd/`: `rejected/<Artist - Album>/` (rejected tracks,
  their reads in `reads/`, `repair.json`), `library.json`, `covers/`, locks
  `lock-<drive>`, `lock-phone`, `lock-server-push`, `lock-outbox`,
  `lock-album-<crc>` (a disc ripping into it), `sync-work/`

## Library contract (shared with LocalBeat and LocalBeatServer)

- `Artist/Album/NN. Artist - Title.flac`; multi-disc sets in one folder as
  `2.07. Artist - Title.flac`; `cover.jpg` + embedded front cover (type 3);
  `<Album>[ CDn].log` rip log. Windows-safe names (`?` → `？` etc.)
- Picard tag names: ALBUMARTIST, TRACKTOTAL, DISCTOTAL, RELEASETYPE,
  MUSICBRAINZ_*, MUSICBRAINZ_DISCID, CDTOC, ISRC; ORIGINALDATE/ORIGINALYEAR
  (DATE is the CD's own release); up to 3 GENRE values (release group's by
  votes, else the artist's). Older whipper/cyanrip rips use lowercase keys:
  compare keys case-insensitively
- Phone: Opus 160 kbps (lossy sources at min(160, own bitrate), ≥ 64), tags +
  cover copied, REPLAYGAIN_*/R128_* dropped (opusenc made them a bogus +99.6 dB
  header gain). .m4p (DRM) never converted

## Ripping rules

- A track is kept only if it matches AccurateRip, or a clean read repeats an
  earlier clean read's CRC (reads with unreadable sectors never count). Up to
  4 reads with drive-cache flush; never overwrite; encode to `.part` then
  rename; failures to the rejected folder; eject when done
- Only tracks missing from the library are ripped (resume)
- Other pressings: AR frame-450 checksums find a shift within ±10 sectors
  (Acceptance::OtherPressing). Identical reads with no AR match but an
  unshifted frame-450 match = consistent misread: saved, shown amber "clean &
  re-rip" (RepeatDiffers)
- Damaged spots: a stalled chunk (the 30 s READ CD timeout, then a USB reset)
  is skipped with a growing gap (max 750 sectors), budget 150 s per read.
  Rejected tracks keep all clean reads; after the eject the disc is repaired
  with CUETools DB parity (`docs/ctdb-repair.md`) and written if CTDB +
  AccurateRip agree (or CTDB ≥ 3 when AR doesn't know it)
- MusicBrainz: exact disc ID, else releases whose offsets are within 3 sectors
  (names from CTDB); ranking US, Official, not club edition, CD, earliest. A
  fuzzy-only match needs `-R ID` or "rip as unknown"
- Every drive rips at once (one loop per drive, per-drive lock). Rips no
  longer make phone copies (phone sync converts at sync time)
- Offset sign: +N = the audio starts N frames later in what the drive returns
  (EAC/AccurateRip convention)

## Drives

Offsets file key = udev ID_SERIAL minus `-0:0` (the same key on Windows), with
ID_SERIAL_SHORT as fallback.

| Drive | Key | Offset |
|---|---|---|
| hp DVDRAM GTA0N (USB slim, LG) | `hp_DVDRAM_GTA0N_423442525150313439373432` | **+102** |
| Apple SuperDrive A1379 (inside: HL-DT-ST GX50N) | `Apple_SuperDrive_KZ4F9KI4627` (was `HL-DT-ST_DVDRW_GX50N_KZ4F9KI4627`) | **+6** |

- Offset finding from one disc can be fooled by an odd pressing (it once gave
  -1151 and +396). Check the model at accuraterip.com/driveoffsets.htm. If
  every track fails AR on a good disc, suspect the offset first
- Drives differ a lot on damaged spots: when one stalls or rejects a track,
  try the other drive before anything else
- SuperDrive needs a wake-up command (`sg3_utils`), udev rule
  `/etc/udev/rules.d/91-superdrive.rules`:
  `ACTION=="add", SUBSYSTEM=="block", KERNEL=="sr[0-9]*", ATTRS{idVendor}=="05ac", ATTRS{idProduct}=="1500", RUN+="/usr/bin/sg_raw /dev/%k EA 00 00 00 00 00 01"`.
  Use a USB 3 port on the laptop (it failed to eject on weaker ports).
  Slot-loading: don't push discs in while it's busy; thick printed discs can
  stall on eject
- Linux eject unlocks the tray first (CDROM_LOCKDOOR 0) and retries; the
  reader must be closed before ejecting. /dev/srN is readable without sudo

## Phone

- Pixel 9. USB via adb (`/sdcard/Music`) or Wi-Fi via KDE Connect's sftp
  mount (shared storage `Music`; free space unknown there, estimated from the
  last adb reading). Don't run GSConnect alongside KDE Connect. If adb lists
  only the wireless (`_adb-tls-connect`) device: `adb kill-server`
- Sync = make the phone match the ticked albums. Record
  `Music/.ripshelf-sync.json` (album folder → source + file fingerprints).
  Removals first; every operation is idempotent, so a sleeping phone is
  waited out (reconnect every 5 s, up to 10 min). KDE Connect battery usage
  should be Unrestricted on the phone
- Rip notifications go to the phone through `kdeconnect-cli --ping-msg`

## Docs

- `docs/rust-app-design.md`: original design (2026-11-07)
- `docs/ui-redesign.md`: "Prism" design language and the redesign chunks
- `docs/ctdb-repair.md`: CUETools DB protocol, layout, repair algorithm
- `docs/server-library.md`: the server as the one library (in progress)
- `docs/history.md`: the full project journal up to 2026-11-14

## Open items

- **Server as the library** (`docs/server-library.md`): steps 1-2 done
  (2026-11-14, Server page and catalogue check 2026-11-16). Finished discs
  (and repairs) are queued in the outbox and sent to the Mac with rsync over
  SSH from the home network only; with a music server login Studio waits
  for the scan and checks the catalogue before marking an album sent.
  Settings in localbeat.json `server`, set on the Lenovo laptop. Next: step
  3, the laptop's tunnel and login. Laptop copies are kept until the Mac's
  library has a backup
- Replugging a USB drive while Studio runs adds a second drive card (watcher
  keys drives by /dev path). Fix: key by udev ID_SERIAL, drop vanished paths
- Not yet seen on a real disc: OtherPressing acceptance, per-drive Stop / Stop
  & eject buttons during a rip
- To re-rip: Led Zeppelin box RepeatDiffers tracks (disc 1: 1, 4, 5, 10;
  disc 2: 14; disc 3: 2); Radiohead "I Might Be Wrong" (ripped at a wrong
  -1151); old fre:ac albums without art/MBIDs
