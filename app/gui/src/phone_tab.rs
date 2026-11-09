//! The Phone tab: choose which albums are on the phone and sync them. The
//! work (connecting, scanning, converting, sending) runs in background
//! threads through `ripshelf_core::phone_sync`, like `ripshelf sync`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use ripshelf_core::config::{Prefs, Settings};
use ripshelf_core::phone_sync::{
    self, Album, AlbumAction, Origin, Patient, PhoneFs, PhoneState, Plan, Reconnect, SyncEvent, SyncSummary,
};
use ripshelf_platform::phone::{self, AdbPhone, KdeConnectPhone};

trait Conn: PhoneFs + Reconnect + Send + Sync {}
impl<T: PhoneFs + Reconnect + Send + Sync> Conn for T {}

struct Loaded {
    conn: Arc<dyn Conn>,
    desc: String,
    lib: Vec<Album>,
    phone: PhoneState,
}

enum Msg {
    Loaded(Result<Loaded, String>),
    Event(SyncEvent),
    Waiting(String),
    Done(Result<SyncSummary, String>),
}

#[derive(PartialEq, Clone, Copy)]
enum Show {
    All,
    OnPhone,
    NotOnPhone,
    Changes,
}

const GREEN: Color32 = Color32::from_rgb(60, 160, 80);
const BLUE: Color32 = Color32::from_rgb(70, 130, 220);
const GREY: Color32 = Color32::from_rgb(140, 140, 140);

pub struct PhoneTab {
    loaded: Option<Loaded>,
    status: String,
    error: Option<String>,
    wanted: BTreeSet<String>,
    plan: Plan,
    search: String,
    show: Show,
    rx: Option<Receiver<Msg>>,
    busy: bool,
    syncing: bool,
    cancel: Arc<AtomicBool>,
    progress: (u64, u64, String),
    waiting: Option<String>,
    confirm: bool,
    result: Option<String>,
}

fn size(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

impl PhoneTab {
    pub fn new() -> Self {
        PhoneTab {
            loaded: None,
            status: "Not connected".into(),
            error: None,
            wanted: BTreeSet::new(),
            plan: Plan::default(),
            search: String::new(),
            show: Show::All,
            rx: None,
            busy: false,
            syncing: false,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: (0, 0, String::new()),
            waiting: None,
            confirm: false,
            result: None,
        }
    }

    /// Connects (over USB or Wi-Fi) and scans the library and the phone.
    fn connect(&mut self, s: &Settings, usb: bool, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.error = None;
        self.status = if usb { "Connecting over USB…" } else { "Connecting over Wi-Fi…" }.into();
        let s = s.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<Loaded> {
                let (conn, desc): (Arc<dyn Conn>, String) = if usb {
                    let p = AdbPhone::connect(phone::ADB_MUSIC)?;
                    let d = p.describe();
                    (Arc::new(p), d)
                } else {
                    let p = KdeConnectPhone::connect(
                        phone::preferred_kdeconnect_device(&s.config_dir).as_deref(),
                        phone::KDECONNECT_MUSIC,
                    )?;
                    let d = p.describe();
                    (Arc::new(p), d)
                };
                let lib = phone_sync::scan_library(&s.flac_dir, &s.misc_dir);
                let phone = phone_sync::read_phone(&*conn)?;
                Ok(Loaded { conn, desc, lib, phone })
            })();
            let _ = tx.send(Msg::Loaded(r.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn replan(&mut self) {
        if let Some(l) = &self.loaded {
            self.plan = phone_sync::plan(&l.lib, &l.phone, &self.wanted);
        }
    }

    fn start_sync(&mut self, s: &Settings, ctx: &egui::Context) {
        let Some(l) = &self.loaded else { return };
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.syncing = true;
        self.waiting = None;
        self.result = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        let (conn, phone, plan) = (l.conn.clone(), l.phone.clone(), self.plan.clone());
        let (work, kbps, cancel, ctx) = (s.cache_dir.join("sync-work"), s.opus_kbps, self.cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let cancelled = || cancel.load(Ordering::Relaxed);
            let fs = Patient {
                phone: &*conn,
                wait: Duration::from_secs(600),
                on_wait: &|msg| send(Msg::Waiting(msg.to_string())),
                cancelled: &cancelled,
            };
            let r = phone_sync::execute(&fs, &phone, &plan, &work, kbps, &|e| send(Msg::Event(e)), &cancelled);
            send(Msg::Done(r.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Takes in background results; returns lines for the window's log.
    pub fn poll(&mut self, s: &Settings, prefs: &mut Prefs, ctx: &egui::Context) -> Vec<String> {
        let mut log = Vec::new();
        let Some(rx) = &self.rx else { return log };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        let mut reconnect = false;
        for m in msgs {
            match m {
                Msg::Loaded(Ok(l)) => {
                    self.busy = false;
                    self.status = format!("Connected: {}", l.desc);
                    if let (true, Some((total, free))) = (prefs.phone_usb, l.phone.space) {
                        // Remembered for Wi-Fi, which can't tell.
                        let date = chrono_date();
                        prefs.last_space = Some((total, free, l.phone.music_bytes(), date));
                        let _ = prefs.save(s);
                    }
                    self.wanted = phone_sync::on_phone(&l.lib, &l.phone);
                    self.loaded = Some(l);
                    self.replan();
                }
                Msg::Loaded(Err(e)) => {
                    self.busy = false;
                    self.loaded = None;
                    self.status = "Not connected".into();
                    self.error = Some(e);
                }
                Msg::Event(SyncEvent::Removing(r)) => {
                    self.waiting = None;
                    self.progress.2 = format!("Removing {r}");
                }
                Msg::Event(SyncEvent::Sending { file, done, total }) => {
                    self.waiting = None;
                    self.progress = (done, total, if file.is_empty() { "Finishing…".into() } else { file });
                }
                Msg::Event(SyncEvent::Failed { file, error }) => log.push(format!("Phone sync: couldn't convert {file}: {error}")),
                Msg::Waiting(w) => self.waiting = Some(w),
                Msg::Done(r) => {
                    self.busy = false;
                    self.syncing = false;
                    self.waiting = None;
                    let text = match r {
                        Ok(sum) => {
                            let mut t = format!(
                                "{}: {} files sent, {} albums removed",
                                if sum.stopped { "Stopped" } else { "Sync done" },
                                sum.sent,
                                sum.removed_albums
                            );
                            if !sum.failed.is_empty() {
                                t.push_str(&format!(", {} failed (see the log)", sum.failed.len()));
                            }
                            t
                        }
                        Err(e) => format!("Sync stopped: {e}"),
                    };
                    log.push(format!("Phone sync: {text}"));
                    self.result = Some(text);
                    reconnect = true;
                }
            }
        }
        if reconnect {
            self.connect(s, prefs.phone_usb, ctx);
        }
        log
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) {
        let ctx = ui.ctx().clone();
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Connection
        ui.horizontal(|ui| {
            ui.label("Phone:");
            let before = prefs.phone_usb;
            ui.add_enabled_ui(!self.busy, |ui| {
                ui.radio_value(&mut prefs.phone_usb, true, "adb")
                    .on_hover_text("USB cable, or Android's wireless debugging: fastest, and shows free space");
                ui.radio_value(&mut prefs.phone_usb, false, "KDE Connect")
                    .on_hover_text("Wi-Fi through KDE Connect: no setup on the phone beyond pairing, but slower and can't show free space");
            });
            if before != prefs.phone_usb {
                let _ = prefs.save(s);
                self.loaded = None;
                self.connect(s, prefs.phone_usb, &ctx);
            }
            if ui.add_enabled(!self.busy, egui::Button::new("🔄 Connect / refresh")).clicked() {
                self.connect(s, prefs.phone_usb, &ctx);
            }
            if self.busy && !self.syncing {
                ui.spinner();
            }
            ui.label(&self.status);
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if self.loaded.is_none() {
            ui.add_space(12.0);
            if !self.busy {
                ui.label("Connect the phone to choose what's on it.");
                ui.small("USB: phone plugged in with USB debugging on. Wi-Fi: phone paired in KDE Connect, same network.");
            }
            return;
        }

        self.capacity(ui, s, prefs);
        ui.separator();

        if self.syncing {
            self.sync_progress(ui);
            return;
        }
        if let Some(r) = &self.result {
            ui.label(RichText::new(r).strong());
        }

        // Search, filter, actions
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search artists and albums…").desired_width(260.0));
            egui::ComboBox::from_id_salt("phone-show")
                .selected_text(match self.show {
                    Show::All => "All albums",
                    Show::OnPhone => "On the phone",
                    Show::NotOnPhone => "Not on the phone",
                    Show::Changes => "Changes only",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.show, Show::All, "All albums");
                    ui.selectable_value(&mut self.show, Show::OnPhone, "On the phone");
                    ui.selectable_value(&mut self.show, Show::NotOnPhone, "Not on the phone");
                    ui.selectable_value(&mut self.show, Show::Changes, "Changes only");
                });
            if ui.button("Undo changes").on_hover_text("Back to what's on the phone now").clicked() {
                if let Some(l) = &self.loaded {
                    self.wanted = phone_sync::on_phone(&l.lib, &l.phone);
                    changed = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = !self.plan.is_empty() && !self.over_capacity(prefs);
                if ui.add_enabled(ok, egui::Button::new(RichText::new("⟳ Sync to phone").strong())).clicked() {
                    self.confirm = true;
                }
            });
        });
        ui.add_space(4.0);
        changed |= self.album_list(ui);
        if changed {
            self.replan();
        }
        self.confirm_window(&ctx, s);
    }

    fn space(&self, prefs: &Prefs) -> Option<(u64, u64, Option<String>)> {
        let l = self.loaded.as_ref()?;
        match l.phone.space {
            Some((t, f)) => Some((t, f, None)),
            None => prefs.last_space.as_ref().map(|(t, f, music, date)| {
                // Other apps' use assumed unchanged since that USB reading.
                let free = (*f + *music).saturating_sub(l.phone.music_bytes());
                (*t, free, Some(format!("estimated from the USB reading on {date}")))
            }),
        }
    }

    fn over_capacity(&self, prefs: &Prefs) -> bool {
        match self.space(prefs) {
            Some((_, free, _)) => {
                let after = free as i64 - self.plan.add_bytes() as i64 + self.plan.remove_bytes() as i64;
                self.plan.add_bytes() > 0 && after < (prefs.reserve_gb * 1e9) as i64
            }
            None => false,
        }
    }

    fn capacity(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) {
        let Some(l) = &self.loaded else { return };
        let music = l.phone.music_bytes();
        let tracks: usize = l.phone.albums.iter().map(|a| a.audio_names().len()).sum();
        let (add, rem) = (self.plan.add_bytes(), self.plan.remove_bytes());
        let reserve = (prefs.reserve_gb * 1e9) as u64;
        let space = self.space(prefs);
        let avg = {
            let v: Vec<u64> = l.lib.iter().map(|a| a.est_bytes()).filter(|b| *b > 0).collect();
            if v.is_empty() { 0 } else { v.iter().sum::<u64>() / v.len() as u64 }
        };
        ui.horizontal(|ui| {
            let tile = |ui: &mut egui::Ui, title: &str, value: String, sub: String, colour: Option<Color32>| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_min_width(220.0);
                    ui.vertical(|ui| {
                        ui.label(title);
                        let v = RichText::new(value).size(24.0).strong();
                        ui.label(match colour {
                            Some(c) => v.color(c),
                            None => v,
                        });
                        ui.small(sub);
                    });
                });
            };
            let change = if add + rem > 0 { format!("; after sync {}", size((music + add).saturating_sub(rem))) } else { String::new() };
            tile(ui, "Music on the phone", size(music), format!("{} albums · {tracks} tracks{change}", l.phone.albums.len()), None);
            match &space {
                Some((total, free, note)) => {
                    let after = (*free + rem).saturating_sub(add);
                    let room = after as i64 - reserve as i64;
                    let mut sub = format!("of {}", size(*total));
                    if let Some(n) = note {
                        sub.push_str(&format!(" ({n})"));
                    }
                    tile(ui, "Free on the phone", size(after), sub, None);
                    let more = if avg > 0 && room > 0 { format!("about {} more albums, ", room as u64 / avg) } else { String::new() };
                    tile(
                        ui,
                        "Room for more music",
                        if room < 0 { format!("−{}", size(room.unsigned_abs())) } else { size(room as u64) },
                        format!("{more}keeping {} free", size(reserve)),
                        (room < 0).then_some(ui.visuals().error_fg_color),
                    );
                }
                None => tile(ui, "Free on the phone", "?".into(), "Wi-Fi can't tell: connect by USB once".into(), None),
            }
        });
        ui.horizontal(|ui| {
            ui.label("Always leave free:");
            if ui.add(egui::DragValue::new(&mut prefs.reserve_gb).range(0.0..=100.0).speed(0.5).suffix(" GB")).changed() {
                let _ = prefs.save(s);
            }
            if self.over_capacity(prefs) {
                ui.colored_label(ui.visuals().error_fg_color, "Too much selected for the space left.");
            }
        });
    }

    /// The album list; returns whether the selection changed.
    fn album_list(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(l) = &self.loaded else { return false };
        let q = self.search.trim().to_lowercase();
        let on_phone: BTreeSet<&str> = l.phone.albums.iter().map(|a| a.key.as_str()).collect();

        // Rows: (group, key, title, tag, size), library albums by artist, then phone-only ones.
        struct Row {
            key: String,
            title: String,
            misc: bool,
            bytes: u64,
            drm: usize,
        }
        let mut groups: BTreeMap<String, (String, Vec<Row>)> = BTreeMap::new();
        for a in &l.lib {
            if a.files.iter().all(|f| !f.convert) {
                continue; // only DRM files
            }
            let artist = if a.artist.is_empty() { a.rel.clone() } else { a.artist.clone() };
            let gk = phone_sync::norm_key(&artist).trim_start_matches("the ").to_string();
            let bytes = l.phone.album(&a.key).map(|p| p.bytes()).unwrap_or_else(|| a.est_bytes());
            let title = if a.artist.is_empty() { a.rel.clone() } else { a.title.clone() };
            groups.entry(gk).or_insert_with(|| (artist, Vec::new())).1.push(Row {
                key: a.key.clone(),
                title,
                misc: a.origin == Origin::Misc,
                bytes,
                drm: a.protected.len(),
            });
        }
        let lib_keys: BTreeSet<&str> = l.lib.iter().map(|a| a.key.as_str()).collect();
        for p in &l.phone.albums {
            if !lib_keys.contains(p.key.as_str()) {
                groups
                    .entry("\u{10FFFF}".into())
                    .or_insert_with(|| ("On the phone, not in your library".into(), Vec::new()))
                    .1
                    .push(Row { key: p.key.clone(), title: p.rel.clone(), misc: false, bytes: p.bytes(), drm: 0 });
            }
        }

        let mut changed = false;
        let wanted = &mut self.wanted;
        let actions = &self.plan.actions;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (gk, (artist, rows)) in &groups {
                let visible: Vec<&Row> = rows
                    .iter()
                    .filter(|r| q.is_empty() || artist.to_lowercase().contains(&q) || r.title.to_lowercase().contains(&q))
                    .filter(|r| {
                        let action = actions.get(&r.key).cloned().unwrap_or(AlbumAction::None);
                        match self.show {
                            Show::All => true,
                            Show::OnPhone => on_phone.contains(r.key.as_str()),
                            Show::NotOnPhone => !on_phone.contains(r.key.as_str()),
                            Show::Changes => !matches!(action, AlbumAction::None | AlbumAction::Adopt),
                        }
                    })
                    .collect();
                if visible.is_empty() {
                    continue;
                }
                let on = rows.iter().filter(|r| wanted.contains(&r.key)).count();
                let id = ui.make_persistent_id(("phone-artist", gk));
                let open_default = !q.is_empty() || self.show == Show::Changes;
                let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, open_default);
                if !q.is_empty() || self.show == Show::Changes {
                    state.set_open(true);
                }
                state
                    .show_header(ui, |ui| {
                        let mut all = on == rows.len();
                        let label = RichText::new(artist).strong();
                        if ui.checkbox(&mut all, label).changed() {
                            for r in rows {
                                if all {
                                    wanted.insert(r.key.clone());
                                } else {
                                    wanted.remove(&r.key);
                                }
                            }
                            changed = true;
                        }
                        if rows.len() > 1 {
                            ui.weak(format!("{on} of {} albums", rows.len()));
                        }
                    })
                    .body(|ui| {
                        for r in &visible {
                            ui.horizontal(|ui| {
                                let mut w = wanted.contains(&r.key);
                                if ui.checkbox(&mut w, &r.title).changed() {
                                    if w {
                                        wanted.insert(r.key.clone());
                                    } else {
                                        wanted.remove(&r.key);
                                    }
                                    changed = true;
                                }
                                if r.misc {
                                    ui.small("Misc").on_hover_text("From the Miscellaneous library (converted from AAC/MP3/etc.)");
                                }
                                if r.drm > 0 {
                                    ui.small(format!("{} DRM", r.drm)).on_hover_text("iTunes DRM-protected files: can't be converted, left out");
                                }
                                ui.weak(size(r.bytes));
                                let (text, colour) = match actions.get(&r.key) {
                                    Some(AlbumAction::Add) => ("will be added".to_string(), GREEN),
                                    Some(AlbumAction::Update { files }) => (format!("will update ({files} files)"), BLUE),
                                    Some(AlbumAction::Replace) => ("will be replaced (new rip)".into(), BLUE),
                                    Some(AlbumAction::Remove) => ("will be removed".into(), ui.visuals().error_fg_color),
                                    _ if on_phone.contains(r.key.as_str()) => ("on phone".into(), ui.visuals().text_color()),
                                    _ => ("not on phone".into(), GREY),
                                };
                                ui.colored_label(colour, text);
                            });
                        }
                    });
            }
        });
        changed
    }

    fn sync_progress(&mut self, ui: &mut egui::Ui) {
        let (done, total, file) = &self.progress;
        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
        ui.heading("Syncing…");
        ui.add(egui::ProgressBar::new(frac).show_percentage());
        ui.label(file);
        if let Some(w) = &self.waiting {
            ui.colored_label(Color32::from_rgb(210, 130, 30), w);
        }
        if ui.button("⏹ Stop sync").on_hover_text("Stops after the current file; what's sent stays on the phone").clicked() {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn confirm_window(&mut self, ctx: &egui::Context, s: &Settings) {
        if !self.confirm {
            return;
        }
        let Some(l) = &self.loaded else { return };
        let name = |key: &str| {
            l.lib
                .iter()
                .find(|a| a.key == key)
                .map(|a| a.rel.clone())
                .or_else(|| l.phone.album(key).map(|p| p.rel.clone()))
                .unwrap_or_else(|| key.to_string())
        };
        let mut lines: Vec<(String, String)> = Vec::new();
        for (key, action) in &self.plan.actions {
            let what = match action {
                AlbumAction::Add => "add",
                AlbumAction::Update { .. } => "update",
                AlbumAction::Replace => "replace",
                AlbumAction::Remove => "REMOVE",
                _ => continue,
            };
            lines.push((what.into(), name(key)));
        }
        lines.sort();
        let mut go = false;
        let mut close = false;
        egui::Window::new("Sync to phone?")
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "About {} to send, {} to remove from the phone.",
                    size(self.plan.add_bytes()),
                    size(self.plan.remove_bytes())
                ));
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (what, album) in &lines {
                        ui.horizontal(|ui| {
                            let c = if what == "REMOVE" { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                            ui.colored_label(c, format!("{what:8}"));
                            ui.label(album);
                        });
                    }
                    if lines.is_empty() {
                        ui.label("(Recording the albums already on the phone; nothing to send.)");
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Sync").strong()).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if go {
            self.confirm = false;
            self.start_sync(s, ctx);
        } else if close {
            self.confirm = false;
        }
    }
}

/// Today's date (YYYY-MM-DD) without a date library.
fn chrono_date() -> String {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0) as i64;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}
