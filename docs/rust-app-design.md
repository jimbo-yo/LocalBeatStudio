# Rust app: design (draft 2026-11-07)

One desktop app that replaces rip-cd.sh, rip-cd-gui.py, flac2phone.sh,
mb-enrich.py, fetch-cover.py and phone-sync.py. Targets Windows and Linux
(Fedora/KDE, Ubuntu/GNOME) from one codebase. Developed first on Windows.

Working name: **ripshelf** (placeholder; rename freely).

## Goals

- Insert a disc → verified FLAC in the library, tagged (MusicBrainz, original
  date, genres), cover embedded, Opus phone copy made, disc ejected. No
  terminal.
- Same acceptance rules as the cyanrip branch of rip-cd.sh: keep a track only if
  it matches AccurateRip or two clean reads give the same checksum; give up after
  READS (4) reads; never overwrite; failures go to a rejected folder.
- Resume: rip only the tracks missing from the library.
- Two drives at once.
- Phone tab: the Phone Sync app (checkboxes, capacity bar, sync = make the phone
  match).
- Same library layout and tags as now, so the existing library, Phone Sync's
  matching and LocalBeat keep working.

Not goals (for now): syncing the FLAC library between computers, a Windows
installer, editing tags by hand.

## Layout

Cargo workspace in `app/` inside this repo (the scripts stay until replaced):

```
app/
  Cargo.toml            workspace
  core/                 library crate, no GUI, no OS-specific code
    toc.rs              TOC, MusicBrainz disc ID, AccurateRip ID (cdtoc crate)
    accuraterip.rs      v1/v2 checksums from samples, DB fetch + match
    verify.rs           read/accept/reject rules (READS, repeat-match)
    musicbrainz.rs      release lookup + choice (US first, then any), genres,
                        original date (from mb-enrich.py)
    cover.rs            Cover Art Archive, search fallback (from fetch-cover.py)
    encode.rs           FLAC (libFLAC) and Opus (libopusenc) writers
    tags.rs             Vorbis comments + pictures (lofty)
    naming.rs           Artist/Album/NN. Artist - Title, safe characters
    library.rs          scan FLAC/phone folders, which tracks are missing
    phone_plan.rs       sync plan: what to copy/remove (from phone-sync.py)
    events.rs           progress events (same set as RIPCD_EVENTS)
  platform/             OS-specific, behind traits
    drive.rs            trait CdDrive: list, toc, read_sectors, eject, identity
    drive_windows.rs    via cd-da-reader (SCSI pass-through)
    drive_linux.rs      via cd-da-reader (SG_IO); later libcdio-paranoia
    hotplug.rs          disc inserted/removed: udev (Linux), device-change
                        notifications (Windows)
    phone.rs            trait PhoneTransport: list, free space, push, delete
    phone_adb.rs        adb (USB, or adb over Wi-Fi), both OSes
    phone_kdeconnect.rs Linux only: zbus → KDE Connect sftp mount
  gui/                  binary crate (egui/eframe)
    rip view            cover, album, per-track status + AR results, ETA, log
    library/phone view  Phone Sync's checkbox tree + capacity bar
```

Rule: `core` never calls the OS directly, so it builds and is tested the same
everywhere. Tests use saved TOCs, AccurateRip .bin files and short PCM samples
as fixtures.

## Ripping pipeline

1. **Detect** a drive with an audio disc (hotplug event or a manual Rip
   button). One worker per drive, with a lock per drive.
2. **Identify**: read the TOC → MusicBrainz disc ID → releases. Pick as now: US
   release preferred, then any; on several matches, ask in the GUI instead of
   stopping. Fetch AccurateRip data for the disc.
3. **Plan**: compare against the library and list the missing tracks. Nothing
   missing → eject.
4. **Read** each missing track into staging (in memory or
   `<cache>/staging`):
   - Apply the drive's read offset (per drive in the offsets file; HP GTA0N
     +102, SuperDrive +6). A positive offset needs 102 samples from the next
     track; the last track's overread into the lead-out may fail → zero-fill
     (AccurateRip skips the last 5 frames anyway).
   - Compute AR v1, AR v2 and the CRC32 of the offset-corrected audio.
   - Accept if it matches AccurateRip. Otherwise re-read; accept as soon as
     two reads have the same CRC; reject after READS reads.
   - **Cache defeat:** before a re-read, read a distant part of the disc (larger
     than the drive cache) and/or use the FUA flag, so a re-read actually hits
     the disc. Needs testing per drive; without it, two matching reads mean
     nothing.
5. **Encode + tag**: FLAC (level 5) with the tags cyanrip writes today plus
   ORIGINALDATE/ORIGINALYEAR/GENRE, embedded front cover, cover.jpg, log and
   CUE (one per disc, " CDn" for multi-disc).
6. **Move** accepted tracks into the library (never overwrite); rejected ones
   go to `<cache>/rejected`.
7. **Phone copy**: Opus 160 kbps from the new FLACs (in-process, parallel).
8. **Eject**; notify; wait for the next disc.

Phase 1 reads with `cd-da-reader` (no paranoia error correction; our
AR/repeat rules do the checking). Phase 2 adds libcdio-paranoia bindings
behind the same `CdDrive` trait, if bad discs need it. To validate: rip a disc
already in the library and compare audio checksums with the cyanrip files.

## Files and settings

| | Windows | Linux |
|---|---|---|
| FLAC library | `%USERPROFILE%\Music\FLAC` | `~/Music/FLAC` |
| Phone copies | `%USERPROFILE%\Music\phone` | `~/Music/phone` |
| Settings, offsets | `%APPDATA%\rip-cd\` | `~/.config/rip-cd/` (existing files read as is) |
| Staging, rejected, logs | `%LOCALAPPDATA%\rip-cd\` | `~/.cache/rip-cd/` |

All of these paths are settings. Drive identity differs per OS (Windows
device instance ID vs. udev ID_SERIAL), so each OS keeps its own offset
entries; the app shows the offset and warns when a drive has none (link to
accuraterip.com/driveoffsets.htm, as learned from the -1151/+396 mix-ups).

## File names (needs a decision)

Windows forbids `< > : " / \ | ? *` and trailing dots/spaces in names; Linux
allows all but `/`. cyanrip already swaps `?` for `？`. Options:

- **Same names on both OSes** (recommended): always use the Windows-safe set,
  with lookalikes (`：`, `＂`, `＊`, etc., the way cyanrip does `？`). Existing
  Linux albums with a plain `:` in a name would then look "missing" unless
  library matching normalises the names (Phone Sync's matching already does
  this for quotes and case).
- Per-OS names: simpler, but a library copied between machines would get
  renamed.

Check LocalBeat's CLAUDE.md (folder/tag contract) before settling this.

## Crates

| Job | Crate |
|---|---|
| CD reading | `cd-da-reader` (phase 1), libcdio-paranoia FFI (phase 2) |
| TOC / disc IDs / AR database | `cdtoc` (features: accuraterip, musicbrainz) |
| MusicBrainz | `musicbrainz_rs` or plain `reqwest` + `serde` |
| FLAC encode | libFLAC bindings (`libflac-rs` or `libflac-sys`); not `flacenc` (it can occasionally output broken audio) |
| FLAC decode (for Opus) | `claxon` or `symphonia` |
| Opus | `libopusenc` (young; fallback: run `opusenc.exe`/`opusenc`) |
| Tags | `lofty` |
| GUI | `egui`/`eframe` |
| Async/workers | `tokio` or plain threads + channels |
| Hotplug | `udev` (Linux), `windows` crate (Windows) |
| adb | `adb_client`, or run `adb` |
| KDE Connect | `zbus` (Linux only) |
| Paths | `directories` |
| Notifications | `notify-rust` |
| Single instance | `single-instance` |

## Milestones

1. **Skeleton + core logic, no drive**: workspace, naming, tags, AccurateRip
   checksum (tested against known values), MusicBrainz lookup from a TOC,
   FLAC → Opus conversion. Testable with a few albums copied from the Fedora
   library.
2. **Drive on Windows**: list drives, read TOC, read a track, offset, AR match.
   CLI first (`ripshelf rip --drive D:`), output checked against the existing
   rips.
3. **Full rip pipeline** (still CLI): plan/resume, re-reads, staging/rejected,
   encode, tag, cover, eject.
4. **GUI**: rip view, then library/phone view.
5. **Phone sync**: adb on both OSes, KDE Connect on Linux.
6. **Linux build** on the Fedora and Ubuntu laptops: udev hotplug, drive
   identity, existing config files. Then retire the scripts.
7. Optional: libcdio-paranoia engine, two drives at once in the GUI.

## Open questions

- App name.
- File-name policy (above).
- Exact tag set cyanrip writes today (read from an existing FLAC) so the new
  rips match.
- Does `cd-da-reader` expose FUA / read speed / sub-channel reads? If not:
  fork it or write our own SCSI layer (READ CD, opcode 0xBE).
