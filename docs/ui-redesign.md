# LocalBeat Studio (was ripshelf) redesign + music player (plan, 2026-11-11)

Goal: turn ripshelf-gui into one polished app ("iTunes for 2025-2050"):
library + player, CD ripping, phone sync, tag review, in one design language.
Linux first (Fedora/Ubuntu), Windows later. Done in chunks; each chunk ends
built, committed and installed, and stands on its own.

## Design language: "Prism"

Dark-first (light variant kept), inspired by light on a CD's data side.

- Ink backgrounds: base #0B0C14, content #10121C, cards #171A28, hover
  #1F2335, lines #2A2F45; text #ECEEF7 / dim #9AA0B8 / faint #646A85
- Prism gradient (the signature): cyan #3EE0FF -> blue #5B8CFF -> violet
  #A06BFF -> magenta #FF5FA8 -> amber #FFB547. Used sparingly: wordmark,
  active nav marker, progress/seek bars, the play button, the spinning disc
- States: mint #3DDC97 ok, amber #FFB547 warning, coral #FF6B6B error
- Vector-drawn icons (play, pause, next, prev, stop, eject, shuffle, repeat,
  volume, queue) instead of emoji glyphs
- System fonts loaded at runtime (Cantarell / Noto Sans on Linux, Segoe UI on
  Windows), egui default as fallback
- Album pages tinted with the cover's dominant colour

## Layout

- Left sidebar: wordmark, search; LIBRARY (Albums, Artists, Songs, Recently
  added); DEVICES (each CD drive with a live status dot / mini progress,
  Phone); TOOLS (Review tags)
- Bottom now-playing bar on every page: cover, title/artist, transport,
  gradient seek bar, format badge (FLAC 16/44.1), volume, queue toggle
- Rip page: a "deck" card per drive (spinning disc while reading, model,
  offset, status, its own Stop & eject / Eject grouped on the card); page
  header holds the all-drive controls (Pause all / Resume, Eject all, Notify
  phone); selected drive's album + track cards below; log as a drawer

## Chunks

1. ✅ (2026-11-11) **Design system + app shell.** theme.rs rewritten (palette, fonts,
   gradient painter, icon painter, card/badge/button helpers); sidebar
   navigation replaces the top tabs; Rip, Phone and Review pages moved into
   it unchanged in function. Install.
2. ✅ (2026-11-11) **Rip page redesign.** Drive deck cards with grouped controls and the
   animated disc, header with all-drive controls, album/track layout,
   release-choice dialog, log drawer. Install.
3. ✅ (2026-11-11) **Playback engine** (new crate `app/player`, no UI): Symphonia decode
   (add the `flac` feature; FLAC, MP3, AAC/ALAC, Ogg Vorbis, WAV; .m4p shown
   as DRM-locked), cpal output (ALSA -> PipeWire), gapless queue at the same
   sample rate, seek, pause, volume, position, events. CLI `ripshelf play
   FILE...` to test. **Needs `sudo dnf install alsa-lib-devel`** (Ubuntu:
   `libasound2-dev`).
4. ✅ (2026-11-11) **Library index**: scan FLAC_DIR + MISC_DIR with lofty, cache in
   `<cache>/rip-cd/library.json` (path, size, mtime, tags, duration, format),
   incremental rescans, rescan of an album after a rip; albums grouped by
   album artist + album tag; cover finder + thumbnail cache on disk.
5. ✅ (2026-11-11) **Library UI + now-playing bar**: Albums grid, album page (tinted),
   Artists, Songs table, search, Recently added; play / play next / add to
   queue; Up Next drawer; keyboard (space, arrows).
6. ✅ (2026-11-11) **Desktop integration + polish**: MPRIS via souvlaki (KDE media widget,
   media keys; Windows SMTC later), remember queue/position/volume, phone and
   review pages restyled, notes in CLAUDE.md.

## Checking the look

`LOCALBEAT_DEMO=1` (Rip page), `=phone` or `=review` opens that page with a
made-up session (`=albums|album|play|artists|songs|recent|search` use the
real library at volume 0; `=sr2` shows the second drive, `=choice` the release
dialog); e.g.
`LOCALBEAT_DEMO=1 target/debug/localbeat-studio & sleep 4; spectacle -b -n -a -o shot.png`.

Screenshots of the real window (LOCALBEAT_DEMO=1 for a made-up rip session)
with spectacle, then read the PNG.
