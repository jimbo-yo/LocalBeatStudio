//! Phone sync: which albums go on the phone, made as Opus at sync time from
//! the FLAC library or the Miscellaneous library (nothing stored in between).
//!
//! - Library: every folder with audio under `flac_dir` or `misc_dir` is an
//!   album. Albums are matched by a normalised name ([`norm_key`]), so the
//!   same album in both libraries, or in an old `Artist - Album` folder on the
//!   phone, is one album; the FLAC one wins.
//! - The phone's music folder holds a record, [`MANIFEST`], of what each
//!   file was made from (source album + a fingerprint of each source file:
//!   size and modification time, which a re-tag changes). Files whose source
//!   is unchanged aren't sent again.
//! - Albums put on by the old Phone Sync (no record) are adopted as they are
//!   when their file names match what this sync would make; otherwise (e.g. a
//!   re-rip with new file names) the phone copy is replaced.
//! - Only the phone's music folder is touched, and only albums: loose files
//!   in it are left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::{audio_source, phone_copy};

/// The sync's record, in the phone's music folder.
pub const MANIFEST: &str = ".ripshelf-sync.json";

const COVER_FILES: [&str; 4] = ["cover.jpg", "cover.png", "folder.jpg", "folder.png"];
/// FLAC -> Opus 160 kbps is about this much of the FLAC's size (measured:
/// OK Computer 359 MB -> 63 MB).
const FLAC_TO_OPUS: f64 = 0.18;

// ---------------------------------------------------------------- the phone

/// The phone's music folder, however it's reached (USB, Wi-Fi, a test folder).
/// Paths are relative to it, with `/` separators.
pub trait PhoneFs: Sync {
    /// Every file (relative path, size).
    fn list(&self) -> Result<BTreeMap<String, u64>>;
    /// A small file, or None if it doesn't exist.
    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>>;
    fn write(&self, rel: &str, data: &[u8]) -> Result<()>;
    /// Copies a local file to `rel` (creating folders as needed).
    fn push(&self, local: &Path, rel: &str) -> Result<()>;
    fn remove_file(&self, rel: &str) -> Result<()>;
    /// Removes an album folder (and its parent if that's left empty).
    fn remove_dir(&self, rel: &str) -> Result<()>;
    /// (total, free) bytes, if the connection can tell.
    fn space(&self) -> Result<Option<(u64, u64)>>;
}

/// A phone reachable as a normal folder (KDE Connect's mount), or a test
/// folder standing in for one.
pub struct FolderPhone {
    pub root: PathBuf,
}

fn check_rel(rel: &str) -> Result<()> {
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|p| p == ".." || p.is_empty()) {
        return Err(anyhow!("refusing an odd phone path: {rel:?}"));
    }
    Ok(())
}

impl PhoneFs for FolderPhone {
    fn list(&self) -> Result<BTreeMap<String, u64>> {
        let mut out = BTreeMap::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for e in walkdir::WalkDir::new(&self.root).into_iter().filter_map(|e| e.ok()) {
            if !e.file_type().is_file() || e.file_name().to_string_lossy().ends_with(".part") {
                continue;
            }
            let rel = e.path().strip_prefix(&self.root).unwrap_or(e.path());
            let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            out.insert(rel, e.metadata().map(|m| m.len()).unwrap_or(0));
        }
        Ok(out)
    }

    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        check_rel(rel)?;
        match std::fs::read(self.root.join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {rel}")),
        }
    }

    fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        check_rel(rel)?;
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("part");
        std::fs::write(&tmp, data).with_context(|| format!("writing {rel}"))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("writing {rel}"))?;
        Ok(())
    }

    fn push(&self, local: &Path, rel: &str) -> Result<()> {
        check_rel(rel)?;
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = PathBuf::from(format!("{}.part", p.display()));
        std::fs::copy(local, &tmp).with_context(|| format!("copying to {rel}"))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("copying to {rel}"))?;
        Ok(())
    }

    fn remove_file(&self, rel: &str) -> Result<()> {
        check_rel(rel)?;
        match std::fs::remove_file(self.root.join(rel)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {rel}")),
            _ => Ok(()),
        }
    }

    fn remove_dir(&self, rel: &str) -> Result<()> {
        check_rel(rel)?;
        let p = self.root.join(rel);
        if p.exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("removing {rel}"))?;
        }
        if let Some(parent) = p.parent().filter(|d| *d != self.root) {
            let _ = std::fs::remove_dir(parent); // only if empty
        }
        Ok(())
    }

    fn space(&self) -> Result<Option<(u64, u64)>> {
        Ok(None)
    }
}

/// A connection that can be re-established (the phone slept, Wi-Fi dropped).
pub trait Reconnect {
    fn reconnect(&self) -> Result<()>;
}

/// Wraps a phone connection so a failed operation waits for the phone to come
/// back (reconnecting every 5 s) for up to `wait`, then retries it. Every
/// [`PhoneFs`] operation is safe to repeat, so retrying can't leave a mess.
pub struct Patient<'a, P> {
    pub phone: &'a P,
    pub wait: std::time::Duration,
    /// Told when the phone stops answering (to say "wake it up").
    pub on_wait: &'a (dyn Fn(&str) + Sync),
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl<P: PhoneFs + Reconnect + Sync> Patient<'_, P> {
    fn retry<T>(&self, mut op: impl FnMut() -> Result<T>) -> Result<T> {
        let mut give_up = None;
        loop {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let deadline = *give_up.get_or_insert_with(|| std::time::Instant::now() + self.wait);
                    if std::time::Instant::now() >= deadline || (self.cancelled)() {
                        return Err(e.context("the phone stopped responding; sync again when it's awake (it continues where it left off)"));
                    }
                    (self.on_wait)("The phone isn't responding (asleep?). Wake it up to continue...");
                    for _ in 0..10 {
                        if (self.cancelled)() {
                            return Err(e);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    let _ = self.phone.reconnect();
                }
            }
        }
    }
}

impl<P: PhoneFs + Reconnect + Sync> PhoneFs for Patient<'_, P> {
    fn list(&self) -> Result<BTreeMap<String, u64>> {
        self.retry(|| self.phone.list())
    }
    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        self.retry(|| self.phone.read(rel))
    }
    fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        self.retry(|| self.phone.write(rel, data))
    }
    fn push(&self, local: &Path, rel: &str) -> Result<()> {
        self.retry(|| self.phone.push(local, rel))
    }
    fn remove_file(&self, rel: &str) -> Result<()> {
        self.retry(|| self.phone.remove_file(rel))
    }
    fn remove_dir(&self, rel: &str) -> Result<()> {
        self.retry(|| self.phone.remove_dir(rel))
    }
    fn space(&self) -> Result<Option<(u64, u64)>> {
        self.phone.space()
    }
}

impl Reconnect for FolderPhone {
    fn reconnect(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- names

/// Matching key for album folders: case, curly/straight quotes, `？`, and
/// `Artist/Album` vs `Artist - Album` don't matter.
pub fn norm_key(rel: &str) -> String {
    let mut s = rel.to_lowercase();
    for (a, b) in [("’", "'"), ("‘", "'"), ("“", "\""), ("”", "\""), ("？", "?"), ("∶", ":")] {
        s = s.replace(a, b);
    }
    let s = s.replace('/', " - ");
    let words: Vec<&str> = s.split_whitespace().collect();
    words.join(" ")
}

/// ("Artist", "Album") from `Artist/Album` or `Artist - Album`.
pub fn split_album(rel: &str) -> (String, String) {
    if let Some((a, b)) = rel.split_once('/') {
        (a.to_string(), b.to_string())
    } else if let Some((a, b)) = rel.split_once(" - ") {
        (a.to_string(), b.to_string())
    } else {
        (String::new(), rel.to_string())
    }
}

fn is_audio_name(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    audio_source::AUDIO_EXTENSIONS.contains(&ext.as_str()) || ext == "m4p"
}

fn is_cover_name(name: &str) -> bool {
    COVER_FILES.contains(&name.to_lowercase().as_str())
}

// ---------------------------------------------------------------- library

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Flac,
    Misc,
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    /// File name on the phone (`.opus` for audio; covers keep their name).
    pub phone_name: String,
    /// Audio to convert (false: a cover image, copied).
    pub convert: bool,
    pub size: u64,
    /// Size and modification time: changes when the file is re-tagged.
    pub fingerprint: String,
    /// Estimated size on the phone.
    pub est_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub key: String,
    pub origin: Origin,
    /// Folder relative to its library (`Artist/Album`), also its phone folder.
    pub rel: String,
    pub artist: String,
    pub title: String,
    pub files: Vec<SourceFile>,
    /// DRM-protected files (names): never sent.
    pub protected: Vec<String>,
    /// For a FLAC album: the same album's folder in Miscellaneous, if any.
    pub also_in_misc: Option<String>,
}

impl Album {
    /// Recorded on the phone as where an album came from.
    pub fn source_label(&self) -> String {
        match self.origin {
            Origin::Flac => format!("flac:{}", self.rel),
            Origin::Misc => format!("misc:{}", self.rel),
        }
    }

    pub fn est_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.est_bytes).sum()
    }

    pub fn track_count(&self) -> usize {
        self.files.iter().filter(|f| f.convert).count()
    }
}

/// All albums in the two libraries, FLAC winning over Miscellaneous.
pub fn scan_library(flac_dir: &Path, misc_dir: &Path) -> Vec<Album> {
    let mut by_key: BTreeMap<String, Album> = BTreeMap::new();
    for (root, origin) in [(misc_dir, Origin::Misc), (flac_dir, Origin::Flac)] {
        for album in scan_root(root, origin) {
            match by_key.get(&album.key) {
                Some(old) if old.origin == Origin::Misc && origin == Origin::Flac => {
                    let misc = old.rel.clone();
                    by_key.insert(album.key.clone(), Album { also_in_misc: Some(misc), ..album });
                }
                Some(_) => {} // two folders with the same name in one library: first wins
                None => {
                    by_key.insert(album.key.clone(), album);
                }
            }
        }
    }
    by_key.into_values().collect()
}

fn scan_root(root: &Path, origin: Origin) -> Vec<Album> {
    let mut dirs: BTreeMap<PathBuf, Vec<walkdir::DirEntry>> = BTreeMap::new();
    if !root.is_dir() {
        return Vec::new();
    }
    for e in walkdir::WalkDir::new(root).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() {
            if let Some(parent) = e.path().parent() {
                dirs.entry(parent.to_path_buf()).or_default().push(e);
            }
        }
    }
    let mut albums = Vec::new();
    for (dir, entries) in dirs {
        let Ok(rel_path) = dir.strip_prefix(root) else { continue };
        if rel_path.as_os_str().is_empty() {
            continue; // loose files at the top of a library
        }
        let rel = rel_path.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        let mut files = Vec::new();
        let mut protected = Vec::new();
        let mut cover = None;
        for e in &entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let fingerprint = format!("{size}:{mtime}");
            if audio_source::is_protected(e.path()) {
                protected.push(name);
            } else if is_audio_name(&name) {
                let is_flac = name.to_lowercase().ends_with(".flac");
                let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name.clone());
                files.push(SourceFile {
                    path: e.path().to_path_buf(),
                    phone_name: format!("{stem}.opus"),
                    convert: true,
                    size,
                    fingerprint,
                    // Lossy sources are capped at their own bitrate: assume
                    // they stay the same size (errs on the safe side).
                    est_bytes: if is_flac { (size as f64 * FLAC_TO_OPUS) as u64 } else { size },
                });
            } else if is_cover_name(&name) && cover.is_none() {
                cover = Some(SourceFile {
                    path: e.path().to_path_buf(),
                    phone_name: name,
                    convert: false,
                    size,
                    fingerprint,
                    est_bytes: size,
                });
            }
        }
        if files.is_empty() && protected.is_empty() {
            continue;
        }
        files.sort_by(|a, b| a.phone_name.cmp(&b.phone_name));
        if !files.is_empty() {
            files.extend(cover);
        }
        let (artist, title) = split_album(&rel);
        albums.push(Album { key: norm_key(&rel), origin, rel, artist, title, files, protected, also_in_misc: None });
    }
    albums
}

// ---------------------------------------------------------------- phone state

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManifestAlbum {
    /// [`Album::source_label`] of what this phone album was made from.
    pub source: String,
    /// Phone file name -> fingerprint of the source file it was made from.
    pub files: BTreeMap<String, String>,
}

/// Phone album folder -> what it was made from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    pub albums: BTreeMap<String, ManifestAlbum>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Self {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else { return Self::default() };
        let mut m = Manifest::default();
        for (dir, a) in v["albums"].as_object().into_iter().flatten() {
            let files = a["files"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            m.albums.insert(dir.clone(), ManifestAlbum { source: a["source"].as_str().unwrap_or("").into(), files });
        }
        m
    }

    pub fn to_json(&self) -> Vec<u8> {
        let albums: serde_json::Map<String, Value> = self
            .albums
            .iter()
            .map(|(d, a)| (d.clone(), json!({ "source": a.source, "files": a.files })))
            .collect();
        let v = json!({
            "about": "Written by ripshelf's phone sync: what each album folder was made from. Safe to delete (the next sync then re-checks everything).",
            "version": 1,
            "albums": albums,
        });
        let mut out = serde_json::to_vec_pretty(&v).unwrap_or_default();
        out.push(b'\n');
        out
    }
}

#[derive(Debug, Clone)]
pub struct PhoneAlbum {
    pub rel: String,
    pub key: String,
    /// File name -> size.
    pub files: BTreeMap<String, u64>,
}

impl PhoneAlbum {
    pub fn bytes(&self) -> u64 {
        self.files.values().sum()
    }
    pub fn audio_names(&self) -> BTreeSet<&str> {
        self.files.keys().map(|s| s.as_str()).filter(|n| is_audio_name(n)).collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PhoneState {
    pub albums: Vec<PhoneAlbum>,
    pub manifest: Manifest,
    /// (total, free) if known.
    pub space: Option<(u64, u64)>,
}

impl PhoneState {
    pub fn album(&self, key: &str) -> Option<&PhoneAlbum> {
        self.albums.iter().find(|a| a.key == key)
    }
    pub fn music_bytes(&self) -> u64 {
        self.albums.iter().map(|a| a.bytes()).sum()
    }
}

/// Reads the phone's albums (folders with audio files) and the record.
pub fn read_phone(fs: &dyn PhoneFs) -> Result<PhoneState> {
    let files = fs.list()?;
    let mut dirs: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for (rel, size) in files {
        let Some((dir, name)) = rel.rsplit_once('/') else { continue }; // loose files: not ours
        if dir.split('/').any(|p| p.starts_with('.')) {
            continue; // .thumbnails etc.
        }
        dirs.entry(dir.to_string()).or_default().insert(name.to_string(), size);
    }
    let albums = dirs
        .into_iter()
        .filter(|(_, f)| f.keys().any(|n| is_audio_name(n)))
        .map(|(rel, files)| PhoneAlbum { key: norm_key(&rel), rel, files })
        .collect();
    let manifest = fs.read(MANIFEST)?.map(|b| Manifest::parse(&b)).unwrap_or_default();
    Ok(PhoneState { albums, manifest, space: fs.space().unwrap_or(None) })
}

// ---------------------------------------------------------------- planning

/// What a sync would do to one album.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumAction {
    /// Nothing to do (on the phone and current, or not wanted and not there).
    None,
    Add,
    /// Some files new or changed.
    Update { files: usize },
    /// The phone copy came from something else (e.g. re-ripped): swap it.
    Replace,
    /// On the phone from the old Phone Sync with matching names: just record it.
    Adopt,
    Remove,
}

#[derive(Debug, Clone)]
pub struct Push {
    pub key: String,
    /// Phone album folder.
    pub dir: String,
    pub file: SourceFile,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Album folders to remove first (unchecked, or replaced).
    pub remove_dirs: Vec<(String, u64)>,
    /// Single stale files (a renamed track) to remove.
    pub remove_files: Vec<(String, u64)>,
    /// Phone folder -> record entry for albums adopted as they are.
    pub adopt: Vec<(String, ManifestAlbum)>,
    /// Record entries for current albums whose source label changed only
    /// cosmetically (nothing to send).
    pub pushes: Vec<Push>,
    /// Per album key, for showing.
    pub actions: BTreeMap<String, AlbumAction>,
    /// For each album key with pushes: (phone folder, source label).
    pub targets: BTreeMap<String, (String, String)>,
}

impl Plan {
    pub fn add_bytes(&self) -> u64 {
        self.pushes.iter().map(|p| p.file.est_bytes).sum()
    }
    pub fn remove_bytes(&self) -> u64 {
        self.remove_dirs.iter().chain(&self.remove_files).map(|(_, b)| b).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.remove_dirs.is_empty() && self.remove_files.is_empty() && self.adopt.is_empty() && self.pushes.is_empty()
    }
}

/// The albums that are on the phone now (keys): the default selection.
pub fn on_phone(library: &[Album], phone: &PhoneState) -> BTreeSet<String> {
    let _ = library;
    phone.albums.iter().map(|a| a.key.clone()).collect()
}

/// Makes the phone hold exactly the `wanted` albums (by key). Albums on the
/// phone that aren't in the library stay unless their key is left out of
/// `wanted`.
pub fn plan(library: &[Album], phone: &PhoneState, wanted: &BTreeSet<String>) -> Plan {
    let mut p = Plan::default();
    let lib_keys: BTreeSet<&str> = library.iter().map(|a| a.key.as_str()).collect();

    for album in library {
        let on = phone.album(&album.key);
        let want = wanted.contains(&album.key);
        let action = match (want, on) {
            (false, None) => AlbumAction::None,
            (false, Some(pa)) => {
                p.remove_dirs.push((pa.rel.clone(), pa.bytes()));
                AlbumAction::Remove
            }
            (true, None) => {
                push_all(&mut p, album, &album.rel);
                AlbumAction::Add
            }
            (true, Some(pa)) => plan_present(&mut p, album, pa, phone.manifest.albums.get(&pa.rel)),
        };
        if album.files.iter().any(|f| f.convert) || action != AlbumAction::None {
            p.actions.insert(album.key.clone(), action);
        }
    }
    for pa in &phone.albums {
        if !lib_keys.contains(pa.key.as_str()) {
            let action = if wanted.contains(&pa.key) {
                AlbumAction::None
            } else {
                p.remove_dirs.push((pa.rel.clone(), pa.bytes()));
                AlbumAction::Remove
            };
            p.actions.insert(pa.key.clone(), action);
        }
    }
    p
}

fn push_all(p: &mut Plan, album: &Album, dir: &str) {
    for f in &album.files {
        p.pushes.push(Push { key: album.key.clone(), dir: dir.to_string(), file: f.clone() });
    }
    p.targets.insert(album.key.clone(), (dir.to_string(), album.source_label()));
}

fn plan_present(p: &mut Plan, album: &Album, pa: &PhoneAlbum, rec: Option<&ManifestAlbum>) -> AlbumAction {
    let expected: BTreeSet<&str> = album.files.iter().map(|f| f.phone_name.as_str()).collect();
    let expected_audio: BTreeSet<&str> = album.files.iter().filter(|f| f.convert).map(|f| f.phone_name.as_str()).collect();
    let phone_audio = pa.audio_names();

    let replace = |p: &mut Plan| {
        p.remove_dirs.push((pa.rel.clone(), pa.bytes()));
        push_all(p, album, &album.rel);
        AlbumAction::Replace
    };

    match rec {
        Some(rec) if rec.source != album.source_label() => replace(p),
        Some(rec) => {
            let mut changed = 0;
            for f in &album.files {
                let current = pa.files.contains_key(&f.phone_name) && rec.files.get(&f.phone_name) == Some(&f.fingerprint);
                if !current {
                    p.pushes.push(Push { key: album.key.clone(), dir: pa.rel.clone(), file: f.clone() });
                    changed += 1;
                }
            }
            for (name, size) in &pa.files {
                if !expected.contains(name.as_str()) {
                    p.remove_files.push((format!("{}/{name}", pa.rel), *size));
                    changed += 1;
                }
            }
            if changed == 0 {
                AlbumAction::None
            } else {
                p.targets.insert(album.key.clone(), (pa.rel.clone(), album.source_label()));
                AlbumAction::Update { files: changed }
            }
        }
        // No record: put on by the old Phone Sync (or by hand).
        None if !phone_audio.is_empty() && phone_audio.is_subset(&expected_audio) => {
            let mut rec = ManifestAlbum { source: album.source_label(), files: BTreeMap::new() };
            let mut missing = 0;
            for f in &album.files {
                if pa.files.contains_key(&f.phone_name) {
                    rec.files.insert(f.phone_name.clone(), f.fingerprint.clone());
                } else {
                    p.pushes.push(Push { key: album.key.clone(), dir: pa.rel.clone(), file: f.clone() });
                    missing += 1;
                }
            }
            p.adopt.push((pa.rel.clone(), rec));
            if missing > 0 {
                p.targets.insert(album.key.clone(), (pa.rel.clone(), album.source_label()));
                AlbumAction::Update { files: missing }
            } else {
                AlbumAction::Adopt
            }
        }
        None => replace(p),
    }
}

// ---------------------------------------------------------------- doing it

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Removing(String),
    /// Converting/copying a file: (album folder/file, bytes done, bytes to do).
    Sending { file: String, done: u64, total: u64 },
    Failed { file: String, error: String },
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub sent: usize,
    pub removed_albums: usize,
    pub removed_files: usize,
    pub adopted: usize,
    pub failed: Vec<(String, String)>,
    pub stopped: bool,
}

/// Carries out `plan`. Removals first (to make room), then album by album:
/// convert its tracks on all cores into `work_dir`, send them, update the
/// record on the phone. Stops between files when `cancelled()`.
pub fn execute(
    fs: &dyn PhoneFs,
    phone: &PhoneState,
    plan: &Plan,
    work_dir: &Path,
    kbps: u32,
    on: &(dyn Fn(SyncEvent) + Sync),
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<SyncSummary> {
    let mut manifest = phone.manifest.clone();
    let mut sum = SyncSummary::default();
    let save = |m: &Manifest| fs.write(MANIFEST, &m.to_json());

    for (dir, _) in &plan.remove_dirs {
        if cancelled() {
            sum.stopped = true;
            return Ok(sum);
        }
        on(SyncEvent::Removing(dir.clone()));
        fs.remove_dir(dir)?;
        manifest.albums.remove(dir);
        sum.removed_albums += 1;
    }
    for (file, _) in &plan.remove_files {
        on(SyncEvent::Removing(file.clone()));
        fs.remove_file(file)?;
        if let Some((dir, name)) = file.rsplit_once('/') {
            if let Some(rec) = manifest.albums.get_mut(dir) {
                rec.files.remove(name);
            }
        }
        sum.removed_files += 1;
    }
    for (dir, rec) in &plan.adopt {
        manifest.albums.insert(dir.clone(), rec.clone());
        sum.adopted += 1;
    }
    if !plan.remove_dirs.is_empty() || !plan.remove_files.is_empty() || !plan.adopt.is_empty() {
        save(&manifest)?;
    }

    let total = plan.add_bytes().max(1);
    let mut done = 0u64;
    // Album by album, in plan order.
    let mut keys: Vec<&str> = Vec::new();
    for push in &plan.pushes {
        if keys.last() != Some(&push.key.as_str()) && !keys.contains(&push.key.as_str()) {
            keys.push(&push.key);
        }
    }
    std::fs::create_dir_all(work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    for key in keys {
        if cancelled() {
            sum.stopped = true;
            break;
        }
        let pushes: Vec<&Push> = plan.pushes.iter().filter(|p| p.key == key).collect();
        let (dir, source) = plan.targets.get(key).cloned().unwrap_or_else(|| (pushes[0].dir.clone(), String::new()));
        let rec = manifest.albums.entry(dir.clone()).or_insert_with(|| ManifestAlbum { source: source.clone(), files: BTreeMap::new() });
        rec.source = source;

        // Convert this album's tracks in parallel into the work folder.
        let tmp = work_dir.join("album");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let converted = convert_all(&pushes, &tmp, kbps, cancelled);
        for (push, result) in pushes.iter().zip(converted) {
            let rel = format!("{dir}/{}", push.file.phone_name);
            let local = match result {
                Ok(p) => p,
                Err(e) => {
                    let e = format!("{e:#}");
                    on(SyncEvent::Failed { file: rel.clone(), error: e.clone() });
                    sum.failed.push((rel, e));
                    continue;
                }
            };
            if cancelled() {
                sum.stopped = true;
                break;
            }
            on(SyncEvent::Sending { file: rel.clone(), done, total });
            match fs.push(&local, &rel) {
                Ok(()) => {
                    manifest.albums.get_mut(&dir).unwrap().files.insert(push.file.phone_name.clone(), push.file.fingerprint.clone());
                    sum.sent += 1;
                }
                Err(e) => {
                    let _ = save(&manifest);
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(e).with_context(|| format!("sending {rel}"));
                }
            }
            done += push.file.est_bytes;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        save(&manifest)?;
        if sum.stopped {
            break;
        }
    }
    on(SyncEvent::Sending { file: String::new(), done: total, total });
    Ok(sum)
}

/// Converts (or, for covers, points at) each file; results in order.
fn convert_all(pushes: &[&Push], tmp: &Path, kbps: u32, cancelled: &(dyn Fn() -> bool + Sync)) -> Vec<Result<PathBuf>> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let results: Mutex<Vec<Option<Result<PathBuf>>>> = Mutex::new((0..pushes.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(pushes.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(push) = pushes.get(i) else { break };
                    let r = if cancelled() {
                        Err(anyhow!("stopped"))
                    } else if push.file.convert {
                        let out = tmp.join(format!("{i:04}.opus"));
                        phone_copy::convert_file(&push.file.path, &out, kbps).map(|()| out)
                    } else {
                        Ok(push.file.path.clone())
                    };
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    results.into_inner().unwrap().into_iter().map(|r| r.unwrap_or_else(|| Err(anyhow!("not converted")))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flac_encode;

    fn flac(path: &Path, title: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let frames: Vec<u32> = (0..44_100 / 2)
            .map(|i| {
                let v = ((i as f64 / 44_100.0 * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                crate::accuraterip::pack_frame(v, v)
            })
            .collect();
        flac_encode::write_flac(path, &frames, &[("TITLE".into(), title.into())], None, 0).unwrap();
    }

    #[test]
    fn keys_match_old_and_new_folder_styles() {
        assert_eq!(norm_key("Guns N’ Roses/Use Your Illusion I"), norm_key("Guns N' Roses - Use Your Illusion I"));
        assert_eq!(norm_key("Radiohead/I Might Be Wrong∶ Live"), "radiohead - i might be wrong: live");
        assert_eq!(split_album("Hole - Celebrity Skin"), ("Hole".into(), "Celebrity Skin".into()));
    }

    #[test]
    fn sync_adopts_replaces_updates_and_removes() {
        let root = std::env::temp_dir().join(format!("ripshelf-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (fl, misc, ph, work) = (root.join("FLAC"), root.join("Misc"), root.join("phone"), root.join("work"));

        // FLAC library: a re-rip of an album the phone has in an old folder.
        flac(&fl.join("Hole/Celebrity Skin/01. Hole - Celebrity Skin.flac"), "Celebrity Skin");
        flac(&fl.join("Hole/Celebrity Skin/02. Hole - Awful.flac"), "Awful");
        // Miscellaneous: one adoptable album, one new, one only DRM files.
        flac(&misc.join("ABBA/Gold/01 Dancing Queen.flac"), "Dancing Queen");
        flac(&misc.join("ABBA/Gold/02 Knowing Me.flac"), "Knowing Me");
        flac(&misc.join("Aerosmith/Pump/01 Love in an Elevator.flac"), "Elevator");
        std::fs::create_dir_all(misc.join("Chant/Meditation")).unwrap();
        std::fs::write(misc.join("Chant/Meditation/01 Qui manducat.m4p"), b"drm").unwrap();
        // The same album in both libraries: FLAC wins.
        flac(&misc.join("Hole - Celebrity Skin/01 - Hole - Celebrity Skin.flac"), "old");

        // Phone: old Phone Sync copies, no record.
        let old = |rel: &str| {
            std::fs::create_dir_all(ph.join(rel).parent().unwrap()).unwrap();
            std::fs::write(ph.join(rel), b"opus").unwrap();
        };
        old("Hole - Celebrity Skin/01 - Hole - Celebrity Skin.opus"); // old names: replace
        old("ABBA/Gold/01 Dancing Queen.opus"); // matching names: adopt, add track 2
        old("Podcasts/Show/ep1.mp3"); // not in the library: stays unless unchecked
        std::fs::write(ph.join("loose.mp3"), b"x").unwrap(); // loose: never touched

        let lib = scan_library(&fl, &misc);
        let hole = lib.iter().find(|a| a.key == norm_key("Hole/Celebrity Skin")).unwrap();
        assert_eq!((hole.origin, hole.also_in_misc.as_deref()), (Origin::Flac, Some("Hole - Celebrity Skin")));
        assert_eq!(lib.iter().find(|a| a.rel == "Chant/Meditation").unwrap().protected.len(), 1);

        let fs = FolderPhone { root: ph.clone() };
        let phone = read_phone(&fs).unwrap();
        let mut wanted = on_phone(&lib, &phone);
        wanted.insert(norm_key("Aerosmith/Pump"));
        let p = plan(&lib, &phone, &wanted);
        assert_eq!(p.actions[&norm_key("Hole/Celebrity Skin")], AlbumAction::Replace);
        assert_eq!(p.actions[&norm_key("ABBA/Gold")], AlbumAction::Update { files: 1 });
        assert_eq!(p.actions[&norm_key("Aerosmith/Pump")], AlbumAction::Add);
        assert_eq!(p.actions[&norm_key("Podcasts/Show")], AlbumAction::None);

        let sum = execute(&fs, &phone, &p, &work, 160, &|_| {}, &|| false).unwrap();
        assert!(sum.failed.is_empty(), "{:?}", sum.failed);
        assert_eq!((sum.sent, sum.removed_albums, sum.adopted), (4, 1, 1));
        assert!(!ph.join("Hole - Celebrity Skin").exists());
        assert!(std::fs::read(ph.join("Hole/Celebrity Skin/02. Hole - Awful.opus")).unwrap().starts_with(b"OggS"));
        assert!(ph.join("ABBA/Gold/02 Knowing Me.opus").exists());
        assert!(ph.join("loose.mp3").exists() && ph.join("Podcasts/Show/ep1.mp3").exists());

        // Nothing left to do.
        let phone = read_phone(&fs).unwrap();
        assert!(plan(&lib, &phone, &on_phone(&lib, &phone)).is_empty());

        // A re-tag (new mtime) re-sends that file only; unchecking removes.
        let f = fl.join("Hole/Celebrity Skin/02. Hole - Awful.flac");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(later).unwrap();
        let lib = scan_library(&fl, &misc);
        let mut wanted = on_phone(&lib, &phone);
        wanted.remove(&norm_key("Podcasts/Show"));
        let p = plan(&lib, &phone, &wanted);
        assert_eq!(p.actions[&norm_key("Hole/Celebrity Skin")], AlbumAction::Update { files: 1 });
        assert_eq!(p.remove_dirs.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(), ["Podcasts/Show"]);
        execute(&fs, &phone, &p, &work, 160, &|_| {}, &|| false).unwrap();
        assert!(!ph.join("Podcasts").exists());
        let phone = read_phone(&fs).unwrap();
        assert!(plan(&lib, &phone, &on_phone(&lib, &phone)).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
