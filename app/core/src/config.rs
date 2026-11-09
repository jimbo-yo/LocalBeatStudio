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
    /// Everything not ripped with ripshelf, as is (old AAC/MP3 libraries...):
    /// synced to the phone like the FLAC library, which wins for an album in both.
    pub misc_dir: PathBuf,
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
    /// Opus bitrate for phone copies.
    pub opus_kbps: u32,
}

impl Settings {
    /// OS defaults; `FLAC_DIR`, `PHONE_DIR`, `MISC_DIR` and `COUNTRY` environment
    /// variables override, as in the scripts.
    pub fn load() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let music = dirs::audio_dir().unwrap_or_else(|| home.join("Music"));
        let env_path = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Settings {
            flac_dir: env_path("FLAC_DIR").unwrap_or_else(|| music.join("FLAC")),
            phone_dir: env_path("PHONE_DIR").unwrap_or_else(|| music.join("phone")),
            misc_dir: env_path("MISC_DIR").unwrap_or_else(|| music.join("Miscellaneous")),
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
            opus_kbps: crate::phone_copy::DEFAULT_BITRATE_KBPS,
        }
    }

    pub fn offsets_file(&self) -> PathBuf {
        self.config_dir.join("offsets")
    }

    pub fn rejected_dir(&self) -> PathBuf {
        self.cache_dir.join("rejected")
    }
}

/// Choices made in the app (checkboxes), kept in `<config_dir>/ripshelf.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    /// Phone notification when a drive has waited this long for the next disc.
    pub notify_phone: bool,
    pub notify_after_secs: u64,
    /// Phone sync: USB (adb) rather than Wi-Fi (KDE Connect).
    pub phone_usb: bool,
    /// Phone sync: always leave this much free on the phone.
    pub reserve_gb: f64,
    /// The last free-space reading over USB: (total, free, music bytes,
    /// date), for estimating over Wi-Fi, which can't tell.
    pub last_space: Option<(u64, u64, u64, String)>,
    /// AcoustID application key, for identifying songs by sound (enrich).
    pub acoustid_key: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { notify_phone: false, notify_after_secs: 60, phone_usb: true, reserve_gb: 5.0, last_space: None, acoustid_key: String::new() }
    }
}

impl Prefs {
    fn file(s: &Settings) -> PathBuf {
        s.config_dir.join("ripshelf.json")
    }

    pub fn load(s: &Settings) -> Self {
        let d = Prefs::default();
        let Some(v) = std::fs::read_to_string(Self::file(s)).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            return d;
        };
        Prefs {
            notify_phone: v.get("notify_phone").and_then(|x| x.as_bool()).unwrap_or(d.notify_phone),
            notify_after_secs: v.get("notify_after_secs").and_then(|x| x.as_u64()).unwrap_or(d.notify_after_secs),
            phone_usb: v.get("phone_usb").and_then(|x| x.as_bool()).unwrap_or(d.phone_usb),
            reserve_gb: v.get("reserve_gb").and_then(|x| x.as_f64()).unwrap_or(d.reserve_gb),
            acoustid_key: v.get("acoustid_key").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            last_space: v.get("last_space").and_then(|x| {
                Some((x["total"].as_u64()?, x["free"].as_u64()?, x["music"].as_u64()?, x["date"].as_str()?.to_string()))
            }),
        }
    }

    /// Saves these values, keeping any other keys already in the file.
    pub fn save(&self, s: &Settings) -> Result<()> {
        let f = Self::file(s);
        let mut v = std::fs::read_to_string(&f)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        v["notify_phone"] = self.notify_phone.into();
        v["notify_after_secs"] = self.notify_after_secs.into();
        v["phone_usb"] = self.phone_usb.into();
        v["reserve_gb"] = self.reserve_gb.into();
        v["acoustid_key"] = self.acoustid_key.clone().into();
        v["last_space"] = match &self.last_space {
            Some((total, free, music, date)) => serde_json::json!({"total": total, "free": free, "music": music, "date": date}),
            None => serde_json::Value::Null,
        };
        std::fs::create_dir_all(&s.config_dir).with_context(|| format!("creating {}", s.config_dir.display()))?;
        let tmp = f.with_extension("new");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &f).with_context(|| format!("replacing {}", f.display()))?;
        Ok(())
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
    fn prefs_round_trip_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("ripshelf-prefs-{}", std::process::id()));
        let mut s = Settings::load();
        s.config_dir = dir.clone();
        assert_eq!(Prefs::load(&s), Prefs::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ripshelf.json"), "{\"other\": 1}").unwrap();
        let p = Prefs {
            notify_phone: true,
            notify_after_secs: 90,
            phone_usb: false,
            reserve_gb: 7.5,
            last_space: Some((117, 19, 1, "2026-11-08".into())),
            acoustid_key: "abc".into(),
        };
        p.save(&s).unwrap();
        assert_eq!(Prefs::load(&s), p);
        assert!(std::fs::read_to_string(dir.join("ripshelf.json")).unwrap().contains("\"other\": 1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
