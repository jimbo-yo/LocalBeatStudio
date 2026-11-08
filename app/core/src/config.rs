//! Settings: where things go, and per-drive read offsets.
//!
//! Folder names match the shell scripts (`rip-cd` under the OS's config and
//! cache folders), so on Linux the existing offsets file is used as is.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Settings {
    /// FLAC library: `<flac_dir>/<Artist>/<Album>/...`.
    pub flac_dir: PathBuf,
    /// Phone (Opus) copies.
    pub phone_dir: PathBuf,
    /// Settings and the offsets file.
    pub config_dir: PathBuf,
    /// Rejected reads, logs.
    pub cache_dir: PathBuf,
    /// Preferred release country ("US"); None for no preference.
    pub country: Option<String>,
    /// Reads per track before giving up.
    pub reads: u32,
    /// FLAC compression level.
    pub compression: u32,
    /// Sectors read elsewhere before a re-read, to empty the drive cache.
    pub flush_sectors: u32,
}

impl Settings {
    /// OS defaults; `FLAC_DIR`, `PHONE_DIR` and `COUNTRY` environment
    /// variables override, as in the scripts.
    pub fn load() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let music = dirs::audio_dir().unwrap_or_else(|| home.join("Music"));
        let env_path = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Settings {
            flac_dir: env_path("FLAC_DIR").unwrap_or_else(|| music.join("FLAC")),
            phone_dir: env_path("PHONE_DIR").unwrap_or_else(|| music.join("phone")),
            config_dir: dirs::config_dir().unwrap_or_else(|| home.join(".config")).join("rip-cd"),
            cache_dir: dirs::cache_dir().unwrap_or_else(|| home.join(".cache")).join("rip-cd"),
            country: match std::env::var("COUNTRY") {
                Ok(c) if c.is_empty() => None,
                Ok(c) => Some(c),
                Err(_) => Some("US".into()),
            },
            reads: 4,
            compression: 5,
            flush_sectors: 1000,
        }
    }

    pub fn offsets_file(&self) -> PathBuf {
        self.config_dir.join("offsets")
    }

    pub fn rejected_dir(&self) -> PathBuf {
        self.cache_dir.join("rejected")
    }
}

/// Offsets file: lines of `<drive id> <offset>`; later lines win.
///
/// The drive id is udev's ID_SERIAL on Linux (`hp_DVDRAM_GTA0N_4234...-0:0`)
/// and the same without `-0:0` on Windows, so ids are compared without that
/// suffix. A bare serial (ID_SERIAL_SHORT, as rip-cd.sh also accepts)
/// matches too.
pub fn lookup_offset(file: &Path, key: &str, serial: &str) -> Option<i32> {
    let text = std::fs::read_to_string(file).ok()?;
    let want = strip_lun(key);
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let id = it.next()?;
            let off: i32 = it.next()?.parse().ok()?;
            let id = strip_lun(id);
            (id == want || (!serial.is_empty() && id == serial)).then_some(off)
        })
        .last()
}

pub fn save_offset(file: &Path, key: &str, offset: i32) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let old = std::fs::read_to_string(file).unwrap_or_default();
    let want = strip_lun(key);
    let mut out: String = old
        .lines()
        .filter(|l| l.split_whitespace().next().map(strip_lun) != Some(want))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{key} {offset}\n"));
    let tmp = file.with_extension("new");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))?;
    Ok(())
}

/// Drops a SCSI LUN suffix like `-0:0`.
fn strip_lun(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((head, tail)) if tail.contains(':') && tail.chars().all(|c| c.is_ascii_digit() || c == ':') => head,
        _ => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_across_oses() {
        let dir = std::env::temp_dir().join(format!("ripshelf-offsets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("offsets");
        std::fs::write(
            &f,
            "HL-DT-ST_DVDRW_GX50N_KZ4F9KI4627-0:0 6\nhp_DVDRAM_GTA0N_423442525150313439373432-0:0 -1151\nhp_DVDRAM_GTA0N_423442525150313439373432-0:0 102\nKZ4F9KI4627 6\n",
        )
        .unwrap();
        // Windows key (no -0:0) finds the Linux entry; the later line wins.
        assert_eq!(lookup_offset(&f, "hp_DVDRAM_GTA0N_423442525150313439373432", ""), Some(102));
        // Serial-only line.
        assert_eq!(lookup_offset(&f, "Apple_SuperDrive_KZ4F9KI4627", "KZ4F9KI4627"), Some(6));
        assert_eq!(lookup_offset(&f, "Other_Drive", "X"), None);

        save_offset(&f, "hp_DVDRAM_GTA0N_423442525150313439373432", 103).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text.matches("GTA0N").count(), 1, "old entries for the drive replaced: {text}");
        assert!(text.contains("HL-DT-ST_DVDRW_GX50N_KZ4F9KI4627-0:0 6"));
        assert_eq!(lookup_offset(&f, "hp_DVDRAM_GTA0N_423442525150313439373432-0:0", ""), Some(103));
        std::fs::remove_dir_all(&dir).ok();
    }
}
