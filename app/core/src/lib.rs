//! Platform-independent logic for ripshelf. No GUI and no OS-specific code
//! here, so everything builds and tests the same on Windows and Linux.

pub mod accuraterip;
pub mod album_check;
pub mod flacfile;
pub mod http;
pub mod naming;
