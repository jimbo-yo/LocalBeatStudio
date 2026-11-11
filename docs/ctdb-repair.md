# CUETools DB (CTDB) repair for ripshelf: design notes

Goal: repair tracks a damaged disc can't give us, using the parity data the
CUETools database keeps per disc, and verify tracks AccurateRip can't
(consistent misreads that two identical reads let through).

Reference implementation (LGPL): github.com/gchudov/cuetools.net
- `CUETools.AccurateRip/CDRepair.cs`: FindOffset, VerifyParity (decode), fix
- `CUETools.AccurateRip/AccurateRip.cs`: CTDBCRC, syndrome/parity calculation
  (`CalculateCRCs`, `SyndromeCalc16`, `GetSyndrome`, leadin/leadout handling,
  `maxNpar = 16`)
- `CUETools.CTDB/CUEToolsDB.cs`: lookup, parity download (`FetchDB`)
- `CUETools.Parity/`: `Galois.cs` (GF(2^16)), `RsDecode.cs` (calcSigmaMBM,
  chienSearch, doForney), `Parity2Syndrome.cs`

## Protocol (verified 2026-11-11)

- Lookup: `http://db.cuetools.net/lookup2.php?version=3&ctdb=1&fuzzy=0&metadata=none&toc=T`
  where T = track start sectors **minus 150**, `:`-separated, then the lead-out
  (CDTOC values are hex and include the 150). Returns XML `<entry>`s:
  `id, confidence, crc32 (whole disc), npar (8 or 16), stride (5880),
  hasparity (URL, e.g. http://p.cuetools.net/11118), syndrome (base64,
  small), trackcrcs (per track CRC32)`.
- Parity: GET the `hasparity` URL; `npar * stride * 2` bytes; client fetches
  with Range requests for npar 4, 8, 16 (`FetchDB`), converts with
  `ParityToSyndrome.Bytes2Syndrome(stride, npar, bytes)`.

## Layout

- Disc image = all audio as 16-bit words (L, R interleaved), from the first
  track's start to the lead-out. `stride` = 5880 words (2940 stereo samples =
  5 sectors). Column j = words j, j+stride, j+2*stride...: one Reed-Solomon
  codeword over GF(2^16) per column, `npar` parity symbols, so each column
  fixes up to npar/2 wrong words. A damaged stretch spreads over many
  columns (1-2 errors each), which is why ~1 s of damage is repairable.
- `laststride = stride + (samples*2) % stride`; `stridecount = (samples*2)
  / stride - 2` (leadin and leadout strides excluded).
- `CTDBCRC(offset)` = CRC32 of the image minus `stride/2` samples at the start
  and `laststride/2` at the end.

## Grace (Jeff Buckley) test data

- TOC `0:25655:49807:70462:90882:112130:143122:173392:186630:208530:233017`.
  Entry **11118** (conf 999, npar 16) is our pressing: our rips (HP, +102)
  give the same CRC32 for tracks 2, 3, 4, 7, 8, 9 (plain CRC32 of the track's
  audio, no offset). Entry 167943 (conf 1068, npar 8) is another pressing.
- Track 5 (`ec2de8f3`, CTDB `ee3f688e`) and track 6 Hallelujah (repaired
  by hand, `8ea2b70e`, CTDB `f1960f07`) are wrong: consistent misreads.
- Tracks 1 and 10 don't match with the stride/2 / laststride/2 exclusions
  tried (2940 / 4116 samples) although both are AccurateRip-verified: the
  edge rule (`CTDBCRC(iTrack, oi, prefix, suffix)` and the `_CRC32[track,
  offs]` indexing) still needs reading. Settle this first.
- Saved: 8 clean reads of track 6 in `~/.cache/rip-cd/rejected/Jeff Buckley -
  Grace/reads/track06/` (raw 16-bit LE stereo). Damage 0:10.4-0:13.5, ~430
  frames differ between reads, each with exactly 2 versions; one is usually
  the drive's interpolation (478 of 496 channel diffs).

## Plan

1. Track/disc CRCs exactly as CTDB (settle the edge rule on Grace 1 and 10).
2. Port Galois16 + syndrome calculation; check: our syndromes for a clean
   disc (OK Computer, all AR-verified) equal the database's (no errors).
3. Port RsDecode (Berlekamp-Massey, Chien, Forney); repair Grace from the
   library FLACs + saved reads; result must match CTDB crc32 and AccurateRip.
4. `ripshelf repair DIR` (CLI), then in the rip: after AR fails, try CTDB.
