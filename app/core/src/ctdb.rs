//! The CUETools database (CTDB): per-disc checksums and parity, for checking
//! tracks AccurateRip can't vouch for and (later) repairing damaged ones.
//! See docs/ctdb-repair.md. Reference: CUETools (LGPL),
//! CUETools.AccurateRip/AccurateRip.cs `CTDBCRC` and CDRepair.cs.
//!
//! Layout: the disc's audio as 16-bit words (L, R, L, R...) from the start
//! of track 1 to the lead-out, cut into rows of [`STRIDE`] words. The
//! database calls the stride "5880": that's stereo samples (10 sectors);
//! in 16-bit words it's 11760. The first row (lead-in) and the last row,
//! `laststride` words long, are left out of the checksums.

use anyhow::{Context, Result, anyhow};

/// Row length in 16-bit words (5880 stereo samples, 10 CD sectors).
pub const STRIDE: usize = 11760;

/// Row geometry for a disc of `samples` stereo samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub samples: u64,
    /// Words in the last row: one stride plus whatever doesn't fill a row.
    pub laststride: usize,
    /// Rows between the lead-in and lead-out rows (the parity's codeword length).
    pub stridecount: usize,
}

impl Layout {
    pub fn new(samples: u64) -> Self {
        let words = samples * 2;
        Layout {
            samples,
            laststride: STRIDE + (words % STRIDE as u64) as usize,
            stridecount: (words / STRIDE as u64).saturating_sub(2) as usize,
        }
    }

    /// Stereo samples left out at the start of track 1 and the end of the last track.
    pub fn prefix_samples(&self) -> usize {
        STRIDE / 2
    }
    pub fn suffix_samples(&self) -> usize {
        self.laststride / 2
    }
}

fn crc_frames(h: &mut crc32fast::Hasher, frames: &[u32]) {
    let mut buf = Vec::with_capacity(4096 * 4);
    for chunk in frames.chunks(4096) {
        buf.clear();
        for f in chunk {
            buf.extend_from_slice(&f.to_le_bytes());
        }
        h.update(&buf);
    }
}

/// CTDB's per-track CRC32s (`TrackCRC`): each track's audio, but track 1
/// without its first `stride/2` samples and the last track without its last
/// `laststride/2`. `tracks` = every track of the disc, in order, as stereo
/// frames (`u32`, little-endian L then R, as in `secure_read`).
pub fn track_crcs(tracks: &[&[u32]]) -> Vec<u32> {
    let samples: u64 = tracks.iter().map(|t| t.len() as u64).sum();
    let l = Layout::new(samples);
    let n = tracks.len();
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let a = if i == 0 { l.prefix_samples().min(t.len()) } else { 0 };
            let b = if i + 1 == n { t.len().saturating_sub(l.suffix_samples()) } else { t.len() };
            let mut h = crc32fast::Hasher::new();
            crc_frames(&mut h, &t[a..b.max(a)]);
            h.finalize()
        })
        .collect()
}

/// CTDB's whole-disc CRC32 (`CTDBCRC(0)`): all audio without the first
/// `stride/2` and the last `laststride/2` samples.
pub fn disc_crc(tracks: &[&[u32]]) -> u32 {
    let samples: u64 = tracks.iter().map(|t| t.len() as u64).sum();
    let l = Layout::new(samples);
    let (skip, keep) = (l.prefix_samples() as u64, samples.saturating_sub(l.suffix_samples() as u64));
    let mut h = crc32fast::Hasher::new();
    let mut pos = 0u64;
    for t in tracks {
        let (s, e) = (pos, pos + t.len() as u64);
        let a = skip.clamp(s, e) - s;
        let b = keep.clamp(s, e) - s;
        if b > a {
            crc_frames(&mut h, &t[a as usize..b as usize]);
        }
        pos = e;
    }
    h.finalize()
}

/// The TOC as CTDB wants it: track starts relative to track 1, then the
/// lead-out, in sectors, `:`-separated ("0:25655:...:233017").
pub fn toc_string(starts: &[u32], leadout: u32) -> String {
    let first = starts.first().copied().unwrap_or(0);
    starts.iter().chain(std::iter::once(&leadout)).map(|s| (s - first).to_string()).collect::<Vec<_>>().join(":")
}

/// TOC string from a CDTOC tag / `Toc` display ("N+start...+leadout", hex).
pub fn toc_from_cdtoc(cdtoc: &str) -> Result<String> {
    let parts: Vec<u32> = cdtoc.split('+').map(|h| u32::from_str_radix(h, 16)).collect::<Result<_, _>>().map_err(|e| anyhow!("bad CDTOC {cdtoc:?}: {e}"))?;
    let n = *parts.first().ok_or_else(|| anyhow!("empty CDTOC"))? as usize;
    if parts.len() < n + 2 {
        return Err(anyhow!("CDTOC {cdtoc:?} is too short"));
    }
    Ok(toc_string(&parts[1..=n], parts[n + 1]))
}

/// Whether the database has repair data for this disc (any entry with
/// parity). False if it can't be asked.
pub fn has_parity(cdtoc: &str) -> bool {
    toc_from_cdtoc(cdtoc).and_then(|t| lookup(&t)).is_ok_and(|e| e.iter().any(|x| x.parity_url.is_some()))
}

/// TOC string for back-to-back tracks of these lengths (stereo samples):
/// a whole disc ripped by LocalBeat Studio, no data tracks.
pub fn toc_from_lengths(lengths: &[usize]) -> Result<String> {
    let mut starts = Vec::new();
    let mut pos = 0u32;
    for &n in lengths {
        if n % 588 != 0 {
            return Err(anyhow!("a track isn't a whole number of CD sectors: not a straight CD rip"));
        }
        starts.push(pos);
        pos += (n / 588) as u32;
    }
    Ok(toc_string(&starts, pos))
}

/// One pressing (submission group) in the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: u64,
    /// How many rips agreed.
    pub confidence: u32,
    /// Whole-disc CRC32 (see [`disc_crc`]).
    pub crc32: u32,
    /// Parity symbols per column (8 or 16): fixes up to npar/2 bad words per
    /// column, npar if their positions are known.
    pub npar: u32,
    /// Where its parity is, if it has any.
    pub parity_url: Option<String>,
    pub track_crcs: Vec<u32>,
}

/// Looks the TOC up (exact layout only).
pub fn lookup(toc: &str) -> Result<Vec<Entry>> {
    let url = format!("http://db.cuetools.net/lookup2.php?version=3&ctdb=1&fuzzy=0&metadata=none&toc={toc}");
    let body = crate::http::agent()
        .get(&url)
        .call()
        .context("CUETools database lookup")?
        .body_mut()
        .read_to_string()
        .context("reading the CUETools database's answer")?;
    Ok(parse_entries(&body))
}

/// An album name another database (MusicBrainz, Discogs, freedb) gives for
/// a disc layout, through the CUETools database.
#[derive(Debug, Clone, PartialEq)]
pub struct NameHit {
    pub artist: String,
    pub album: String,
    pub relevance: u32,
    /// The MusicBrainz release, when that's the source.
    pub mbid: Option<String>,
}

/// Album names for a disc, most relevant first. `starts` and `leadout` are
/// absolute sectors as MusicBrainz counts them (150-sector pregap included):
/// freedb's entries depend on the absolute positions.
pub fn names(starts: &[u32], leadout: u32) -> Result<Vec<NameHit>> {
    let toc: Vec<String> = starts.iter().chain(std::iter::once(&leadout)).map(|s| s.saturating_sub(150).to_string()).collect();
    let url = format!("http://db.cuetools.net/lookup2.php?version=3&ctdb=0&fuzzy=1&metadata=extensive&toc={}", toc.join(":"));
    let body = crate::http::agent()
        .get(&url)
        .call()
        .context("CUETools database lookup")?
        .body_mut()
        .read_to_string()
        .context("reading the CUETools database's answer")?;
    Ok(parse_names(&body))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// The `<metadata .../>` elements of a lookup2.php answer, most relevant
/// first, one per distinct artist + album.
pub fn parse_names(xml: &str) -> Vec<NameHit> {
    let mut out: Vec<NameHit> = Vec::new();
    for part in xml.split("<metadata ").skip(1) {
        let end = part.find('>').unwrap_or(part.len());
        // Leading space so " id=" can't match inside "discid=".
        let attrs = format!(" {}", &part[..end]);
        let get = |name: &str| -> Option<String> {
            let key = format!(" {name}=\"");
            let i = attrs.find(&key)? + key.len();
            let j = attrs[i..].find('"')? + i;
            Some(xml_unescape(&attrs[i..j]))
        };
        let (Some(artist), Some(album)) = (get("artist"), get("album")) else { continue };
        let hit = NameHit {
            artist,
            album,
            relevance: get("relevance").and_then(|s| s.parse().ok()).unwrap_or(0),
            mbid: get("source").filter(|s| s == "musicbrainz").and(get("id")),
        };
        match out.iter_mut().find(|h| h.artist.eq_ignore_ascii_case(&hit.artist) && h.album.eq_ignore_ascii_case(&hit.album)) {
            Some(h) => {
                h.relevance = h.relevance.max(hit.relevance);
                if h.mbid.is_none() {
                    h.mbid = hit.mbid;
                }
            }
            None => out.push(hit),
        }
    }
    out.sort_by(|a, b| b.relevance.cmp(&a.relevance));
    out
}

/// The `<entry .../>` elements of a lookup2.php answer.
pub fn parse_entries(xml: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for part in xml.split("<entry ").skip(1) {
        let end = part.find('>').unwrap_or(part.len());
        let attrs = &part[..end];
        let get = |name: &str| -> Option<String> {
            let key = format!("{name}=\"");
            let i = attrs.find(&key)? + key.len();
            let j = attrs[i..].find('"')? + i;
            Some(attrs[i..j].to_string())
        };
        let hex = |s: &str| u32::from_str_radix(s.trim(), 16).ok();
        let Some(id) = get("id").and_then(|s| s.parse().ok()) else { continue };
        out.push(Entry {
            id,
            confidence: get("confidence").and_then(|s| s.parse().ok()).unwrap_or(0),
            crc32: get("crc32").as_deref().and_then(hex).unwrap_or(0),
            npar: get("npar").and_then(|s| s.parse().ok()).unwrap_or(0),
            parity_url: get("hasparity").filter(|s| !s.is_empty()),
            track_crcs: get("trackcrcs").map(|s| s.split_whitespace().filter_map(hex).collect()).unwrap_or_default(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_names_from_metadata() {
        let xml = r#"<ctdb><entry />
 <metadata album="Timeless Classic Albums" artist="Frank Sinatra" id="14106661" relevance="50" source="discogs" year="2018"><track name="x" /></metadata>
 <metadata album="Swing Along With Me" artist="Frank Sinatra" genre="jazz" id="jazz/8e07c90c" relevance="99" source="freedb"></metadata>
 <metadata album="Swing along with me" artist="Frank Sinatra" id="983e1fd5-9cf7-3769-82e2-7434964f8e40" relevance="70" source="musicbrainz"></metadata>
 <metadata album="Don&apos;t Cry Joe" artist="Frank Sinatra" relevance="10" source="freedb"></metadata></ctdb>"#;
        let n = parse_names(xml);
        assert_eq!(n.len(), 3);
        assert_eq!((n[0].album.as_str(), n[0].relevance), ("Swing Along With Me", 99));
        assert_eq!(n[0].mbid.as_deref(), Some("983e1fd5-9cf7-3769-82e2-7434964f8e40"));
        assert_eq!(n[1].album, "Timeless Classic Albums");
        assert_eq!(n[1].mbid, None);
        assert_eq!(n[2].album, "Don't Cry Joe");
    }

    #[test]
    fn layout_of_grace() {
        // Jeff Buckley, Grace: lead-out at 233017 sectors.
        let l = Layout::new(233017 * 588);
        assert_eq!(l.laststride, 19992);
        assert_eq!(l.prefix_samples(), 5880);
        assert_eq!(l.suffix_samples(), 9996);
    }

    #[test]
    fn edge_tracks_leave_out_the_lead_in_and_lead_out_rows() {
        // Three tracks of 3 sectors each: frame n holds n.
        let all: Vec<u32> = (0..9 * 588).collect();
        let t: Vec<&[u32]> = all.chunks(3 * 588).collect();
        let l = Layout::new(all.len() as u64);
        let crc = |r: &[u32]| {
            let mut h = crc32fast::Hasher::new();
            crc_frames(&mut h, r);
            h.finalize()
        };
        let got = track_crcs(&t);
        let skip = l.prefix_samples().min(t[0].len());
        assert_eq!(got[0], crc(&t[0][skip..]));
        assert_eq!(got[1], crc(t[1]));
        let keep = t[2].len().saturating_sub(l.suffix_samples());
        assert_eq!(got[2], crc(&t[2][..keep]));
        let disc_keep = all.len().saturating_sub(l.suffix_samples());
        assert_eq!(disc_crc(&t), crc(&all[l.prefix_samples().min(disc_keep)..disc_keep]));
    }

    #[test]
    fn toc_strings() {
        assert_eq!(toc_from_cdtoc("2+96+3E8+BB8").unwrap(), "0:850:2850");
        assert_eq!(toc_string(&[150, 1150], 3150), "0:1000:3000");
        assert_eq!(toc_from_lengths(&[588 * 1000, 588 * 2000]).unwrap(), "0:1000:3000");
        assert!(toc_from_lengths(&[100]).is_err());
    }

    #[test]
    fn parses_a_lookup_answer() {
        let xml = r#"<?xml version="1.0"?><ctdb><entry confidence="999" crc32="1521c397" hasparity="http://p.cuetools.net/11118" id="11118" npar="16" stride="5880" syndrome="AAA=" toc="0:1:2" trackcrcs="b6bf4480 22339c25"/><entry confidence="1" crc32="2" id="5" npar="16" trackcrcs=""/></ctdb>"#;
        let e = parse_entries(xml);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, 11118);
        assert_eq!(e[0].confidence, 999);
        assert_eq!(e[0].crc32, 0x1521c397);
        assert_eq!(e[0].npar, 16);
        assert_eq!(e[0].parity_url.as_deref(), Some("http://p.cuetools.net/11118"));
        assert_eq!(e[0].track_crcs, vec![0xb6bf4480, 0x22339c25]);
        assert_eq!(e[1].parity_url, None);
        assert!(e[1].track_crcs.is_empty());
    }
}

// ---------------------------------------------------------------- parity

/// GF(2^16) with CUETools' polynomial (x^16 + x^12 + x^3 + x + 1), as log /
/// antilog tables; `exp` is doubled so sums of two logs need no modulo.
pub struct Gf16 {
    pub exp: Vec<u16>,
    pub log: Vec<u16>,
}

pub const GF_MAX: usize = 65535;

pub fn gf() -> &'static Gf16 {
    static GF: std::sync::OnceLock<Gf16> = std::sync::OnceLock::new();
    GF.get_or_init(|| {
        let mut exp = vec![0u16; GF_MAX * 2];
        let mut log = vec![0u16; GF_MAX + 1];
        let mut d: u32 = 1;
        for i in 0..GF_MAX {
            exp[i] = d as u16;
            exp[GF_MAX + i] = d as u16;
            log[d as usize] = i as u16;
            d <<= 1;
            if d & 0x10000 != 0 {
                d = (d ^ 0x1100B) & 0xFFFF;
            }
        }
        Gf16 { exp, log }
    })
}

impl Gf16 {
    pub fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 { 0 } else { self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize] }
    }
    /// a * α^e
    pub fn mul_exp(&self, a: u16, e: usize) -> u16 {
        if a == 0 { 0 } else { self.exp[self.log[a as usize] as usize + e % GF_MAX] }
    }
    pub fn div(&self, a: u16, b: u16) -> u16 {
        if a == 0 { 0 } else { self.exp[self.log[a as usize] as usize + GF_MAX - self.log[b as usize] as usize] }
    }
}

/// The disc as 16-bit words (L, R, L, R...), all tracks back to back.
pub fn disc_words(tracks: &[&[u32]]) -> Vec<u16> {
    let mut out = Vec::with_capacity(tracks.iter().map(|t| t.len() * 2).sum());
    for t in tracks {
        for f in *t {
            out.push(*f as u16);
            out.push((*f >> 16) as u16);
        }
    }
    out
}

/// Syndromes of each column of `words` (a whole disc, the layout's length),
/// `npar` per column: syn[col * npar + i] = Σ over rows 1..=stridecount of
/// word(row, col) · α^(i · (stridecount - row)). The lead-in row and the
/// last row(s) aren't in the code. These are what CTDB's parity file holds
/// for a disc it knows; ours XOR theirs is the syndrome of our errors.
pub fn column_syndromes(words: &[u16], npar: usize) -> Vec<u16> {
    let layout = Layout::new((words.len() / 2) as u64);
    let g = gf();
    let mut syn = vec![0u16; STRIDE * npar];
    for row in 1..=layout.stridecount {
        let base = row * STRIDE;
        for col in 0..STRIDE {
            let d = words[base + col];
            let s = &mut syn[col * npar..col * npar + npar];
            for (i, v) in s.iter_mut().enumerate() {
                let shifted = if *v == 0 { 0 } else { g.exp[g.log[*v as usize] as usize + i] };
                *v = shifted ^ d;
            }
        }
    }
    syn
}

/// A downloaded parity file (`npar` blocks of STRIDE little-endian words)
/// as syn[col * npar + i], the layout of [`column_syndromes`].
pub fn parse_parity(bytes: &[u8], npar: usize) -> Result<Vec<u16>> {
    if bytes.len() < npar * STRIDE * 2 {
        return Err(anyhow!("parity file too short: {} bytes for npar {npar}", bytes.len()));
    }
    let mut syn = vec![0u16; STRIDE * npar];
    for i in 0..npar {
        for col in 0..STRIDE {
            let k = (i * STRIDE + col) * 2;
            syn[col * npar + i] = u16::from_le_bytes([bytes[k], bytes[k + 1]]);
        }
    }
    Ok(syn)
}

/// Downloads an entry's parity file.
pub fn fetch_parity(entry: &Entry) -> Result<Vec<u16>> {
    let url = entry.parity_url.as_ref().ok_or_else(|| anyhow!("entry {} has no parity", entry.id))?;
    let mut body = Vec::new();
    use std::io::Read;
    crate::http::agent()
        .get(url)
        .call()
        .context("downloading CTDB parity")?
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)?;
    parse_parity(&body, entry.npar as usize)
}

/// How our disc compares with an entry's parity: columns whose syndromes
/// agree exactly, out of STRIDE.
pub fn matching_columns(ours: &[u16], theirs: &[u16], npar: usize) -> usize {
    ours.chunks(npar).zip(theirs.chunks(npar)).filter(|(a, b)| a == b).count()
}

/// Syndromes of column `col` of the disc shifted by `k` words (the
/// shifted disc's word p is our word p + k), from our offset-0 column
/// syndromes `base` and the words themselves. A shift moves a column into
/// its neighbour `col + k`; past a row's end it also slides one row, which
/// adds the word entering at one end and drops the one leaving at the other
/// (CUETools `GetSyndrome(.., offset)`).
pub fn shifted_column(words: &[u16], base: &[u16], npar: usize, col: usize, k: isize) -> Vec<u16> {
    let layout = Layout::new((words.len() / 2) as u64);
    let r = layout.stridecount;
    let g = gf();
    let s = col as isize + k;
    let (src, rows) = if s >= STRIDE as isize {
        ((s - STRIDE as isize) as usize, 1)
    } else if s < 0 {
        ((s + STRIDE as isize) as usize, -1)
    } else {
        (s as usize, 0)
    };
    let orig = &base[src * npar..src * npar + npar];
    let w = |row: usize| words[row * STRIDE + src];
    (0..npar)
        .map(|i| match rows {
            0 => orig[i],
            // Rows 2..=R+1 of `src`: drop row 1 (weight α^(i·R) after the shift), add row R+1.
            1 => g.mul_exp(orig[i], i) ^ g.mul_exp(w(1), i * r) ^ w(r + 1),
            // Rows 0..=R-1: drop row R, add row 0 (weight α^(i·(R-1))).
            _ => {
                let x = orig[i] ^ w(r);
                let x = if x == 0 { 0 } else { g.exp[g.log[x as usize] as usize + GF_MAX - i % GF_MAX] };
                x ^ g.mul_exp(w(0), i * (r - 1))
            }
        })
        .collect()
}

/// The shift (in words, even: whole stereo samples) at which our disc lines
/// up with an entry's parity, searched within half a row either way: the
/// one where a few columns spread across the row agree exactly.
pub fn find_shift(words: &[u16], base: &[u16], theirs: &[u16], npar: usize) -> Option<isize> {
    let probe = [0usize, 1471, 2942, 4413, 5884, 7355, 8826, 10297];
    let half = (STRIDE / 2) as isize;
    let mut best: Option<(usize, isize)> = None;
    let mut k = -half + 2;
    while k < half {
        let agree = probe
            .iter()
            .filter(|&&c| shifted_column(words, base, npar, c, k) == theirs[c * npar..c * npar + npar])
            .count();
        if agree > best.map_or(0, |b| b.0) {
            best = Some((agree, k));
        }
        k += 2;
    }
    // More than half the probes: one damaged column can't decide it.
    best.filter(|b| b.0 * 2 > probe.len()).map(|b| b.1)
}

/// Columns agreeing with `theirs` when our disc is shifted by `k` words.
pub fn matching_columns_shifted(words: &[u16], base: &[u16], theirs: &[u16], npar: usize, k: isize) -> usize {
    (0..STRIDE).filter(|&c| shifted_column(words, base, npar, c, k) == theirs[c * npar..c * npar + npar]).count()
}

#[cfg(test)]
mod parity_tests {
    use super::*;

    /// A small made-up disc: 40 rows plus a ragged end.
    fn disc() -> Vec<u16> {
        let n = STRIDE * 40 + 3000;
        (0..n).map(|i| ((i as u64 * 2654435761) >> 7) as u16).collect()
    }

    #[test]
    fn shifting_matches_recomputing() {
        let w = disc();
        let npar = 8;
        let base = column_syndromes(&w, npar);
        for k in [-5878isize, -2, 2, 30, 5878] {
            // The shifted disc, computed the slow way: same length, word p = w[p + k].
            let shifted: Vec<u16> = (0..w.len() as isize).map(|p| *w.get((p + k).clamp(0, w.len() as isize - 1) as usize).unwrap()).collect();
            let want = column_syndromes(&shifted, npar);
            for col in [0usize, 1, 5879, 5880, 11000, STRIDE - 1] {
                assert_eq!(shifted_column(&w, &base, npar, col, k), want[col * npar..col * npar + npar], "k {k} col {col}");
            }
        }
    }

    #[test]
    fn finds_the_shift() {
        let w = disc();
        let npar = 8;
        let base = column_syndromes(&w, npar);
        let k = 1234;
        let shifted: Vec<u16> = (0..w.len() as isize).map(|p| *w.get((p + k).clamp(0, w.len() as isize - 1) as usize).unwrap()).collect();
        let theirs = column_syndromes(&shifted, npar);
        assert_eq!(find_shift(&w, &base, &theirs, npar), Some(k));
        assert_eq!(matching_columns_shifted(&w, &base, &theirs, npar, k), STRIDE);
    }
}
