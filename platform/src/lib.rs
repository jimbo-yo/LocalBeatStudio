//! OS-specific parts: CD drives now; disc-inserted detection and phone
//! connections later (see docs/rust-app-design.md).

pub mod disk;
pub mod drive;
pub mod phone;
pub mod phone_notify;

/// A command for a console program (adb, rsync, ssh…). On Windows it runs
/// without a console window: Studio has none, so each run would flash one up.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod sys;
