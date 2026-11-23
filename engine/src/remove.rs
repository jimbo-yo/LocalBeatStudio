//! Taking albums and songs out of the library ("Remove from library" in
//! Studio): on the server and on this computer, so they're gone from every
//! device. Nothing is deleted outright: this computer's files go to the
//! Recycle Bin (the desktop's trash on Linux), the server's to
//! `/srv/music/Removed/<date-time>/` on the Mac (LocalBeatServer's
//! `localbeat-writer-session`, reached with this computer's writer key, so
//! an admin computer on the home network only).
//!
//! The server goes first: if it can't be reached nothing is touched here,
//! else a local album would vanish only for its server copy (hidden while
//! this computer had one) to show up in its place.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use localbeat_core::config::{ServerPrefs, Settings};
use localbeat_core::library::{EXTENSIONS, Origin, Song};
use localbeat_core::outbox::{self, Outbox};

use crate::push::{self, Rsync};

/// What to remove, worked out from the library's songs.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Plan {
    /// This computer's files.
    pub local: Vec<PathBuf>,
    /// The server's, relative to its library folder ("Artist/Album/01. … .flac").
    pub server: Vec<String>,
    /// Server songs whose place on the server isn't known yet (a catalogue
    /// fetched before Studio kept it): can't be removed until the next fetch.
    pub unknown: usize,
}

/// How it went.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub local_removed: usize,
    /// Moved to Removed on the server (the same path in FLAC and
    /// Miscellaneous counts twice).
    pub server_removed: usize,
    /// Asked for but not on the server (never sent there, say).
    pub server_missing: usize,
    pub notes: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.server_removed > 0 {
            parts.push(format!("{} from the server (kept in /srv/music/Removed on the Mac)", files(self.server_removed)));
        }
        if self.local_removed > 0 {
            parts.push(format!("{} from this computer (in the Recycle Bin)", files(self.local_removed)));
        }
        if parts.is_empty() {
            "Nothing was removed".into()
        } else {
            format!("Removed {}", parts.join(" and "))
        }
    }
}

fn files(n: usize) -> String {
    if n == 1 { "1 song".into() } else { format!("{n} songs") }
}

/// `path` relative to `root` with `/` between parts, if it's inside it.
fn rel_to(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty() && !parts.iter().any(|p| p == ".." || p == ".")).then(|| parts.join("/"))
}

/// The files behind `songs`. A local song's server copy is at the same
/// place under the server's folder of the same kind (the library contract:
/// `Artist/Album/NN. Artist - Title.flac`), so it goes too if it's there.
pub fn plan(s: &Settings, songs: &[&Song]) -> Plan {
    let mut p = Plan::default();
    let mut server = BTreeSet::new();
    for x in songs {
        match x.origin {
            Origin::Server => match &x.server_path {
                Some(rel) => {
                    server.insert(rel.clone());
                }
                None => p.unknown += 1,
            },
            Origin::Flac | Origin::Misc => {
                p.local.push(x.path.clone());
                let root = if x.origin == Origin::Flac { &s.flac_dir } else { &s.misc_dir };
                if let Some(rel) = rel_to(&x.path, root) {
                    server.insert(rel);
                }
            }
        }
    }
    p.server = server.into_iter().collect();
    p
}

/// "artist|album" for matching copies of one album: album artist (else
/// artist) and album, case, accents, punctuation, a leading "The" and disc
/// labels ("CD 2") ignored. None without an album name.
fn album_key(x: &Song) -> Option<String> {
    let artist = if x.album_artist.trim().is_empty() { &x.artist } else { &x.album_artist };
    let artist = localbeat_core::enrich::norm(artist);
    let artist = artist.strip_prefix("the ").unwrap_or(&artist).to_string();
    let album = localbeat_core::enrich::norm(&localbeat_core::enrich::clean_album(&x.album));
    if matches!(album.as_str(), "" | "unknown album" | "untitled") {
        return None;
    }
    Some(format!("{artist}|{album}"))
}

/// Where a song is inside its library folder ("Artist/Album/01. ….flac"):
/// the server's path, or a local file's place under FLAC / Miscellaneous.
fn library_rel(s: &Settings, x: &Song) -> Option<String> {
    match x.origin {
        Origin::Server => x.server_path.clone(),
        Origin::Flac => rel_to(&x.path, &s.flac_dir),
        Origin::Misc => rel_to(&x.path, &s.misc_dir),
    }
}

/// After a FLAC album (`album`, its outbox name "Artist/Album") reached the
/// server: the library's other copies of the same album (same album artist
/// and album tags), such as the old MP3/AAC copy in Miscellaneous, as
/// indices into `songs`. The new rip's own songs (here, or the server's
/// copy of them at the same place) never count, and nor does anything at a
/// place the new rip has (the server removes a path from both its library
/// folders).
pub fn old_copies(s: &Settings, songs: &[Song], album: &str) -> Vec<usize> {
    let prefix = format!("{}/", album.trim_end_matches('/'));
    let new: Vec<usize> = (0..songs.len())
        .filter(|&i| {
            let x = &songs[i];
            matches!(x.origin, Origin::Flac | Origin::Server)
                && x.codec == "FLAC"
                && (x.origin == Origin::Flac || x.server_path.is_some())
                && library_rel(s, x).is_some_and(|r| r.starts_with(&prefix))
        })
        .collect();
    let keys: BTreeSet<String> = new.iter().filter_map(|&i| album_key(&songs[i])).collect();
    if keys.is_empty() {
        return Vec::new();
    }
    let taken: BTreeSet<String> = new.iter().filter_map(|&i| library_rel(s, &songs[i])).collect();
    (0..songs.len())
        .filter(|i| !new.contains(i))
        .filter(|&i| album_key(&songs[i]).is_some_and(|k| keys.contains(&k)))
        .filter(|&i| library_rel(s, &songs[i]).is_none_or(|r| !taken.contains(&r)))
        .collect()
}

/// Whether this computer can take songs off the server: it has a writer key
/// (an admin computer, docs/joining.md).
pub fn can_remove_from_server(sp: &ServerPrefs) -> bool {
    sp.enabled()
}

/// Does it: the server first (waiting for its scan, so the catalogue
/// fetched next no longer has them), then this computer.
pub fn remove(s: &Settings, sp: &ServerPrefs, plan: &Plan) -> Result<Report> {
    let mut r = Report::default();
    if plan.unknown > 0 {
        r.notes.push(format!(
            "{} only on the server couldn't be removed: their place there isn't known yet. Try again in a minute (after the library refreshes).",
            files(plan.unknown)
        ));
    }
    // Only paths the server could have: a local song outside the library
    // folders has none.
    if !plan.server.is_empty() {
        if can_remove_from_server(sp) {
            from_server(s, sp, &plan.server, &mut r)?;
        } else if plan.local.is_empty() {
            bail!("only an admin computer can take songs off the server (this one has no writer key)");
        }
    }
    from_here(s, &plan.local, &mut r, &|p: &[&Path]| trash::delete_all(p).map_err(Into::into))?;
    Ok(r)
}

/// Puts files or folders where they can be got back from (the Recycle Bin).
type Discard<'a> = &'a dyn Fn(&[&Path]) -> Result<()>;

fn from_server(s: &Settings, sp: &ServerPrefs, paths: &[String], r: &mut Report) -> Result<()> {
    push::reachable(sp).map_err(|e| anyhow::anyhow!("{e}. Removing works from home only (the server's SSH is open there); nothing was removed"))?;
    let rs = Rsync::find()?;
    let key = rs.path(&sp.key_path());
    let home = dirs::home_dir().unwrap_or_default().join(".ssh").join("known_hosts");
    let files = format!("UserKnownHostsFile=\"{}\" \"{}\"", rs.path(&home), rs.path(&push::known_hosts(s)));
    let (host, port) = push::host_port(sp);
    let mut child = rs
        .ssh()
        .args(["-p", &port.to_string(), "-i", &key])
        .args(["-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", &files, "-o", "ConnectTimeout=10"])
        .arg(format!("{}@{host}", sp.user))
        .arg("localbeat-remove")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    {
        let mut stdin = child.stdin.take().context("ssh's input")?;
        for p in paths {
            writeln!(stdin, "{p}").context("writing to ssh")?;
        }
    }
    let out = child.wait_with_output().context("waiting for ssh")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut answered = false;
    for line in stdout.lines() {
        if line.starts_with("removed ") {
            r.server_removed += 1;
            answered = true;
        } else if line.starts_with("missing ") {
            r.server_missing += 1;
            answered = true;
        } else if let Some(p) = line.strip_prefix("refused ") {
            r.notes.push(format!("the server refused {p}"));
            answered = true;
        }
    }
    if !answered {
        let why = stderr.lines().chain(stdout.lines()).find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        if stderr.contains("rrsync") || why.contains("rsync") {
            bail!("the server doesn't know how to remove songs yet: run host/setup.sh on the Mac (LocalBeatServer). Nothing was removed");
        }
        if stderr.contains("Permission denied") {
            bail!("the server refused this computer's key. Nothing was removed");
        }
        bail!("the server didn't answer ({}). Nothing was removed", if why.is_empty() { format!("ssh exit {:?}", out.status.code()) } else { why.to_string() });
    }
    if r.server_removed > 0 {
        // The catalogue fetched next must no longer have them.
        match localbeat_core::server_library::client(s) {
            Some((sub, _)) => {
                if let Err(e) = push::scan(&sub) {
                    r.notes.push(format!("the server's scan: {e:#} (they disappear at its next rescan, within 10 minutes)"));
                }
            }
            None => r.notes.push("no music server login here: the server drops them at its next rescan (within 10 minutes)".into()),
        }
    }
    Ok(())
}

fn is_music(p: &Path) -> bool {
    p.extension().is_some_and(|x| EXTENSIONS.iter().any(|e| x.eq_ignore_ascii_case(e)))
}

/// Whether `dir` still holds music, at any depth.
fn has_music(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.filter_map(|e| e.ok()).any(|e| {
        let p = e.path();
        if p.is_dir() { has_music(&p) } else { is_music(&p) }
    })
}

fn from_here(s: &Settings, files: &[PathBuf], r: &mut Report, discard: Discard) -> Result<()> {
    let present: Vec<&Path> = files.iter().map(PathBuf::as_path).filter(|p| p.is_file()).collect();
    if present.is_empty() {
        return Ok(());
    }
    discard(&present).context("moving the files to the Recycle Bin (is one of them playing?)")?;
    r.local_removed += present.len();
    // Album folders left with no music: the rest (cover, rip log) goes too,
    // and an artist folder left empty. Never a library folder itself.
    let roots = [&s.flac_dir, &s.misc_dir];
    let dirs: BTreeSet<&Path> = present.iter().filter_map(|p| p.parent()).collect();
    let mut gone = Vec::new();
    for d in dirs {
        if roots.iter().any(|root| d == root.as_path()) || !roots.iter().any(|root| d.starts_with(root)) {
            continue;
        }
        if d.is_dir() && !has_music(d) {
            match discard(&[d]) {
                Ok(()) => gone.push(d.to_path_buf()),
                Err(e) => r.notes.push(format!("couldn't put {} in the Recycle Bin: {e:#}", d.display())),
            }
            if let Some(artist) = d.parent()
                && !roots.iter().any(|root| artist == root.as_path())
                && std::fs::read_dir(artist).is_ok_and(|mut e| e.next().is_none())
            {
                let _ = std::fs::remove_dir(artist);
            }
        }
    }
    // An album gone from here no longer waits to be sent.
    let names: Vec<String> = gone.iter().filter_map(|d| outbox::album_name(s, d).ok()).collect();
    if !names.is_empty() {
        let _ = Outbox::update(s, |o| {
            for n in &names {
                o.forget(n);
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(path: &str, origin: Origin, server_path: Option<&str>) -> Song {
        Song {
            path: path.into(),
            size: 1,
            mtime: 0,
            ctime: 0,
            ctime_nsec: 0,
            tag_schema: 1,
            added: 0,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            album_artist: String::new(),
            track: None,
            disc: None,
            year: None,
            genre: None,
            genres: Vec::new(),
            moods: Vec::new(),
            ai_enriched: None,
            seconds: 0.0,
            codec: "FLAC".into(),
            sample_rate: None,
            bits: None,
            bitrate: None,
            drm: false,
            origin,
            picture: false,
            server_path: server_path.map(str::to_string),
        }
    }

    /// Deletes for real instead of using the Recycle Bin (tests don't fill it).
    fn delete(p: &[&Path]) -> Result<()> {
        for x in p {
            if x.is_dir() { std::fs::remove_dir_all(x)? } else { std::fs::remove_file(x)? }
        }
        Ok(())
    }

    #[test]
    fn plan_finds_both_copies() {
        let mut s = Settings::load();
        s.flac_dir = PathBuf::from("/m/FLAC");
        s.misc_dir = PathBuf::from("/m/Misc");
        let a = song("/m/FLAC/The Beatles/Revolver/01. The Beatles - Taxman.flac", Origin::Flac, None);
        let b = song("/m/Misc/Old/Album/x.mp3", Origin::Misc, None);
        let c = song("/cache/server-songs/a1/s1.flac", Origin::Server, Some("Tori Amos/Boys for Pele/01. Tori Amos - Beauty Queen ／ Horses.flac"));
        let d = song("/cache/server-songs/a1/s2.flac", Origin::Server, None);
        let e = song("/elsewhere/y.flac", Origin::Flac, None);
        let p = plan(&s, &[&a, &b, &c, &d, &e]);
        assert_eq!(p.local.len(), 3);
        assert_eq!(
            p.server,
            [
                "Old/Album/x.mp3",
                "The Beatles/Revolver/01. The Beatles - Taxman.flac",
                "Tori Amos/Boys for Pele/01. Tori Amos - Beauty Queen ／ Horses.flac"
            ]
        );
        assert_eq!(p.unknown, 1);
    }

    fn tagged(path: &str, origin: Origin, server_path: Option<&str>, artist: &str, album: &str, codec: &str) -> Song {
        Song { album_artist: artist.into(), album: album.into(), codec: codec.into(), ..song(path, origin, server_path) }
    }

    #[test]
    fn old_copies_of_a_new_flac_album() {
        let mut s = Settings::load();
        s.flac_dir = PathBuf::from("/m/FLAC");
        s.misc_dir = PathBuf::from("/m/Misc");
        let songs = vec![
            // 0, 1: the new rip, here and its server copy.
            tagged("/m/FLAC/The Beach Boys/Greatest Hits/01. The Beach Boys - Surfin' U.S.A..flac", Origin::Flac, None, "The Beach Boys", "Greatest Hits", "FLAC"),
            tagged("/c/s/a9/s1.flac", Origin::Server, Some("The Beach Boys/Greatest Hits/02. The Beach Boys - I Get Around.flac"), "The Beach Boys", "Greatest Hits", "FLAC"),
            // 2, 3: the old copy on the server (another folder name, no "The").
            tagged("/c/s/a1/s7.mp3", Origin::Server, Some("Beach Boys/Greatest Hits [Disc 1]/01 Surfin USA.mp3"), "Beach Boys", "Greatest Hits (Disc 1)", "MP3"),
            tagged("/c/s/a1/s8.m4a", Origin::Server, Some("Beach Boys/Greatest Hits [Disc 1]/02 I Get Around.m4a"), "Beach Boys", "Greatest Hits", "AAC"),
            // 4: an old copy in this computer's Miscellaneous, in the same folder name.
            tagged("/m/Misc/The Beach Boys/Greatest Hits/03 Help Me Rhonda.mp3", Origin::Misc, None, "The Beach Boys", "Greatest Hits", "MP3"),
            // 5: another artist's album of the same name stays.
            tagged("/c/s/a2/s9.mp3", Origin::Server, Some("Eagles/Greatest Hits/01.mp3"), "Eagles", "Greatest Hits", "MP3"),
            // 6: a server song where the new rip's file is: removing it would take the new one too.
            tagged("/c/s/a3/s5.flac", Origin::Server, Some("The Beach Boys/Greatest Hits/01. The Beach Boys - Surfin' U.S.A..flac"), "The Beach Boys", "Greatest Hits", "FLAC"),
        ];
        assert_eq!(old_copies(&s, &songs, "The Beach Boys/Greatest Hits"), [2, 3, 4]);
        assert!(old_copies(&s, &songs, "Nobody/Nothing").is_empty());
    }

    #[test]
    fn local_album_goes_to_the_trash_with_its_folder() {
        let root = std::env::temp_dir().join(format!("localbeat-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = Settings::load();
        s.config_dir = root.join("config");
        s.cache_dir = root.join("cache");
        s.flac_dir = root.join("FLAC");
        s.misc_dir = root.join("Misc");
        let album = s.flac_dir.join("Artist").join("Album");
        let other = s.flac_dir.join("Artist").join("Other");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&s.misc_dir).unwrap();
        for f in ["01. A - One.flac", "02. A - Two.flac", "cover.jpg", "Album.log"] {
            std::fs::write(album.join(f), b"x").unwrap();
        }
        std::fs::write(other.join("01. A - Else.flac"), b"x").unwrap();
        Outbox::update(&s, |o| o.add("Artist/Album")).unwrap();

        let mut r = Report::default();
        from_here(&s, &[album.join("01. A - One.flac")], &mut r, &delete).unwrap();
        assert_eq!(r.local_removed, 1);
        assert!(album.join("02. A - Two.flac").is_file() && album.join("cover.jpg").is_file(), "the rest of the album stays");
        assert!(Outbox::load(&s).pending().contains(&"Artist/Album".to_string()));

        from_here(&s, &[album.join("02. A - Two.flac")], &mut r, &delete).unwrap();
        assert!(!album.exists(), "no music left: the folder goes too");
        assert!(other.is_dir() && s.flac_dir.is_dir(), "the artist's other album and the library stay");
        assert!(Outbox::load(&s).pending().is_empty(), "no longer waits to be sent");
        std::fs::remove_dir_all(&root).ok();
    }
}
