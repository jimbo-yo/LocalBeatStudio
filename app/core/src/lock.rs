//! Cross-process locks (lock files in the cache folder), so two rips can run
//! at once on different drives without stepping on each other: one lock per
//! drive, one for phone conversion. Released when the `Lock` is dropped or
//! the process exits.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result};

pub struct Lock {
    _file: File,
}

fn open(dir: &Path, name: &str) -> Result<File> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("lock-{name}"));
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

/// `Ok(None)` if another process holds it.
pub fn try_lock(dir: &Path, name: &str) -> Result<Option<Lock>> {
    let file = open(dir, name)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e).context("locking"),
    }
}

/// Waits until it's free.
pub fn lock(dir: &Path, name: &str) -> Result<Lock> {
    let file = open(dir, name)?;
    file.lock().context("locking")?;
    Ok(Lock { _file: file })
}

/// Lock name for a drive path: `\\.\D:` → `D`, `/dev/sr1` → `sr1`.
pub fn drive_lock_name(path: &str) -> String {
    let n: String = path.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let n = n.strip_prefix("dev").unwrap_or(&n).to_string();
    if n.is_empty() { "drive".into() } else { n }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused_until_released() {
        let dir = std::env::temp_dir().join(format!("ripshelf-lock-{}", std::process::id()));
        let a = try_lock(&dir, "D").unwrap();
        assert!(a.is_some());
        assert!(try_lock(&dir, "D").unwrap().is_none());
        assert!(try_lock(&dir, "E").unwrap().is_some());
        drop(a);
        assert!(try_lock(&dir, "D").unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names() {
        assert_eq!(drive_lock_name(r"\\.\D:"), "D");
        assert_eq!(drive_lock_name("/dev/sr1"), "sr1");
    }
}
