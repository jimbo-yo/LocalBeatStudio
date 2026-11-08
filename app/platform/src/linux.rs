//! Linux: drive identity from sysfs, eject via ioctl. NOT yet built or
//! tested on Linux. The serial (udev ID_SERIAL, used by rip-cd.sh's offsets
//! file) needs the udev crate; until then the key has no serial.

use std::ffi::CString;
use std::io;

use anyhow::Result;

use crate::drive::DriveIdentity;

const CDROMEJECT: u64 = 0x5309;

pub fn normalize_path(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('/') { s.to_string() } else { format!("/dev/{s}") }
}

pub fn identity(path: &str) -> Result<DriveIdentity> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let read = |f: &str| {
        std::fs::read_to_string(format!("/sys/block/{name}/device/{f}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    Ok(DriveIdentity { vendor: read("vendor"), product: read("model"), revision: read("rev"), serial: String::new() })
}

pub fn eject(path: &str) -> Result<()> {
    let c = CString::new(path)?;
    unsafe {
        let fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let r = libc::ioctl(fd, CDROMEJECT as _, 0);
        let err = io::Error::last_os_error();
        libc::close(fd);
        if r < 0 {
            return Err(err.into());
        }
    }
    Ok(())
}
