//! Free space on the disk a path is (or will be) on.

use std::path::Path;

/// Bytes free for this user on the disk holding `path`. A path that doesn't
/// exist yet (an album folder about to be made) is measured at its nearest
/// existing parent. None if it can't be found out.
pub fn free_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    sys_free(existing)
}

#[cfg(windows)]
fn sys_free(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // Follows junctions and mount points: the space of the disk behind them.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn sys_free(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::free_space;

    #[test]
    fn measures_a_missing_folder_at_its_parent() {
        let here = std::env::temp_dir();
        let free = free_space(&here).expect("temp dir's disk");
        assert!(free > 0);
        assert!(free_space(&here.join("no such album").join("nor this")).is_some());
    }
}
