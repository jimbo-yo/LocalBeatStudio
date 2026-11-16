//! The outbox: ripped albums waiting to go to the LocalBeat server
//! (docs/server-library.md). A finished disc adds its album; the pusher
//! (engine::push) sends what's waiting whenever this computer is on the home
//! network, and marks it sent or failed. Kept in `<config_dir>/outbox.json`,
//! changed under a lock so a rip and the pusher can both use it.
//!
//! Albums are named by their folder relative to the FLAC library
//! (`Bob Dylan/Blood on the Tracks`), with `/` on every OS.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::lock;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum State {
    /// Not sent yet (or changed since it was).
    Waiting,
    /// On the server and checked (`at`: when).
    Sent { at: String },
    /// The last try failed for a reason other than being away from home;
    /// tried again next time.
    Failed { at: String, error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Folder relative to the FLAC library, `/`-separated.
    pub album: String,
    /// When it was (last) added.
    pub added: String,
    #[serde(flatten)]
    pub state: State,
}

impl Entry {
    pub fn pending(&self) -> bool {
        !matches!(self.state, State::Sent { .. })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Outbox {
    pub albums: Vec<Entry>,
}

/// Sent albums are kept this long in the list (so the window can say so), then dropped.
const KEEP_SENT: usize = 200;

fn file(s: &Settings) -> PathBuf {
    s.config_dir.join("outbox.json")
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The album's name in the outbox: its folder relative to the FLAC library.
pub fn album_name(s: &Settings, album_dir: &Path) -> Result<String> {
    let rel = album_dir
        .strip_prefix(&s.flac_dir)
        .with_context(|| format!("{} isn't in the FLAC library ({})", album_dir.display(), s.flac_dir.display()))?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.is_empty() || parts.iter().any(|p| p == ".." || p == ".") {
        bail!("not an album folder: {}", album_dir.display());
    }
    Ok(parts.join("/"))
}

/// The album's folder on this computer.
pub fn album_dir(s: &Settings, album: &str) -> PathBuf {
    album.split('/').fold(s.flac_dir.clone(), |p, c| p.join(c))
}

/// Lock name held while a disc is ripped into `album`, so it isn't pushed half done.
pub fn ripping_lock_name(album: &str) -> String {
    format!("album-{:08x}", crc32fast::hash(album.as_bytes()))
}

impl Outbox {
    pub fn load(s: &Settings) -> Outbox {
        std::fs::read_to_string(file(s)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, s: &Settings) -> Result<()> {
        let f = file(s);
        std::fs::create_dir_all(&s.config_dir).with_context(|| format!("creating {}", s.config_dir.display()))?;
        let tmp = f.with_extension("new");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &f).with_context(|| format!("replacing {}", f.display()))
    }

    /// Changes the outbox under its lock (load, change, save).
    pub fn update<R>(s: &Settings, change: impl FnOnce(&mut Outbox) -> R) -> Result<R> {
        let _lock = lock::lock(&s.cache_dir, "outbox")?;
        let mut o = Outbox::load(s);
        let r = change(&mut o);
        o.save(s)?;
        Ok(r)
    }

    /// Adds `album` as waiting (again, if it was sent: something changed).
    pub fn add(&mut self, album: &str) {
        self.albums.retain(|e| e.album != album);
        self.albums.push(Entry { album: album.to_string(), added: now(), state: State::Waiting });
        // Old sent entries go; pending ones never do.
        let sent = self.albums.iter().filter(|e| !e.pending()).count();
        if sent > KEEP_SENT {
            let mut drop = sent - KEEP_SENT;
            self.albums.retain(|e| {
                if drop > 0 && !e.pending() {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn set(&mut self, album: &str, state: State) {
        if let Some(e) = self.albums.iter_mut().find(|e| e.album == album) {
            e.state = state;
        }
    }

    pub fn sent(&mut self, album: &str) {
        self.set(album, State::Sent { at: now() });
    }

    /// Marks `album` sent if it wasn't queued again since `added` (when the
    /// send started): a disc ripped into it meanwhile means it goes again.
    pub fn sent_if_unchanged(&mut self, album: &str, added: &str) -> bool {
        match self.albums.iter_mut().find(|e| e.album == album) {
            Some(e) if e.added == added => {
                e.state = State::Sent { at: now() };
                true
            }
            _ => false,
        }
    }

    pub fn failed(&mut self, album: &str, error: &str) {
        self.set(album, State::Failed { at: now(), error: error.to_string() });
    }

    /// Albums still to send, oldest first.
    pub fn pending(&self) -> Vec<String> {
        self.albums.iter().filter(|e| e.pending()).map(|e| e.album.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str) -> Settings {
        let dir = std::env::temp_dir().join(format!("localbeat-outbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Settings::load();
        s.config_dir = dir.join("config");
        s.cache_dir = dir.join("cache");
        s.flac_dir = dir.join("FLAC");
        s
    }

    #[test]
    fn names_are_relative_to_the_library() {
        let s = settings("names");
        let dir = s.flac_dir.join("Bob Dylan").join("Blood on the Tracks");
        assert_eq!(album_name(&s, &dir).unwrap(), "Bob Dylan/Blood on the Tracks");
        assert_eq!(album_dir(&s, "Bob Dylan/Blood on the Tracks"), dir);
        assert!(album_name(&s, Path::new("/elsewhere/A/B")).is_err());
        assert!(album_name(&s, &s.flac_dir).is_err());
        assert_ne!(ripping_lock_name("A/B"), ripping_lock_name("A/C"));
    }

    #[test]
    fn add_send_fail_round_trip() {
        let s = settings("states");
        Outbox::update(&s, |o| {
            o.add("A/One");
            o.add("B/Two");
        })
        .unwrap();
        assert_eq!(Outbox::load(&s).pending(), ["A/One", "B/Two"]);

        Outbox::update(&s, |o| {
            o.sent("A/One");
            o.failed("B/Two", "disk full");
        })
        .unwrap();
        let o = Outbox::load(&s);
        assert_eq!(o.pending(), ["B/Two"], "failed ones are tried again");
        assert!(matches!(&o.albums[1].state, State::Failed { error, .. } if error == "disk full"));

        // Ripped again (another disc of the set, a repair): waiting again, once.
        Outbox::update(&s, |o| o.add("A/One")).unwrap();
        let o = Outbox::load(&s);
        assert_eq!(o.pending(), ["B/Two", "A/One"]);
        assert_eq!(o.albums.iter().filter(|e| e.album == "A/One").count(), 1);
        std::fs::remove_dir_all(s.config_dir.parent().unwrap()).ok();
    }

    #[test]
    fn queued_again_while_sending_stays_waiting() {
        let mut o = Outbox::default();
        o.add("A/B");
        let added = o.albums[0].added.clone();
        assert!(o.sent_if_unchanged("A/B", &added));
        o.add("A/B");
        o.albums[0].added = "later".into();
        assert!(!o.sent_if_unchanged("A/B", &added));
        assert_eq!(o.pending(), ["A/B"]);
        assert!(!o.sent_if_unchanged("Not/There", &added));
    }

    #[test]
    fn old_sent_entries_are_dropped_pending_never() {
        let s = settings("trim");
        let mut o = Outbox::default();
        o.add("Waiting/Album");
        for i in 0..KEEP_SENT + 5 {
            let a = format!("Sent/{i}");
            o.add(&a);
            o.sent(&a);
        }
        o.add("Last/One");
        assert_eq!(o.albums.iter().filter(|e| !e.pending()).count(), KEEP_SENT);
        assert_eq!(o.pending(), ["Waiting/Album", "Last/One"]);
        assert!(!o.albums.iter().any(|e| e.album == "Sent/0"), "oldest sent went first");
        let _ = s;
    }

    #[test]
    fn file_format() {
        let mut o = Outbox::default();
        o.add("A/B");
        o.failed("A/B", "x");
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["albums"][0]["album"], "A/B");
        assert_eq!(j["albums"][0]["state"], "failed");
        assert_eq!(j["albums"][0]["error"], "x");
        let back: Outbox = serde_json::from_value(j).unwrap();
        assert_eq!(back, o);
    }
}
