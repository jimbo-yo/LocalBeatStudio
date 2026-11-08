//! Linux: drive identity from sysfs and udev's database, eject via ioctl.
//! The offsets key is udev's ID_SERIAL, the same one rip-cd.sh's offsets file
//! uses, so that file works unchanged.

use std::ffi::CString;
use std::io;

use anyhow::Result;

use crate::drive::DriveIdentity;

const CDROMEJECT: u64 = 0x5309;
const CDROM_LOCKDOOR: u64 = 0x5329;

pub fn normalize_path(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('/') { s.to_string() } else { format!("/dev/{s}") }
}

pub fn drive_paths() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("sr") && n[2..].chars().all(|c| c.is_ascii_digit()) && n.len() > 2)
        .map(|n| format!("/dev/{n}"))
        .collect();
    v.sort();
    v
}

pub fn identity(path: &str) -> Result<DriveIdentity> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let read = |f: &str| {
        std::fs::read_to_string(format!("/sys/block/{name}/device/{f}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let udev = udev_properties(name);
    let prop = |k: &str| udev.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
    Ok(DriveIdentity {
        vendor: read("vendor"),
        product: read("model"),
        revision: read("rev"),
        serial: prop("ID_SERIAL_SHORT"),
        os_id: prop("ID_SERIAL"),
    })
}

/// The `E:KEY=value` properties udev recorded for a block device, read from
/// its database (`/run/udev/data/b<major>:<minor>`, what `udevadm info`
/// prints), so no libudev is needed. Empty if udev has no record.
fn udev_properties(name: &str) -> Vec<(String, String)> {
    let Ok(dev) = std::fs::read_to_string(format!("/sys/block/{name}/dev")) else {
        return Vec::new();
    };
    std::fs::read_to_string(format!("/run/udev/data/b{}", dev.trim()))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("E:")?.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Ejects the disc. The tray can be left locked against removal after a rip
/// (seen with the HP GTA0N), and CDROMEJECT then returns a positive SCSI
/// status (2, check condition) instead of failing, so: unlock the door first,
/// count only 0 as success, and retry briefly in case the drive is busy.
pub fn eject(path: &str) -> Result<()> {
    let c = CString::new(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        unsafe {
            // Ignore failure: the eject below says whether it worked.
            libc::ioctl(fd, CDROM_LOCKDOOR as _, 0);
            let r = libc::ioctl(fd, CDROMEJECT as _, 0);
            if r == 0 {
                libc::close(fd);
                return Ok(());
            }
            last = if r < 0 {
                io::Error::last_os_error().to_string()
            } else {
                format!("the drive refused (status {r})")
            };
        }
    }
    unsafe { libc::close(fd) };
    Err(anyhow::anyhow!("couldn't eject: {last}"))
}
