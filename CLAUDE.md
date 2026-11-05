# CD Ripping Pipeline: Project Notes

Goal: a single CLI command that rips a CD to FLAC (accurate, verified), tags it
with MusicBrainz metadata and embedded cover art, converts it to Opus for the
phone, and ejects the disc.

## System

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

## Next steps (planned 2026-11-05)

1. Test a second (shorter) CD with the new Artist/Album layout
2. Desktop progress window for rip-cd.sh (CLI still works), and a loop mode
   that waits for the next disc instead of exiting after eject
3. Later: a replacement for Musicolet on the phone (user has ideas). The phone
   library is already in use in Musicolet; older phone folders still mix
   `Artist - Album` and `Artist/Album` layouts and straight vs. curly `'`
