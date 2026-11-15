//! The music library for the player: every song under the FLAC library and
//! Miscellaneous, with its tags, length and format, grouped into albums and
//! artists. Tags are read with lofty (fast: only the tag and header parts of
//! each file) and cached in `<cache>/library.json` keyed by path, size and
//! modification time, so a rescan only reads new or changed files.
//!
//! Albums are grouped like iTunes does, by album tag + album artist, so an
//! album split over folders ("Biograph CD1", "CD2") is one album. Songs
//! without an album artist group by album tag within their folder. When an
//! album is in both libraries, the FLAC copy wins (as in phone sync).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::config::Settings;

/// File types the library lists (.m4p is listed, marked DRM, never played).
pub const EXTENSIONS: [&str; 9] = ["flac", "mp3", "m4a", "m4p", "ogg", "oga", "wav", "aif", "aiff"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// The verified CD-rip library.
    Flac,
    /// Miscellaneous: older copies, as they came.
    Misc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Song {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since 1970: modification (for the cache) and creation (when
    /// the song arrived, for "Recently added"; mtime if the system doesn't say).
    pub mtime: i64,
    pub added: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Empty if the file doesn't say.
    pub album_artist: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    /// Original release year if tagged, else the release's.
    pub year: Option<u16>,
    pub genre: Option<String>,
    pub seconds: f64,
    /// "FLAC", "MP3", "AAC", "ALAC", "Vorbis", "WAV", "AIFF".
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    /// kbps.
    pub bitrate: Option<u32>,
    pub drm: bool,
    pub origin: Origin,
    /// Has an embedded picture.
    pub picture: bool,
}

impl Song {
    pub fn lossless(&self) -> bool {
        matches!(self.codec.as_str(), "FLAC" | "ALAC" | "WAV" | "AIFF")
    }
}

#[derive(Debug, Clone)]
pub struct Album {
    /// Stable identity (also names its cover thumbnail).
    pub key: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub genre: Option<String>,
    /// Indices into `Library::songs`, in disc/track order.
    pub songs: Vec<usize>,
    pub origin: Origin,
    /// The folders its songs are in (usually one).
    pub dirs: Vec<PathBuf>,
    pub seconds: f64,
    /// When its newest song arrived.
    pub added: i64,
    pub discs: u32,
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    /// Indices into `Library::albums`, oldest first.
    pub albums: Vec<usize>,
}

#[derive(Debug, Default)]
pub struct Library {
    pub songs: Vec<Song>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
}

/// What a scan did.
#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files: usize,
    /// Files whose tags were read (new or changed); the rest came from the cache.
    pub read: usize,
    pub failed: Vec<(PathBuf, String)>,
    /// Songs hidden because the same album is in the FLAC library.
    pub hidden_duplicates: usize,
}

/// Lower case, no accents, curly quotes straight, spaces squeezed: for
/// grouping and sorting, not display.
pub fn norm(s: &str) -> String {
    let s: String = s
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .map(|c| match c {
            '‘' | '’' | '′' => '\'',
            '“' | '”' => '"',
            '？' => '?',
            '∶' => ':',
            c => c,
        })
        .collect::<String>()
        .to_lowercase();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sort key for names: "The Smiths" sorts under S.
pub fn sort_name(s: &str) -> String {
    let n = norm(s);
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = n.strip_prefix(article) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    n
}

fn cache_path(s: &Settings) -> PathBuf {
    s.cache_dir.join("library.json")
}

fn load_cache(s: &Settings) -> HashMap<PathBuf, Song> {
    let Ok(bytes) = std::fs::read(cache_path(s)) else { return HashMap::new() };
    let songs: Vec<Song> = serde_json::from_slice(&bytes).unwrap_or_default();
    songs.into_iter().map(|x| (x.path.clone(), x)).collect()
}

fn save_cache(s: &Settings, songs: &[Song]) -> Result<()> {
    std::fs::create_dir_all(&s.cache_dir)?;
    let tmp = cache_path(s).with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec(songs)?)?;
    std::fs::rename(&tmp, cache_path(s))?;
    Ok(())
}

fn secs(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    t.ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

fn is_audio(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

impl Library {
    /// Scans the FLAC library and Miscellaneous. `progress(done, total)`.
    pub fn scan(s: &Settings, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<(Library, ScanStats)> {
        let roots = [(s.flac_dir.as_path(), Origin::Flac), (s.misc_dir.as_path(), Origin::Misc)];
        Self::scan_roots(&roots, s, progress)
    }

    pub fn scan_roots(roots: &[(&Path, Origin)], s: &Settings, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<(Library, ScanStats)> {
        let mut files: Vec<(PathBuf, Origin, u64, i64, i64)> = Vec::new();
        for (root, origin) in roots {
            if !root.is_dir() {
                continue;
            }
            for e in walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_map(|e| e.ok()) {
                if !e.file_type().is_file() || !is_audio(e.path()) {
                    continue;
                }
                // Hidden files and AppleDouble "._x.m4a" leftovers aren't songs.
                if e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let mtime = secs(md.modified()).unwrap_or(0);
                let added = secs(md.created()).unwrap_or(mtime);
                files.push((e.into_path(), *origin, md.len(), mtime, added));
            }
        }
        let mut stats = ScanStats { files: files.len(), ..Default::default() };
        let cache = load_cache(s);
        let mut songs: Vec<Option<Song>> = vec![None; files.len()];
        let mut todo = Vec::new();
        for (i, (path, origin, size, mtime, _)) in files.iter().enumerate() {
            match cache.get(path) {
                Some(c) if c.size == *size && c.mtime == *mtime && c.origin == *origin => songs[i] = Some(c.clone()),
                _ => todo.push(i),
            }
        }
        stats.read = todo.len();

        // Read the new/changed ones on all cores.
        let done = std::sync::atomic::AtomicUsize::new(files.len() - todo.len());
        let total = files.len();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
        let results: Vec<(usize, Result<Song>)> = std::thread::scope(|sc| {
            let chunks: Vec<&[usize]> = todo.chunks(todo.len().div_ceil(threads).max(1)).collect();
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    let (files, done) = (&files, &done);
                    sc.spawn(move || {
                        chunk
                            .iter()
                            .map(|&i| {
                                let (p, o, size, mtime, added) = &files[i];
                                let r = read_song(p, *o, *size, *mtime, *added);
                                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                if n % 25 == 0 {
                                    progress(n, total);
                                }
                                (i, r)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        for (i, r) in results {
            match r {
                Ok(song) => songs[i] = Some(song),
                Err(e) => {
                    let (p, o, size, mtime, added) = &files[i];
                    stats.failed.push((p.clone(), format!("{e:#}")));
                    // Still listed, named after the file and folder.
                    songs[i] = Some(fallback_song(p, *o, *size, *mtime, *added));
                }
            }
        }
        progress(total, total);
        let songs: Vec<Song> = songs.into_iter().flatten().collect();
        if stats.read > 0 || cache.len() != songs.len() {
            save_cache(s, &songs).context("saving the library cache")?;
        }
        let lib = Library::build(songs, &mut stats);
        Ok((lib, stats))
    }

    /// Groups songs into albums and artists.
    pub fn build(mut songs: Vec<Song>, stats: &mut ScanStats) -> Library {
        // A song with no album artist next to songs of the same album that
        // have one (one file the tagger missed) belongs with them.
        let dir_of = |x: &Song| x.path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let mut folder_artist: HashMap<(String, String), String> = HashMap::new();
        for x in &songs {
            if !x.album_artist.is_empty() {
                folder_artist.entry((norm(&x.album), dir_of(x))).or_insert_with(|| x.album_artist.clone());
            }
        }
        for x in songs.iter_mut().filter(|x| x.album_artist.is_empty()) {
            if let Some(a) = folder_artist.get(&(norm(&x.album), dir_of(x))) {
                x.album_artist = a.clone();
            }
        }
        // Album key per song.
        let key_of = |x: &Song| -> String {
            let album = norm(&x.album);
            let dir = x.path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            if album.is_empty() || album.starts_with("unknown album") {
                return format!("dir:{dir}");
            }
            if x.album_artist.is_empty() {
                format!("{album}\u{1}dir:{dir}")
            } else {
                format!("{album}\u{1}{}", norm(&x.album_artist))
            }
        };
        // An album in both libraries: keep the FLAC songs only.
        let mut has_flac: HashMap<String, bool> = HashMap::new();
        for x in &songs {
            *has_flac.entry(key_of(x)).or_default() |= x.origin == Origin::Flac;
        }
        let before = songs.len();
        songs.retain(|x| x.origin == Origin::Flac || !has_flac.get(&key_of(x)).copied().unwrap_or(false));
        stats.hidden_duplicates = before - songs.len();

        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, x) in songs.iter().enumerate() {
            groups.entry(key_of(x)).or_default().push(i);
        }
        let mut albums: Vec<Album> = groups
            .into_iter()
            .map(|(key, mut idx)| {
                idx.sort_by(|&a, &b| {
                    let (x, y) = (&songs[a], &songs[b]);
                    (x.disc.unwrap_or(1), x.track.unwrap_or(u32::MAX), &x.path).cmp(&(y.disc.unwrap_or(1), y.track.unwrap_or(u32::MAX), &y.path))
                });
                let first = &songs[idx[0]];
                let title = if key.starts_with("dir:") {
                    first.path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                } else {
                    first.album.clone()
                };
                let artist = if !first.album_artist.is_empty() {
                    first.album_artist.clone()
                } else {
                    let mut names: Vec<String> = idx.iter().map(|&i| norm(&songs[i].artist)).collect();
                    names.sort();
                    names.dedup();
                    if names.len() > 1 { "Various Artists".to_string() } else { first.artist.clone() }
                };
                let mut dirs: Vec<PathBuf> = idx.iter().filter_map(|&i| songs[i].path.parent().map(Path::to_path_buf)).collect();
                dirs.sort();
                dirs.dedup();
                Album {
                    title,
                    artist,
                    year: idx.iter().filter_map(|&i| songs[i].year).min(),
                    genre: idx.iter().find_map(|&i| songs[i].genre.clone()),
                    origin: first.origin,
                    seconds: idx.iter().map(|&i| songs[i].seconds).sum(),
                    added: idx.iter().map(|&i| songs[i].added).max().unwrap_or(0),
                    discs: idx.iter().filter_map(|&i| songs[i].disc).max().unwrap_or(1),
                    dirs,
                    songs: idx,
                    key,
                }
            })
            .collect();
        albums.sort_by(|a, b| (sort_name(&a.artist), a.year, sort_name(&a.title)).cmp(&(sort_name(&b.artist), b.year, sort_name(&b.title))));

        let mut by_artist: BTreeMap<String, Artist> = BTreeMap::new();
        for (i, a) in albums.iter().enumerate() {
            by_artist.entry(sort_name(&a.artist)).or_insert_with(|| Artist { name: a.artist.clone(), albums: Vec::new() }).albums.push(i);
        }
        let artists = by_artist.into_values().collect();
        Library { songs, albums, artists }
    }

    /// Albums, newest arrivals first.
    pub fn recently_added(&self, n: usize) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.albums.len()).collect();
        v.sort_by_key(|&i| std::cmp::Reverse(self.albums[i].added));
        v.truncate(n);
        v
    }
}

fn file_stem(p: &Path) -> String {
    p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn fallback_song(p: &Path, origin: Origin, size: u64, mtime: i64, added: i64) -> Song {
    Song {
        path: p.to_path_buf(),
        size,
        mtime,
        added,
        title: file_stem(p),
        artist: String::new(),
        album: String::new(),
        album_artist: String::new(),
        track: None,
        disc: None,
        year: None,
        genre: None,
        seconds: 0.0,
        codec: p.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default(),
        sample_rate: None,
        bits: None,
        bitrate: None,
        drm: p.extension().is_some_and(|e| e.eq_ignore_ascii_case("m4p")),
        origin,
        picture: false,
    }
}

fn read_song(p: &Path, origin: Origin, size: u64, mtime: i64, added: i64) -> Result<Song> {
    // Relaxed: a malformed frame (e.g. a date with "_" in it) is skipped,
    // not the whole tag.
    let opts = lofty::config::ParseOptions::new().parsing_mode(lofty::config::ParsingMode::Relaxed);
    let tf = lofty::probe::Probe::open(p)
        .and_then(|pr| pr.options(opts).read())
        .with_context(|| format!("reading {}", p.display()))?;
    let props = tf.properties();
    let mut song = fallback_song(p, origin, size, mtime, added);
    song.seconds = props.duration().as_secs_f64();
    song.sample_rate = props.sample_rate();
    song.bitrate = props.audio_bitrate();
    song.codec = match tf.file_type() {
        FileType::Flac => "FLAC",
        FileType::Mpeg => "MP3",
        // Apple Lossless reports a bit depth; AAC doesn't.
        FileType::Mp4 if props.bit_depth().is_some() => "ALAC",
        FileType::Mp4 => "AAC",
        FileType::Vorbis => "Vorbis",
        FileType::Wav => "WAV",
        FileType::Aiff => "AIFF",
        _ => "audio",
    }
    .to_string();
    if song.lossless() {
        song.bits = props.bit_depth();
    }
    if let Some(tag) = tf.primary_tag().or_else(|| tf.first_tag()) {
        let text = |v: Option<std::borrow::Cow<'_, str>>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(t) = text(tag.title()) {
            song.title = t;
        }
        song.artist = text(tag.artist()).unwrap_or_default();
        song.album = text(tag.album()).unwrap_or_default();
        song.album_artist = tag.get_string(ItemKey::AlbumArtist).map(|s| s.trim().to_string()).unwrap_or_default();
        song.track = tag.track();
        song.disc = tag.disk();
        song.genre = text(tag.genre());
        let original = tag.get_string(ItemKey::OriginalReleaseDate).and_then(|d| d.get(..4)?.parse::<u16>().ok());
        song.year = original.or_else(|| tag.date().map(|d| d.year)).filter(|&y| y > 0);
        song.picture = !tag.pictures().is_empty();
    }
    // iTunes keeps the art outside the file sometimes; a picture in any tag counts.
    song.picture |= tf.tags().iter().any(|t| !t.pictures().is_empty());
    Ok(song)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(path: &str, album: &str, album_artist: &str, artist: &str, track: u32, origin: Origin) -> Song {
        let mut s = fallback_song(Path::new(path), origin, 1, 1, 1);
        s.album = album.into();
        s.album_artist = album_artist.into();
        s.artist = artist.into();
        s.track = Some(track);
        s.title = format!("t{track}");
        s
    }

    #[test]
    fn groups_by_album_and_album_artist_across_folders() {
        let songs = vec![
            song("/m/Biograph CD2/01.mp3", "Biograph", "Bob Dylan", "Bob Dylan", 1, Origin::Misc),
            song("/m/Biograph CD1/02.mp3", "Biograph", "Bob Dylan", "Bob Dylan", 2, Origin::Misc),
            song("/m/Biograph CD1/01.mp3", "Biograph", "bob dylan", "Bob Dylan", 1, Origin::Misc),
        ];
        let lib = Library::build(songs, &mut ScanStats::default());
        assert_eq!(lib.albums.len(), 1);
        assert_eq!(lib.albums[0].dirs.len(), 2);
        assert_eq!(lib.artists.len(), 1);
    }

    #[test]
    fn no_album_artist_groups_by_folder_and_names_various() {
        let songs = vec![
            song("/m/Mix/01.mp3", "Now 5", "", "A", 1, Origin::Misc),
            song("/m/Mix/02.mp3", "Now 5", "", "B", 2, Origin::Misc),
            song("/m/Other/01.mp3", "Now 5", "", "C", 1, Origin::Misc),
        ];
        let lib = Library::build(songs, &mut ScanStats::default());
        assert_eq!(lib.albums.len(), 2);
        let mix = lib.albums.iter().find(|a| a.dirs[0].ends_with("Mix")).unwrap();
        assert_eq!(mix.artist, "Various Artists");
        assert_eq!(mix.songs.len(), 2);
    }

    #[test]
    fn untagged_album_artist_joins_its_folder_mates() {
        let songs = vec![
            song("/m/Dido/No Angel/01.m4a", "No Angel", "Dido", "Dido", 1, Origin::Misc),
            song("/m/Dido/No Angel/02.m4a", "No Angel", "", "Dido", 2, Origin::Misc),
        ];
        let lib = Library::build(songs, &mut ScanStats::default());
        assert_eq!(lib.albums.len(), 1);
        assert_eq!(lib.albums[0].songs.len(), 2);
    }

    #[test]
    fn flac_copy_hides_the_misc_one() {
        let songs = vec![
            song("/f/R/OK/01.flac", "OK Computer", "Radiohead", "Radiohead", 1, Origin::Flac),
            song("/m/Radiohead - OK Computer/01.m4a", "OK Computer", "Radiohead", "Radiohead", 1, Origin::Misc),
        ];
        let mut st = ScanStats::default();
        let lib = Library::build(songs, &mut st);
        assert_eq!(lib.albums.len(), 1);
        assert_eq!(lib.albums[0].origin, Origin::Flac);
        assert_eq!(st.hidden_duplicates, 1);
    }

    #[test]
    fn tracks_in_disc_then_track_order_and_artists_sorted() {
        let mut a = song("/f/x/2.01.flac", "X", "The Beatles", "The Beatles", 1, Origin::Flac);
        a.disc = Some(2);
        let mut b = song("/f/x/1.02.flac", "X", "The Beatles", "The Beatles", 2, Origin::Flac);
        b.disc = Some(1);
        let c = song("/f/y/01.flac", "Y", "ABBA", "ABBA", 1, Origin::Flac);
        let lib = Library::build(vec![a, b, c], &mut ScanStats::default());
        assert_eq!(lib.albums[0].artist, "ABBA");
        let x = &lib.albums[1];
        assert_eq!(x.discs, 2);
        assert_eq!(lib.songs[x.songs[0]].disc, Some(1));
    }

    #[test]
    fn norm_and_sort_name() {
        assert_eq!(norm("  Björk’s  Song "), "bjork's song");
        assert_eq!(sort_name("The Smiths"), "smiths");
        assert_eq!(sort_name("The"), "the");
    }
}

/// Extra tags for a song's details view, as (label, value), in a fixed
/// order: what the library index doesn't keep. Unknown or empty ones are
/// left out.
pub fn song_details(path: &Path) -> Vec<(String, String)> {
    let opts = lofty::config::ParseOptions::new().parsing_mode(lofty::config::ParsingMode::Relaxed);
    let Ok(tf) = lofty::probe::Probe::open(path).and_then(|pr| pr.options(opts).read()) else { return Vec::new() };
    let Some(tag) = tf.primary_tag().or_else(|| tf.first_tag()) else { return Vec::new() };
    let get = |k: ItemKey| tag.get_string(k).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    // Free-form keys (Vorbis comments we write ourselves): FLAC only.
    let flac_tags = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("flac"))
        .then(|| crate::flacfile::read_tags(path).ok())
        .flatten();
    let free = |name: &str| flac_tags.as_ref().and_then(|f| f.tag(name)).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let mut out = Vec::new();
    let mut add = |label: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push((label.to_string(), v));
        }
    };
    add("Composer", get(ItemKey::Composer));
    add("Conductor", get(ItemKey::Conductor));
    add("Label", get(ItemKey::Label));
    add("Catalogue number", get(ItemKey::CatalogNumber));
    add("Release date", get(ItemKey::RecordingDate).or_else(|| get(ItemKey::Year)));
    add("Original date", get(ItemKey::OriginalReleaseDate));
    add("Release country", free("RELEASECOUNTRY"));
    add("ISRC", get(ItemKey::Isrc));
    add("AccurateRip", free("ACCURATERIP"));
    add("CUETools repair", free("CTDBREPAIR"));
    add("MusicBrainz release", get(ItemKey::MusicBrainzReleaseId));
    add("MusicBrainz recording", get(ItemKey::MusicBrainzRecordingId));
    add("Comment", get(ItemKey::Comment));
    out
}
