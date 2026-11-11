//! Platform-independent logic for ripshelf. No GUI and no OS-specific code
//! here, so everything builds and tests the same on Windows and Linux.

pub mod accuraterip;
pub mod covers;
pub mod library;
pub mod acoustid;
pub mod album_check;
pub mod audio_source;
pub mod config;
pub mod cover;
pub mod enrich;
pub mod enrich_review;
pub mod flac_encode;
pub mod flacfile;
pub mod http;
pub mod lock;
pub mod musicbrainz;
pub mod naming;
pub mod phone_copy;
pub mod phone_sync;
pub mod rip;
pub mod secure_read;
pub mod vote;
