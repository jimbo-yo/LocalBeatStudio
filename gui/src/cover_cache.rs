//! Album covers as textures, loaded on background threads as they come into
//! view (thumbnails are made once and kept on disk by core::covers), with
//! the cover's accent colour. At most `MAX` per size stay in GPU memory; the
//! least recently drawn go first.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, ColorImage, TextureHandle};
use localbeat_core::covers;
use localbeat_core::library::Library;

const MAX: usize = 260;

#[derive(Clone)]
pub struct Cover {
    pub tex: TextureHandle,
    pub accent: Color32,
}

enum Slot {
    Loading,
    Ready(Cover, u64),
    None,
}

struct Job {
    key: String,
    album: usize,
    px: u32,
    lib: Arc<Library>,
}

type Done = (String, u32, Option<(ColorImage, [u8; 3])>);

pub struct CoverCache {
    slots: HashMap<(String, u32), Slot>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    frame: u64,
}

impl CoverCache {
    pub fn new(cache_dir: PathBuf, ctx: egui::Context) -> Self {
        let (jobs, rx) = channel::<Job>();
        let (tx, done) = channel::<Done>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..3 {
            let (rx, tx, cache_dir, ctx) = (rx.clone(), tx.clone(), cache_dir.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                let job = match rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let album = &job.lib.albums[job.album];
                let result = covers::thumbnail(album, &job.lib.songs, &cache_dir, job.px)
                    .ok()
                    .flatten()
                    .and_then(|p| image::open(p).ok())
                    .map(|img| {
                        let rgba = img.to_rgba8();
                        let accent = covers::accent_color(rgba.as_raw());
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        (ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), accent)
                    });
                if tx.send((job.key, job.px, result)).is_err() {
                    return;
                }
                ctx.request_repaint();
            });
        }
        CoverCache { slots: HashMap::new(), jobs, done, frame: 0 }
    }

    /// Takes finished loads; call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        for (key, px, r) in self.done.try_iter() {
            let slot = match r {
                Some((img, [r, g, b])) => {
                    let tex = ctx.load_texture(format!("cover-{px}-{key}"), img, egui::TextureOptions::LINEAR);
                    Slot::Ready(Cover { tex, accent: Color32::from_rgb(r, g, b) }, self.frame)
                }
                None => Slot::None,
            };
            self.slots.insert((key, px), slot);
        }
        // Keep GPU memory bounded.
        for px in [256u32, 512] {
            let mut ready: Vec<(u64, (String, u32))> = self
                .slots
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready(_, used) if k.1 == px => Some((*used, k.clone())),
                    _ => None,
                })
                .collect();
            let max = if px == 512 { 24 } else { MAX };
            if ready.len() > max {
                ready.sort();
                for (_, k) in ready.into_iter().take(40) {
                    self.slots.remove(&k);
                }
            }
        }
    }

    /// The album's cover at `px` (256 for grids, 512 for big views), loading
    /// it if needed. None while loading or if it has none.
    pub fn get(&mut self, lib: &Arc<Library>, album: usize, px: u32) -> Option<Cover> {
        let key = (lib.albums[album].key.clone(), px);
        let frame = self.frame;
        match self.slots.get_mut(&key) {
            Some(Slot::Ready(c, used)) => {
                *used = frame;
                return Some(c.clone());
            }
            Some(_) => return None,
            None => {}
        }
        self.slots.insert(key.clone(), Slot::Loading);
        let _ = self.jobs.send(Job { key: key.0, album, px, lib: lib.clone() });
        None
    }

    /// Forget everything (after a rescan: covers may have changed).
    pub fn clear(&mut self) {
        self.slots.clear();
    }
}
