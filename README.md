# LocalBeat Studio

A desktop music app for Linux and Windows, part of the LocalBeat suite
(with the LocalBeat Android player and LocalBeatServer).

- **Rip CDs** to verified FLAC: AccurateRip and CUETools DB checks, re-reads
  with the drive cache defeated, repair of damaged tracks from CUETools
  parity, several drives at once
- **Tag** from MusicBrainz (original dates, genres, cover art), and fix tags
  on older files by name or by sound (AcoustID)
- **Play** the library (gapless, MPRIS media keys)
- **Sync** Opus copies to an Android phone over USB (adb) or Wi-Fi (KDE
  Connect)

## Build

Needs Rust, CMake and a C/C++ compiler (plus ALSA headers on Linux).

```sh
cargo test --workspace
tools/install-linux.sh     # release build, installs localbeat + localbeat-studio
```

`localbeat --help` lists the command-line tools. Notes for working on the
code are in [CLAUDE.md](CLAUDE.md) and [docs/](docs/).

This repo began as a set of rip scripts ("CDRipPipeline"); they're preserved
at the git tag `scripts-final`.

## License

MIT: see [LICENSE](LICENSE).
