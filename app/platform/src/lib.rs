//! OS-specific parts: CD drives now; disc-inserted detection and phone
//! connections later (see docs/rust-app-design.md).

pub mod drive;

#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod sys;
