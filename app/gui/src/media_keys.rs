//! The desktop's media controls: on Linux ripshelf appears over MPRIS
//! (D-Bus, via souvlaki's pure-Rust zbus backend) in Plasma's media widget
//! and lock screen, and the keyboard's play/next/previous keys work.
//!
//! Runs on its own thread, talking to the player directly: a hidden or
//! minimised window gets no frames, and media keys must work then too.
//! Windows needs the window handle (SMTC); not wired up yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ripshelf_core::library::Library;
use ripshelf_player::{Player, State};
use souvlaki::{MediaControlEvent as E, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};

/// The library as last scanned, shared with the UI.
pub type SharedLibrary = Arc<Mutex<Option<Arc<Library>>>>;

/// Starts the media-controls thread. Err if the desktop has none.
pub fn spawn(player: Arc<Player>, library: SharedLibrary, cache_dir: PathBuf, ctx: eframe::egui::Context) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let (tx, events) = channel();
    let config = PlatformConfig { display_name: "ripshelf", dbus_name: "ripshelf", hwnd: None };
    let mut controls = MediaControls::new(config).map_err(|e| format!("media keys unavailable: {e:?}"))?;
    let repaint = ctx.clone();
    controls
        .attach(move |e| {
            let _ = tx.send(e);
            repaint.request_repaint();
        })
        .map_err(|e| format!("media keys unavailable: {e:?}"))?;
    std::thread::Builder::new()
        .name("media-keys".into())
        .spawn(move || {
            let mut shown: Option<PathBuf> = None;
            let mut state = State::Stopped;
            let mut sent_at = Instant::now();
            let mut index: (usize, HashMap<PathBuf, usize>) = (0, HashMap::new());
            loop {
                while let Ok(e) = events.try_recv() {
                    let s = player.status();
                    match e {
                        E::Play => player.play(),
                        E::Pause => player.pause(),
                        E::Toggle => player.toggle(),
                        E::Next => player.next(),
                        E::Previous => player.prev(),
                        E::Stop => player.stop(),
                        E::Seek(d) | E::SeekBy(d, _) => {
                            let step = if let E::SeekBy(_, by) = e { by.as_secs_f64() } else { 10.0 };
                            player.seek(if d == SeekDirection::Forward { s.position + step } else { (s.position - step).max(0.0) });
                        }
                        E::SetPosition(p) => player.seek(p.0.as_secs_f64()),
                        E::SetVolume(v) => {
                            let v = (v as f32).clamp(0.0, 1.0);
                            player.set_volume(v * v);
                        }
                        E::Raise => ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus),
                        _ => {}
                    }
                }
                let s = player.status();
                if s.path != shown {
                    let lib = library.lock().unwrap().clone();
                    // Wait for the library to know the song (right after start).
                    let song = match (&lib, &s.path) {
                        (Some(l), Some(p)) => {
                            let id = Arc::as_ptr(l) as usize;
                            if index.0 != id {
                                index = (id, l.songs.iter().enumerate().map(|(i, x)| (x.path.clone(), i)).collect());
                            }
                            index.1.get(p).copied()
                        }
                        _ => None,
                    };
                    if s.path.is_none() || song.is_some() {
                        shown = s.path.clone();
                        let meta = match (song, &lib) {
                            (Some(i), Some(l)) => {
                                let x = &l.songs[i];
                                let cover = l
                                    .albums
                                    .iter()
                                    .find(|a| a.songs.contains(&i))
                                    .and_then(|a| ripshelf_core::covers::thumbnail(a, &l.songs, &cache_dir, 512).ok().flatten())
                                    .map(|p| format!("file://{}", p.display()));
                                let artist = if x.artist.is_empty() { &x.album_artist } else { &x.artist };
                                let _ = controls.set_metadata(MediaMetadata {
                                    title: Some(&x.title),
                                    artist: Some(artist),
                                    album: Some(&x.album),
                                    cover_url: cover.as_deref(),
                                    duration: Some(Duration::from_secs_f64(s.duration.unwrap_or(x.seconds))),
                                });
                                true
                            }
                            _ => {
                                let _ = controls.set_metadata(MediaMetadata::default());
                                true
                            }
                        };
                        if meta {
                            state = State::Stopped;
                            sent_at = Instant::now() - Duration::from_secs(60);
                        }
                    }
                }
                if s.state != state || sent_at.elapsed() > Duration::from_secs(5) {
                    state = s.state;
                    sent_at = Instant::now();
                    let progress = Some(MediaPosition(Duration::from_secs_f64(s.position.max(0.0))));
                    let _ = controls.set_playback(match s.state {
                        State::Playing => MediaPlayback::Playing { progress },
                        State::Paused => MediaPlayback::Paused { progress },
                        State::Stopped if s.path.is_some() => MediaPlayback::Paused { progress },
                        State::Stopped => MediaPlayback::Stopped,
                    });
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}
