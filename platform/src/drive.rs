//! CD drives: listing, TOC, identity, eject, and raw sector reads (via
//! cd-da-reader) for `localbeat_core::secure_read`.

use anyhow::{Context, Result, anyhow};
use cd_da_reader::{CdReader, ReadOptions, ReadSpeed, RetryConfig};
use cdtoc::Toc;
use localbeat_core::secure_read::SectorSource;

use crate::sys;

/// What the drive says it is. Used as the key for its read offset.
#[derive(Debug, Clone, Default)]
pub struct DriveIdentity {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    /// Empty if the OS doesn't report one.
    pub serial: String,
    /// The OS's own id for the drive, used as the offsets key as is when set:
    /// udev's ID_SERIAL on Linux (what rip-cd.sh stored). Rebuilding it from
    /// the parts above isn't reliable: udev puts the vendor in the model for
    /// SATA drives and uses USB descriptor strings for USB ones.
    pub os_id: String,
}

impl DriveIdentity {
    /// "hp DVDRAM GTA0N".
    pub fn model(&self) -> String {
        format!("{} {}", self.vendor, self.product).trim().to_string()
    }

    /// Key for the offsets file, shaped like udev's ID_SERIAL:
    /// `vendor_product_serial` with spaces as underscores.
    pub fn key(&self) -> String {
        if !self.os_id.is_empty() {
            return self.os_id.clone();
        }
        [&self.vendor, &self.product, &self.serial]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join("_"))
            .collect::<Vec<_>>()
            .join("_")
    }
}

pub struct DriveInfo {
    /// `\\.\E:` on Windows, `/dev/sr1` on Linux.
    pub path: String,
    pub has_audio_cd: bool,
    pub identity: Option<DriveIdentity>,
}

pub fn list_drives() -> Result<Vec<DriveInfo>> {
    Ok(CdReader::list_drives()
        .context("listing CD drives")?
        .into_iter()
        .map(|d| DriveInfo { identity: sys::identity(&d.path).ok(), path: d.path, has_audio_cd: d.has_audio_cd })
        .collect())
}

/// CD drive paths, without opening them (so a drive another process is
/// ripping from isn't disturbed).
pub fn drive_paths() -> Vec<String> {
    sys::drive_paths()
}

/// Whether the drive has a disc with audio tracks in it.
pub fn has_audio_cd(path: &str) -> bool {
    Drive::open(path).and_then(|d| d.read_toc()).is_ok()
}

/// Accepts `E:`, `E`, `\\.\E:` (Windows) or `/dev/sr1`, `sr1` (Linux).
pub fn normalize_path(s: &str) -> String {
    sys::normalize_path(s)
}

pub struct Drive {
    reader: CdReader,
    path: String,
    options: ReadOptions,
}

impl Drive {
    pub fn open(path: &str) -> Result<Self> {
        let path = normalize_path(path);
        let reader = CdReader::open_path(&path).with_context(|| format!("opening {path}"))?;
        // Our own reader falls back to single sectors, so keep the library's
        // retries short.
        let options = ReadOptions::default().with_retry(RetryConfig::default().with_max_attempts(3));
        Ok(Self { reader, path, options })
    }

    /// The first drive with an audio CD in it.
    pub fn open_first_with_audio() -> Result<Self> {
        let d = list_drives()?
            .into_iter()
            .find(|d| d.has_audio_cd)
            .ok_or_else(|| anyhow!("no drive has an audio CD in it"))?;
        Self::open(&d.path)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn identity(&self) -> Result<DriveIdentity> {
        sys::identity(&self.path)
    }

    /// Reads the disc's TOC as a `cdtoc::Toc` (which gives the MusicBrainz and
    /// AccurateRip IDs and the audio track ranges).
    pub fn read_toc(&self) -> Result<Toc> {
        let raw = self.reader.read_toc().context("reading the TOC (is there a disc in the drive?)")?;
        toc_from_raw(&raw)
    }

    /// Requests a read speed (`None` = drive's maximum). Drives may ignore it.
    pub fn set_speed(&self, multiplier: Option<u8>) -> Result<()> {
        let speed = match multiplier {
            Some(x) => ReadSpeed::CustomMultiplier(x as _),
            None => ReadSpeed::Optimal,
        };
        // cd-da-reader applies speed as part of a read; read one sector.
        let opts = self.options.clone().with_read_speed(speed);
        self.reader.read_sector_range(0, 1, &opts).context("setting read speed")?;
        Ok(())
    }

    /// Closes the drive, then ejects. On Linux the kernel locks the tray while
    /// the device is open, and an eject sent through another handle then
    /// "succeeds" without the tray moving, so the reader must be closed first.
    pub fn eject(self) -> Result<()> {
        let Drive { reader, path, .. } = self;
        drop(reader);
        sys::eject(&path).with_context(|| format!("ejecting {path}"))
    }
}

impl SectorSource for Drive {
    fn read_sectors(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
        self.reader.read_sector_range(lba, count, &self.options).map_err(|e| e.to_string())
    }
}

/// cd-da-reader's TOC (LBA 0 = after the pregap) → cdtoc's (+150).
fn toc_from_raw(raw: &cd_da_reader::Toc) -> Result<Toc> {
    let audio: Vec<u32> = raw.tracks.iter().filter(|t| t.is_audio).map(|t| t.start_lba + 150).collect();
    let first_audio = *audio.first().ok_or_else(|| anyhow!("the disc has no audio tracks"))?;
    // At most one data track is supported: before the audio, or a CD-Extra
    // data session after it.
    let data = raw
        .tracks
        .iter()
        .filter(|t| !t.is_audio)
        .map(|t| t.start_lba + 150)
        .find(|&d| d < first_audio)
        .or_else(|| raw.tracks.iter().filter(|t| !t.is_audio).map(|t| t.start_lba + 150).next());
    Toc::from_parts(audio, data, raw.leadout_lba + 150).map_err(|e| anyhow!("unusual TOC: {e}"))
}
