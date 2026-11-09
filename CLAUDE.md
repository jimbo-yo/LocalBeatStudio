# CD Ripping Pipeline: Project Notes

Goal: a single CLI command that rips a CD to FLAC (accurate, verified), tags it
with MusicBrainz metadata and embedded cover art, converts it to Opus for the
phone, and ejects the disc.

## Machines

The repo is used on two computers. Check `/etc/os-release` to tell which one
you're on; most notes below were written on the Fedora one.

| | Fedora laptop (original) | Ubuntu laptop (added 2026-11-06) |
|---|---|---|
| OS / desktop | Fedora 44, KDE Plasma | Ubuntu 26.04 LTS, GNOME |
| Drives | sr0 built-in GU90N (broken), sr1 Apple SuperDrive (+6) | sr0 built-in GU90N, sr1 HP DVDRAM GTA0N USB (`ID_SERIAL` `hp_DVDRAM_GTA0N_423442525150313439373432-0:0`, offset **+102**: `rip-cd.sh setup` 2026-11-06, 14/14 tracks; confirm with AccurateRip matches on a different disc) |
| Packages | `dnf` | `apt` |

Same layout on both: repo in `~/development/CDRipPipeline`, symlinks in
`~/Applications`, menu entries symlinked into `~/.local/share/applications`,
offsets in `~/.config/rip-cd/offsets` (SuperDrive seeded with 6 under both
serials), cyanrip built from `~/development/cyanrip` (648b1dd) into
`~/.local/bin`. The FLAC/phone libraries are NOT synced between machines:
copy `~/Music/FLAC` and `~/Music/phone` across, or rip-cd.sh re-rips albums
that exist only on the other computer.

Install (equivalents):

- Fedora: `sudo dnf install flac opus-tools ffmpeg-free sg3_utils
  python3-pyside6 android-tools qt6-qttools kde-connect` + cyanrip build deps
  `meson ffmpeg-devel` (negativo17, not ffmpeg-free-devel)
  `libcdio-paranoia-devel libmusicbrainz5-devel libcurl-devel`
- Ubuntu: `sudo apt install flac opus-tools ffmpeg libnotify-bin
  python3-pyside6.qtcore python3-pyside6.qtgui python3-pyside6.qtwidgets
  python3-pyside6.qtnetwork adb kdeconnect qdbus-qt6 sshfs` + cyanrip build
  deps `meson ninja-build pkg-config libcdio-paranoia-dev libmusicbrainz5-dev
  libcurl4-openssl-dev libavcodec-dev libavformat-dev libavutil-dev
  libswresample-dev libavfilter-dev`. Ubuntu packages cyanrip 0.9.3.1, but
  build master anyway: rip-cd.sh's output parsing was tested on 0.9.4-rc3
- SuperDrive only (Fedora laptop; Ubuntu uses the HP drive): `sg3_utils`/
  `sg3-utils` and the udev rule below. Then on both, build cyanrip
  (`meson setup build --prefix=$HOME/.local && ninja -C build install`)

Ubuntu/GNOME differences:
- `desktop/rip-cd-solid.desktop` (disc-inserted popup) is KDE-only; on GNOME
  start rips from the "CD Ripper" menu entry
- Phone Sync's Wi-Fi mode talks to KDE Connect's daemon (`org.kde.kdeconnect`
  over D-Bus), not GSConnect's API, so install `kdeconnect` and don't also
  enable GSConnect (same ports 1714-1764). The phone must be re-paired
- `~/.local/bin` is added to PATH by `~/.profile` only if it existed at login

## System (Fedora laptop; the SuperDrive notes apply on either)

- Lenovo laptop, Fedora, KDE Plasma
- Editor preference: **vi** (use vi, not nano, in instructions)
- Apple USB SuperDrive (A1379), USB ID `05ac:1500`
  - Shows up as **`/dev/sr1`**: `/dev/sr0` is the laptop's built-in DVD drive
    (HL-DT-ST DVDRAM GU90N). rip-cd.sh finds the SuperDrive by USB vendor ID
    (`ID_VENDOR_ID=05ac`), so the sr number doesn't matter
  - Must be on a **USB 3.0 port** directly on the laptop: on a weaker port it
    failed to eject (disc came out partway and got pulled back in)
  - Slot-loading, no eject button: eject with `eject /dev/sr1` or the KDE
    Disks & Devices popup
  - Thick discs (printed labels) can stall in the slot on eject

## SuperDrive wake-up rule (in place and working)

The SuperDrive needs a vendor command before Linux can fully use it (eject etc.).
Needs `sg3_utils` (installed).

`/etc/udev/rules.d/91-superdrive.rules`:

```
ACTION=="add", SUBSYSTEM=="block", KERNEL=="sr[0-9]*", ATTRS{idVendor}=="05ac", ATTRS{idProduct}=="1500", RUN+="/usr/bin/sg_raw /dev/%k EA 00 00 00 00 00 01"
```

An older `90-mac-superdrive.rules` (fired on every USB sub-device and logged
exit-code-52 errors) was removed.

## Folders

- **2026-11-08 layout (decided):** `~/Music/FLAC/` = master copies (ripped
  with the pipeline); `~/Music/Miscellaneous/` = everything else, moved there
  as is (old iTunes library: ~1,700 .m4a, ~1,200 .mp3, 316 DRM .m4p that can't
  be converted, booklets/PDFs, .m4v, plus the 8 old fre:ac `Artist - Album`
  folders); undo list in `Miscellaneous/.moved-from-Music.json`. No stored
  Opus: the phone gets Opus made **at sync time** from FLAC or Miscellaneous
  (lossy sources at min(160 kbps, source bitrate)); `~/Music/phone` goes once
  ripshelf's phone sync replaces Phone Sync. A FLAC re-rip of a Miscellaneous
  album wins everywhere (phone copy replaced); the old copy is only removed if
  the user agrees. Paths below that say `~/Music/Artist - Album` are now under
  `~/Music/Miscellaneous/`.

- Scripts are edited in `~/development/CDRipPipeline`; `~/Applications/rip-cd.sh`
  and `flac2phone.sh` are **symlinks** to them (2026-11-05: the old separate copy
  in ~/Applications was stale, so the Artist/Album layout never took effect)
- FLAC archive: `~/Music/FLAC` (default in rip-cd.sh; adjust as needed)
- Phone copies: `~/Music/phone`
- Older rips in the music library are AAC (.m4a) and should be **copied as-is**,
  not re-encoded

## Scripts

### flac2phone.sh (tested)
`flac2phone.sh SOURCE DEST [opus|aac] [bitrate]`
- Converts every .flac under SOURCE to Opus (opusenc, 160 kbps default) or AAC
  (ffmpeg, 256 kbps default), mirroring album folders
- Keeps tags and embedded cover art; copies cover/folder .jpg/.png
- Skips files already converted; parallel across all cores
- Skips DEST if it's inside SOURCE (fixed a nested-copy bug)
- AAC mode tested end to end; Opus mode not run in testing (opusenc unavailable
  there) but straightforward. Needs `opus-tools` installed.

### rip-cd.sh (working: first full rip 2026-11-05)
- Runs **whipper** natively: it IS in Fedora 44's repos (`whipper 0.10.0`,
  also `flac`, `abcde`), so install with `sudo dnf install whipper flac`.
  Falls back to the container image (`docker.io/whipperteam/whipper`) via
  rootless podman only if whipper isn't installed
- `DEVICE=/dev/srN rip-cd.sh` overrides the SuperDrive auto-detection
- `rip-cd.sh setup`: `whipper drive analyze` + `whipper offset find`
  (config persisted in `~/.config/whipper`). **Done 2026-11-04:** the
  SuperDrive's internal mechanism reports as HL-DT-ST DVDRW GX50N (RR06);
  cdparanoia defeats its cache; **read offset +6** (AccurateRip-confirmed)
- whipper's full TOC read (`cdrdao read-toc`, scans Q subchannel for
  pregaps/indexes/ISRC) took ~10 min on the setup disc, which had Q-subchannel
  CRC errors on every track (1150 on track 16). `--fast-toc` took 1m37s, so the
  scan is ~8.5 min of it. Decided to keep the full scan for archive accuracy;
  could add an opt-in fast mode for bad discs. Likely explains some "stalls"
- `rip-cd.sh`: rips with `--cover-art embed` to FLAC_DIR, runs flac2phone.sh
  into PHONE_DIR, then ejects
- `rip-cd.sh --unknown`: for discs MusicBrainz doesn't recognize
- Layout: `FLAC_DIR/Artist/Album/NN. Artist - Title.flac` (matches the existing
  phone library and Musicolet use) via
  `--track-template`/`--disc-template` (whipper's default adds an `album/` level)
- `--cover-art complete` (embed + cover.jpg). First rip got no cover (whipper
  silently skips on a Cover Art Archive error), so the script now re-fetches
  from coverartarchive.org using the MBID in the .log and embeds with metaflac
- First rip: Use Your Illusion I (worn disc, glitch heard on track 10): all 16
  tracks AccurateRip exact match, 100% quality. ~58 min total (~12 min TOC,
  ~45 min ripping at 2.1x-4.7x); CPU near idle, so the drive is the bottleneck.
  Plan: buy a second (modern external USB) drive and rip two discs in
  parallel; the internal sr0 drive is broken. rip-cd.sh needs a lock around
  flac2phone before running two at once
- Added 2026-11-05 (not yet tested on a full rip):
  - `--country US` by default (`COUNTRY=GB` to change, `COUNTRY=` for any;
    skipped if `-R`/`--unknown` given); if no US edition exists, retries
    without the filter. `-R <mbid>` passes through to pick a release
  - `--keep-going`: an unreadable track is skipped (whipper exits 5); the
    script still makes phone copies and ejects, then names missing tracks
  - TOC read failure is retried once. whipper ignores cdrdao's exit code and
    hides its stderr (the 2026-11-05 failure showed only a confusing
    FileNotFoundError on the .toc temp file), so a PATH wrapper copies
    cdrdao's stderr to `~/.cache/rip-cd/cdrdao.log`. "Unit not ready" there
    means no disc in the drive
  - whole script is inside `main()`, so editing it mid-rip is safe
- Second CD (Lenny Kravitz, Let Love Rule, US): tracks 1-6 verified; track 7
  failed all 5 tries (checksums differ each time, no kernel/USB errors, so the
  disc). Partial rip deleted; clean and re-rip
- Watch a rip: `rip-cd.sh 2>&1 | tee ~/rip.log` and ask Claude to read the log
- Likely first-run issues (container fallback only): user/home path
  (`/home/worker`), `--userns=keep-id`, SELinux `:z`, `/dev/sg*` passthrough

## Open issues

1. **Stalls mid-rip** (seen in fre:ac, at least 10+ minutes frozen). Unknown
   whether it's a damaged disc (paranoia retrying), the SuperDrive losing
   power/USB connection, or software. Diagnose by watching
   `sudo journalctl -k -f` during a rip (look for USB resets/disconnects vs.
   `sr1` I/O / medium errors vs. nothing).
2. One disc: **track 1 failed AccurateRip** and the file stopped halfway
   through, a real read problem. Other tracks on that disc verified, so the
   drive offset is fine. Try cleaning and re-ripping that track.
   Probably *Groove* (sdtk), fre:ac rip 2026-11-04 in
   `~/Music/Soundtrack - Groove (sdtk)/`: track 1 FLAC is 8.7 KB/no audio
   (its Opus is 0.007 s). The 2019 AAC rip `~/Music/Compilations/Groove/` has a
   good track 1 (46 s). Plan: re-rip the CD with rip-cd.sh

## Background / history

- fre:ac 1.1.7 AppImage at `~/Applications/freac.AppImage` (Flatpak removed;
  auto-read/auto-rip options only worked in the AppImage). Unreliable, which
  is why we're moving to a CLI workflow.
- KDE: added a "Rip with fre:ac" device action and overrode
  `solid_audiocd.desktop` in `~/.local/share/solid/actions/` to hide
  "open audio CD in file manager". KDE never auto-runs device actions.
- MusicBrainz Picard is installed for tagging/cover art; its log showed
  MusicBrainz 503s and Internet Archive 500s (server-side, temporary).
- Format plan: FLAC (level 5) archive on the computer, Opus 160 kbps on the
  Android phone (Android 17; music player e.g. Musicolet/Poweramp).

## cyanrip (candidate whipper replacement, 2026-11-05)

- whipper 0.10.0 (2021) is aging: depends on deprecated `pkg_resources`, no
  resume (re-rips and deletes existing tracks: its saved-checksum check always
  fails on a new run), no single-track ripping, hides cdrdao errors
- cyanrip: not in Fedora repos/Flathub; built from git master (0.9.4-rc3,
  648b1dd) in `~/development/cyanrip`, installed to `~/.local/bin/cyanrip`
  (`meson setup build --prefix=$HOME/.local && ninja -C build install`).
  Build deps: meson ffmpeg-devel (negativo17, not ffmpeg-free-devel)
  libcdio-paranoia-devel libmusicbrainz5-devel libcurl-devel. Self-tests pass
- Useful options: `-l 7` (single track), `-Z N` (repeat rips until checksums
  match, for damaged discs), `-s` offset / `-f` find offset, `-o flac,opus`,
  `-D`/`-F` naming schemes, `-R` release, `-Q` eject
- cyanrip behavior notes: stops with "Multiple releases found" (lists
  `N (ID: …): Album (US) (date)`) unless `-R` given; exits 1 on ANY error but
  still writes every track (unreadable spots become silence), so judge each
  track from its output ("Track N ripped and encoded with errors",
  "Accurip v1/v2: … (accurately ripped"); `-Z 1` = two identical reads;
  `-I` lists each track's output path; cover is saved as `Front.jpg`; in rip
  mode there's no "Track N info:" header (block follows "Track N ripped…")
- **Branch `cyanrip`: rip-cd.sh rewritten for cyanrip** (master keeps the
  whipper version). Info pass picks the US release and the tracks missing from
  FLAC_DIR; rips only those into `~/.cache/rip-cd/staging`; moves a track into
  the library if AccurateRip-verified or once two reads give the same
  checksum; gives up after READS (4) reads in all (was: AR_TRIES re-reads
  then `-Z 1`, up to ~15 reads; Zeppelin disc 1 spent ~1.5 h on 2 tracks that
  read differently every time). cyanrip's `-r` can't be lowered for this: it
  also sets per-frame retries;
  failures go to `~/.cache/rip-cd/rejected`; never overwrites. Offsets per
  drive (udev ID_SERIAL) in `~/.config/rip-cd/offsets`; SuperDrive seeded with
  6; `rip-cd.sh setup` runs `cyanrip -f` and saves it. Drive auto-picked as
  the one with an audio CD (no longer SuperDrive-only)
- Tested on cyanrip's disc images (full rip, two-read fallback, resume of one
  missing track, nothing-to-do). NOT yet tested on a real disc: AccurateRip
  path, release selection, rejects, setup
- First real rip 2026-11-05: Kink Kronikles disc 1 (2-CD set), 14/14
  AccurateRip exact on the first read, 0 errors, ~31 min on the SuperDrive.
  Picked regular US release over the BMG club one. Fixed: multi-disc CUE
  sheets/logs now get " CDn" so disc 2 can't overwrite disc 1. cyanrip swaps
  `?` for a lookalike `？` in file names (phone-safe; `-T os_unicode` keeps it)
- 2026-11-05 additions (cyanrip branch):
  - A track whose clean re-read has the same EAC CRC32 as an earlier read is
    accepted then ("repeat", same test as -Z 1) instead of after all AR_TRIES.
    Seen on Led Zeppelin box disc 1: tracks 5/10 read identically every time
    (no AR match, AR 450-frame CRC matched), 4/11/15 differed per read
  - Per-drive lock (`~/.cache/rip-cd/lock-srN`, flock) and a flac2phone lock,
    so two drives can rip at once; --loop and auto-pick skip busy drives
  - `RIPCD_EVENTS=1` prints `@@event<TAB>fields` lines (waiting, inserted,
    drive, stage, release, disc, track, cover, stagedir, pass, result,
    accuraterip, accepted, rejected, finished, error, discend, remove)
  - Replace rip-cd.sh with `mv` (new inode), never in-place edits, while a
    rip runs: bash re-reads the file by offset after main() returns
- 2026-11-06: the SuperDrive's udev ID_SERIAL changed from
  `HL-DT-ST_DVDRW_GX50N_KZ4F9KI4627-0:0` to `Apple_SuperDrive_KZ4F9KI4627-0:0`;
  the offset lookup missed, `rip-cd.sh setup` was run on Zeppelin disc 2 and
  cyanrip -f found a bogus **+396**, so every track failed AccurateRip (AR
  450-frame CRCs matched = right audio, wrong alignment). Fixed: offset reset
  to 6; lookup falls back to ID_SERIAL_SHORT; setup asks before replacing a
  different saved offset; offset shown in output/GUI. **SuperDrive offset is
  +6**: if all tracks fail AR on a good disc, suspect the offset first.
  Then re-ripped at +6: still no AR matches and the AR 450-frame check stopped
  matching, so disc 2 is a **different pressing** shifted ~390 samples from
  the one in AccurateRip (that's what fooled setup). Rip at +6 anyway (true
  drive offset); tracks verify by two identical reads. The GUI now says so
  after 3 first-pass misses with no hits
- 2026-11-06 new drive **hp DVDRAM GTA0N** (LG-made slim USB; udev
  `hp_DVDRAM_GTA0N_423442525150313439373432-0:0`): **offset +102** (AccurateRip
  drive list: all GTA0N variants +102, 56 hp submissions; confirmed: Bosstones
  "Let's Face It" track 1 AR match conf 200). `rip-cd.sh setup` had found
  **-1151** on Radiohead "I Might Be Wrong" (an odd pressing: tracks 1-7 matched
  AR at -1151), so the Bosstones disc matched nothing. Lesson: offset finding
  from one disc can be fooled; check the model in accuraterip.com/driveoffsets.htm.
  The Radiohead rip (at -1151) should be re-ripped at +102
- **rip-cd-gui.py** (PySide6, Fedora python3-pyside6): desktop window that
  runs `rip-cd.sh --loop` with RIPCD_EVENTS=1 in its own session (Stop kills
  cyanrip too). Shows cover, artist/album/disc N of M, per-track status
  (live AccurateRip result per read), current-track and pass progress with
  ETA, session list, log pane; KDE notification per disc; saves output to
  `~/rip.log`; single instance (QLocalServer). `--no-start`; other args go to
  rip-cd.sh; `RIPCD_SCRIPT=` overrides the script (testing). Launchers
  (symlinks into the repo's `desktop/`): app menu "CD Ripper", and a KDE
  disc-inserted popup action "Rip with CD Ripper". Tested offscreen with
  synthetic streams; NOT yet used on a real rip
- Plan: re-rip Let Love Rule on the cyanrip branch: should find 12 tracks
  present and rip only track 7 (on the new drive, after `rip-cd.sh setup`);
  then compare a cyanrip rip of a verified track with whipper's file

### phone-sync.py (2026-11-06; tested offscreen with a fake phone folder only)
- PySide6 app "Phone Sync": albums in `~/Music/phone` with checkboxes (per
  artist/album, search), capacity bar (other apps / music / adding /
  removing / reserve), Sync = make the phone's music folder match the checks
  (copies missing/changed files, removes unchecked albums after a confirm
  list; removes first). Only touches the phone music folder
- Connections, switchable in the window: USB `adb` (needs `sudo dnf install
  android-tools` + USB debugging; default folder `/sdcard/Music`) and Wi-Fi
  KDE Connect (sftp mount via `qdbus-qt6 org.kde.kdeconnect
  /modules/kdeconnect/devices/<id>/sftp mountAndWait/mountPoint/
  getDirectories`; folder `Music` in shared storage; free space via statvfs
  on the sshfs mount does NOT work: sshfs shows a fake
  1000 GiB all-free, detected and treated as unknown; then estimates from the
  last USB/adb numbers (`last_space` in config) or a "music limit" setting).
  Verified 2026-11-06 with the Pixel 9 (read-only): mount at
  `/run/user/1000/<id>/storage/emulated/0`, listing ~2 s; phone needed KDE
  Connect's "filesystem access" permission. Phone had 3 empty album folders
  (Meat Loaf x2, Groove) and was missing Mama Said track 03. Fedora's FedoraWorkstation zone already
  allows KDE Connect's ports
- Albums matched by normalised folder name (case, curly/straight quotes,
  `Artist/Album` == `Artist - Album`), so old-style phone folders count as
  present and get missing files added in place
- Settings/remembered choices: `~/.config/rip-cd/phone-sync.json` (reserve
  GB default 5). Test: `PHONE_SYNC_TEST_DIR=dir PHONE_SYNC_TEST_SIZE=bytes`
- Menu entry "Phone Sync" (desktop/phone-sync.desktop symlinked)
- Sync waits out a sleeping phone: on a copy/remove error it shows "Wake it
  up to continue", reconnects every 5 s and retries, up to WAIT_FOR_PHONE
  (600 s). Can't keep the phone awake over KDE Connect; the fix on the phone
  is KDE Connect battery usage = Unrestricted. First real sync 2026-11-06
  (Meat Loaf x2, OK Computer, Mama Said track) worked
- Re-rips: a local `Artist/Album` copy wins over an old `Artist - Album` one;
  if the phone copy has audio files the local copy lacks (new file names), the
  phone album is replaced (old folder removed, new pushed to the new name)
- 2026-11-06 phone library review: old fre:ac rips (Appetite, UYI II 13/14,
  Hole x2, Bat Out of Hell, BOOH II 6/11, Mama Said 4/16, Groove w/ broken
  track 1) have no art/MBIDs: to be re-ripped. whipper rips (Let Love Rule,
  OK Computer) lacked embedded art: embedded cover.jpg with metaflac, Opus
  regenerated. After re-rips, delete the old `~/Music/Artist - Album` FLAC
  folders and `~/Music/phone/Artist - Album` copies

### mb-enrich.py (2026-11-06)
- cyanrip writes no GENRE and no original release date (DATE is that CD's
  release: Kink Kronikles says 1989, the album is 1972). LocalBeat's Genres and
  Decades views need both
- `mb-enrich.py [--force] [--dry-run] [PATH...]` (default `$FLAC_DIR`): for
  FLACs with MUSICBRAINZ_ALBUMID, sets ORIGINALDATE/ORIGINALYEAR from the
  release group's first-release-date and up to 3 GENRE values (release-group
  genres by votes, Title Case; else the artist's). Skips files that already
  have ORIGINALDATE; skips files without an MBID (old fre:ac rips). 1 req/s,
  retries 503s. Tags only (audio MD5 and pictures unchanged; verified)
- metaflac bumps the FLAC mtime, so flac2phone.sh re-makes those Opus copies
  and Phone Sync replaces them on the phone
- rip-cd.sh runs it on the album right before flac2phone (`ev stage tagging`;
  failure is non-fatal)
- Backfilled 2026-11-06: all 7 cyanrip/whipper albums (100 FLACs), 88 Opus
  copies regenerated. Note the "Led Zeppelin" rip is the 1990 box set, so 1990
  is its right original date

### fetch-cover.py (2026-11-06)
- For album folders with no front cover (old fre:ac rips): searches
  MusicBrainz by ALBUMARTIST/ALBUM (drops bracketed junk like "(sdtk)"; tries
  " - " as ": "; albums ranked above singles; artist "Soundtrack" searches
  soundtracks), downloads the Cover Art Archive release-group front (1200 px),
  saves cover.jpg and embeds it (type 3) in FLACs lacking one.
  `--preview DIR` only downloads, to check first; `--release-group MBID`
  overrides the search
- Text search failed for Groove (sdtk); its CDTOC tag gave an exact MusicBrainz
  TOC lookup (`/ws/2/discid/-?toc=1+N+leadout+offsets`, CDTOC values +150):
  release group 352498a1-4906-395d-9c89-61fc4613e11b
- Done 2026-11-06 for all 8 old albums in `~/Music/Artist - Album/` (covers
  checked by eye), then `flac2phone.sh "~/Music/X" "~/Music/phone/X"` per album

## Rust app "ripshelf" (branch `rust-app`, started 2026-11-07)

- One app to replace all the scripts; Windows + Linux. Design and milestones:
  `docs/rust-app-design.md`. Developed on a Windows laptop (third machine,
  Rust 1.99 MSVC; no CMake yet, so no libFLAC/libopus crates yet)
- Cargo workspace `app/`: `core` (no OS code), `platform` (drive/phone traits,
  empty so far), `ripshelf` (CLI now, GUI later)
- Done: AccurateRip v1/v2 checksums (unit-tested against the reference
  algorithm), AR database fetch via `cdtoc`, TOC from CDTOC tag or
  MusicBrainz disc ID, FLAC decode (claxon), Windows-safe naming
- `ripshelf check DIR...`: decodes an album's FLACs and checks them against
  AccurateRip. NOT yet run on real rips (none on the Windows laptop): run it on
  a cyanrip album that matched AR (e.g. Kink Kronikles disc 1) to validate the
  checksum code
- Online test: `cargo test -p ripshelf-core -- --ignored`
- cd-da-reader 1.0 review (2026-11-08): reads TOC + any sector range (READ
  CD), set speed; Windows SPTI / Linux SG_IO / macOS. No eject, drive
  identity, C2 pointers, subchannel (ISRC/pregaps), CD-TEXT. A failed sector
  fails the whole range read (no silent zero-fill: good). Usable without a
  fork; we add eject + identity (platform/src/windows.rs, linux.rs) and
  chunking/offset/cache-flush (core/src/secure_read.rs, tested on a fake disc)
- **First real-drive test 2026-11-08 (Windows, HP GTA0N on D:)**: `drives`
  reports key `hp_DVDRAM_GTA0N_423442525150313439373432` = udev ID_SERIAL
  minus `-0:0`, so one offsets entry can serve both OSes. Track 1 (16:20) of
  an 8-track disc read twice at +102: both AccurateRip v2 match (conf 15),
  same CRC, ~5.3x (debug build), ~3 min each. Confirms AR checksum code,
  offset +102 and its sign
- Drive test commands: `ripshelf drives`,
  `toc`, `read-track N --offset 102 --reads 2 [--speed X] [--flush 1000]
  [--wav f.wav]`, `eject`. read-track prints CRC + AR v1/v2 per read and
  "VERIFIED" on an AR match or two equal CRCs. Things to learn on the HP
  drive: does a re-read after --flush give a different CRC on a bad spot (cache
  really defeated: yes, see rip --loop below), real read speed, whether +102
  gives AR matches (yes)
- FLAC encoding (core/src/flac_encode.rs): reference libFLAC via
  `libflac-sys` (built by CMake: `winget install Kitware.CMake`; a new shell
  may need `C:\Program Files\CMake\bin` on PATH), level 5, **verify mode**,
  Vorbis comments + front cover + 8 KiB padding, written through Rust File
  (Unicode paths OK). Workspace Cargo.toml builds libflac-sys/claxon
  optimized in dev (else CMake links the debug CRT: LNK4098)
- MusicBrainz (core/src/musicbrainz.rs): discid lookup with `toc=` so
  unknown disc IDs still get similar-TOC ("fuzzy") candidates; ranking =
  exact disc ID, country (US default), Official, not club edition, CD,
  earliest date; genres/original date like mb-enrich.py; Picard tag names
  (ALBUMARTIST, TRACKTOTAL, DISCTOTAL, RELEASETYPE, MUSICBRAINZ_* ...) plus
  MUSICBRAINZ_DISCID and CDTOC. 1 req/s, 503 retries (core/src/http.rs).
  `ripshelf identify [--drive D:] [--cdtoc X] [--country US]`. Test disc
  in drive 2026-11-08: Gershwin "Greatest Hits" (RCA Victor 60834-2-RG, US
  1991); fixture core/tests/data/discid-rQRpBjAQ.json. Still to do: compare
  with the tags cyanrip writes (need a cyanrip FLAC here)
- `ripshelf rip` (core/src/rip.rs, ripshelf/src/rip_cmd.rs): offset from the
  offsets file (`ripshelf offset [set N]`; Windows file
  `%APPDATA%\rip-cd\offsets`, HP drive saved as +102 on 2026-11-08; lookup
  ignores udev's `-0:0` so Linux lines match), MusicBrainz (refuses a
  fuzzy-only match: use -R ID or --unknown), genres, CAA front-1200 cover
  (release, else release group), AccurateRip, then per missing track up to 4
  reads with cache flush; accept on AR match or a clean read repeating an
  earlier clean CRC (reads with unreadable sectors never count); encode to
  `<name>.part` then rename; never overwrite; rejects (last read) to
  `<cache>\rip-cd\rejected\<Artist - Album>\`; cover.jpg; appends
  `<Album>[ CDn].log`; ejects unless --no-eject. Multi-disc: `2.07. Artist -
  Title` in one folder (as rip-cd.sh). First full rip 2026-11-08: Gershwin
  8/8 AR on first read, 9 min (release build, ~7.7x); `check` re-verified
  the FLACs. Not yet: CUE sheet
- `rip --loop` (rip_cmd.rs rip_loop): polls drives every 2 s (all, or
  --drive), rips each new disc, ejects, carries on after a failed disc
  (ejects it); remembers a finished disc per drive until it's removed. One
  drive at a time per process; parallel = a second `rip --loop --drive X`.
  Locks (core/src/lock.rs, std File::try_lock): `<cache>/lock-D` per drive
  (held while probing and ripping; busy drives aren't even opened) and
  `lock-phone` around phone conversion. --release refused with --loop.
  Drive paths listed without opening (platform drive_paths: GetDriveTypeW /
  /dev/sr*). Tested 2026-11-08: Gershwin (already present: skipped,
  ejected), then OK Computer 12/12 AR conf 200. **Cache flush confirmed**:
  track 2 read 1 and 2 differed (no AR match), read 3 matched AR, so re-reads
  really hit the disc
- Phone copies (core/src/phone_copy.rs; `ripshelf phone [SRC] [DEST]
  [--bitrate 160]`, and `rip` runs it on the album after ejecting):
  flac2phone.sh's rules (only new/changed, .part + rename, all cores, copy
  cover/folder images, drop REPLAYGAIN_*/R128_* tags) with libopusenc via
  raw `libopusenc-static-sys` (the `libopusenc` wrapper only takes 48/24/..
  kHz input); cover from the FLAC PICTURE block (parsed in flacfile.rs),
  else cover.jpg. Link quirk: opus-static-sys builds libopus with LTCG, whose
  symbols get lost when bundled into the rlib on MSVC; core/build.rs links
  opus.lib directly. Gershwin album: 8 files in 51 s, ~161 kbps. AAC mode
  not ported (Opus only)
- Crates now: `core`, `platform`, `engine` (the rip session: rip_once,
  rip_loop, rip_disc; owned `Event`s; `Frontend` trait answers
  choose_release + cancelled; Stop works by a SectorSource wrapper that fails
  reads once cancelled), `ripshelf` (CLI = text Frontend: auto-picks the best
  exact release, refuses fuzzy/unknown), `gui` (`ripshelf-gui.exe`, eframe/
  egui **0.37**: App has `fn ui(&mut self, ui, frame)` and panels are
  `egui::Panel::top/bottom/left(..).show(ui, ..)`, `show_collapsible`,
  `exact_size`; older TopBottomPanel/SidePanel/update() are gone)
- GUI (gui/src/main.rs): starts the loop on launch; Start/Stop; stage +
  overall progress/ETA; left: cover, album, release, drive/offset, session
  list; centre: tracks with status, per-read CRC/AR lines, progress bars;
  log panel toggle; release dialog when several match or none/fuzzy (best
  exact one auto-chosen after 30 s; "Rip as unknown", "Skip disc"). No
  console window in release builds. Not yet: notifications, single instance,
  settings UI, phone sync tab
- Offset sign: +N = the track's audio starts N frames later in what the drive
  returns (same convention as EAC/AccurateRip/cyanrip -s)
- **Linux build (Fedora, 2026-11-08)**: `sudo dnf install rust cargo rustfmt
  clippy` (1.99.1, same as Windows) + cmake/gcc. Two Fedora-only build fixes,
  Linux-only via `app/.cargo/config.toml` [env] target-suffixed names (Windows
  never reads them): `CMAKE_x86_64_unknown_linux_gnu` -> `app/tools/
  cmake-libdir-lib` wrapper adding `-DCMAKE_INSTALL_LIBDIR=lib` (opus-static-sys
  hardcodes lib/libopus.a; Fedora's GNUInstallDirs uses lib64 -> build-script
  panic) and `-DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF` (GCC IPO =
  -fno-fat-lto-objects -> undefined opus_*); `CFLAGS_x86_64_unknown_linux_gnu
  = -ffat-lto-objects` (libopusenc's -flto -> undefined ope_*). After changing
  the wrapper, delete target/*/build/opus-static-sys-* (CMake cache persists;
  cargo doesn't track the script). Ubuntu (Debian: lib/) needs neither but
  they're harmless. All 31 tests pass on Linux
- Linux drive identity (platform/src/linux.rs): sysfs vendor/model/rev; key =
  udev ID_SERIAL read from `/run/udev/data/b<maj>:<min>` (no libudev), serial
  = ID_SERIAL_SHORT. `DriveIdentity.os_id` holds it and `key()` uses it as is
  (rebuilding from parts fails: SATA ID_MODEL includes the vendor). So
  rip-cd.sh's offsets file works unchanged: `ripshelf offset --drive /dev/sr1`
  -> +102. /dev/srN readable without sudo (logind uaccess ACL)
- **First Linux rip 2026-11-08** (debug build, HP on /dev/sr1, into a scratch
  FLAC_DIR/PHONE_DIR): OK Computer 12/12 AR conf 200 in 9 min (track 2 matched
  on read 2), tags incl. genres/ORIGINALDATE/ISRC, cover, log, 12 Opus copies.
  Audio MD5 of all 12 FLACs **identical** to the 2026-11 whipper/SuperDrive rip
- Linux eject: CDROMEJECT returned **2** (SCSI check condition, drive refused)
  instead of failing, and the tray stayed shut; the old code only treated
  r < 0 as failure, so it said "Ejected". The tray had been left locked
  against removal. Fix (linux.rs eject): CDROM_LOCKDOOR(0) first, success only
  on 0, up to 5 tries 1 s apart. Also `Drive::eject(self)` now closes the
  reader before ejecting (the kernel locks the tray while the device is open)
- Phone notifications (2026-11-08): platform/src/phone_notify.rs runs
  `kdeconnect-cli -d <id> --ping-msg` (Phone Sync's `kdeconnect_device` if
  reachable, else first reachable; Windows: `%ProgramFiles%\KDE Connect\bin`).
  engine/src/notify.rs `Notifier`: observes Events; after Waiting{done>0} for
  `notify_after_secs` (60) sends one summary ("OK Computer: all 12 tracks in
  the library (8:12). Ready for the next disc."); rejected tracks / DiscFailed
  sent at once; nothing if the next disc starts in time. GUI: "Notify phone"
  checkbox (top bar), saved in `<config>/rip-cd/ripshelf.json` (core
  `config::Prefs`, keeps unknown keys); results in the log. CLI: `rip --loop
  --notify-phone`; `ripshelf notify [MSG]` sends a test (worked to the Pixel 9).
  KDE Connect goes over the LAN (not Bluetooth), home Wi-Fi only by design
- GUI on Linux: wgpu's default Vulkan backend **garbled all text** on the
  Fedora laptop (Intel Haswell; Mesa: "Haswell Vulkan support is incomplete";
  same reason COSMIC looked broken there). gui main() sets WGPU_BACKEND=gl on
  Linux unless already set: renders correctly. Release build takes ~10 min

## Next steps (planned 2026-11-05)

1. Test a second (shorter) CD with the new Artist/Album layout
2. ~~Desktop progress window and loop mode~~ done (rip-cd-gui.py, --loop);
   try the window on a real disc
3. Musicolet replacement: **LocalBeat**, separate repo `~/development/LocalBeat`
   (started 2026-11-06; its CLAUDE.md records the folder/tag contract with
   this pipeline: change one, update the other). Musicolet garbles some UTF-8
   titles by guessing encodings (e.g. "Sittin’ on Top of the World"). The phone
   library is already in use in Musicolet; older phone folders still mix
   `Artist - Album` and `Artist/Album` layouts and straight vs. curly `'`
