//! Windows: drive identity and eject via DeviceIoControl.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};

use crate::drive::DriveIdentity;

pub fn normalize_path(s: &str) -> String {
    let s = s.trim().trim_end_matches('\\');
    if s.starts_with(r"\\.\") {
        return s.to_string();
    }
    let letter = s.trim_end_matches(':');
    if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
        return format!(r"\\.\{}:", letter.to_ascii_uppercase());
    }
    s.to_string()
}

pub fn drive_paths() -> Vec<String> {
    const DRIVE_CDROM: u32 = 5;
    (b'A'..=b'Z')
        .map(|l| l as char)
        .filter(|l| {
            let root: Vec<u16> = format!("{l}:\\").encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_CDROM }
        })
        .map(|l| format!(r"\\.\{l}:"))
        .collect()
}

fn open(path: &str, access: u32) -> io::Result<OwnedHandle> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

pub fn identity(path: &str) -> Result<DriveIdentity> {
    // Querying properties needs no read/write access.
    let h = open(path, 0)?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = vec![0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    buf.truncate(returned as usize);
    if buf.len() < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        bail!("short device descriptor");
    }
    let d: STORAGE_DEVICE_DESCRIPTOR = unsafe { ptr::read_unaligned(buf.as_ptr().cast()) };
    let field = |off: u32| -> String {
        let off = off as usize;
        if off == 0 || off >= buf.len() {
            return String::new();
        }
        let end = buf[off..].iter().position(|&b| b == 0).map_or(buf.len(), |p| off + p);
        String::from_utf8_lossy(&buf[off..end]).trim().to_string()
    };
    Ok(DriveIdentity {
        vendor: field(d.VendorIdOffset),
        product: field(d.ProductIdOffset),
        revision: field(d.ProductRevisionOffset),
        serial: field(d.SerialNumberOffset),
        os_id: String::new(),
    })
}

pub fn eject(path: &str) -> Result<()> {
    let h = open(path, GENERIC_READ)?;
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            IOCTL_STORAGE_EJECT_MEDIA,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// Sends Apple's SuperDrive the vendor command that lets it take discs on a
/// computer that isn't a Mac (Linux: sg_raw EA 00 00 00 00 00 01 from a
/// udev rule). Without it the drive spits every disc straight back out.
/// Needed again after each replug.
pub fn wake_superdrive(path: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::IscsiDisc::{
        IOCTL_SCSI_PASS_THROUGH_DIRECT, SCSI_IOCTL_DATA_UNSPECIFIED, SCSI_PASS_THROUGH_DIRECT,
    };
    #[repr(C)]
    struct WithSense {
        sptd: SCSI_PASS_THROUGH_DIRECT,
        sense: [u8; 32],
    }
    const CDB: [u8; 7] = [0xEA, 0, 0, 0, 0, 0, 0x01];
    let h = open(path, GENERIC_READ | GENERIC_WRITE)?;
    let mut w: WithSense = unsafe { std::mem::zeroed() };
    w.sptd.Length = size_of::<SCSI_PASS_THROUGH_DIRECT>() as u16;
    w.sptd.CdbLength = CDB.len() as u8;
    w.sptd.DataIn = SCSI_IOCTL_DATA_UNSPECIFIED as u8;
    w.sptd.TimeOutValue = 10;
    w.sptd.SenseInfoLength = w.sense.len() as u8;
    w.sptd.SenseInfoOffset = std::mem::offset_of!(WithSense, sense) as u32;
    w.sptd.Cdb[..CDB.len()].copy_from_slice(&CDB);
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            IOCTL_SCSI_PASS_THROUGH_DIRECT,
            &mut w as *mut _ as *mut _,
            size_of::<WithSense>() as u32,
            &mut w as *mut _ as *mut _,
            size_of::<WithSense>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if w.sptd.ScsiStatus != 0 {
        bail!("the drive refused the wake-up command (SCSI status {})", w.sptd.ScsiStatus);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn paths() {
        assert_eq!(normalize_path("e:"), r"\\.\E:");
        assert_eq!(normalize_path("E"), r"\\.\E:");
        assert_eq!(normalize_path(r"E:\"), r"\\.\E:");
        assert_eq!(normalize_path(r"\\.\F:"), r"\\.\F:");
    }
}
