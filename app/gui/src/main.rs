//! LocalBeat Studio's window: runs the rip loop in the background (disc after disc)
//! and shows what it's doing.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use localbeat_core::accuraterip::Verdict;
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::flac_encode::Picture;
use localbeat_core::musicbrainz::Release;
use localbeat_core::rip::Acceptance;
use localbeat_engine::notify::Notifier;
use localbeat_engine::{self as engine, Event, Frontend, Options, ReleaseChoice, TrackInfo};
use localbeat_platform::phone_notify;

mod cover_cache;
mod library_ui;
mod media_keys;
mod phone_tab;
mod review_tab;
mod theme;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Library,
    Rip,
    Phone,
    Review,
}

/// Seconds before the best exact release is used if nobody answers.
const AUTO_CHOOSE_SECS: u64 = 30;

fn main() -> eframe::Result {
    // Linux: draw with OpenGL unless the user picked a backend. On the Fedora
    // laptop's Intel Haswell GPU, Mesa's Vulkan driver is incomplete and wgpu's
    // default (Vulkan) garbled all text. Set before any threads start.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WGPU_BACKEND").is_none() {
        unsafe { std::env::set_var("WGPU_BACKEND", "gl") };
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("LocalBeat Studio")
            .with_app_id("localbeat-studio")
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native("LocalBeat Studio", options, Box::new(|cc| {
        theme::apply(&cc.egui_ctx);
        Ok(Box::new(App::new(cc.egui_ctx.clone())))
    }))
}

/// The LocalBeat mark for the window / task bar (the app menu uses the SVG).
fn window_icon() -> std::sync::Arc<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/localbeat-256.png")).map(|i| i.to_rgba8());
    std::sync::Arc::new(match img {
        Ok(i) => egui::IconData { width: i.width(), height: i.height(), rgba: i.into_raw() },
        Err(_) => egui::IconData::default(),
    })
}

/// Messages from the engine thread.
enum Msg {
    /// An event from the rip loop of a drive (path).
    Event(String, Event),
    Choose { drive: String, candidates: Vec<Release>, reply: Sender<ReleaseChoice> },
    /// A drive's rip loop started (with its stop-this-disc flag) / ended.
    LoopStarted(String, Arc<AtomicBool>),
    Stopped(String, Option<String>),
    /// The drive watcher ended (after Stop).
    WatcherDone,
}

struct GuiFrontend {
    drive: String,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    /// "Stop & eject" for this drive's current disc.
    stop_disc: Arc<AtomicBool>,
    ctx: egui::Context,
    notifier: Arc<Notifier>,
}

impl Frontend for GuiFrontend {
    fn event(&self, e: Event) {
        if matches!(e, Event::DiscStart { .. }) {
            self.stop_disc.store(false, Ordering::Relaxed);
        }
        self.notifier.observe(&e);
        let _ = self.tx.send(Msg::Event(self.drive.clone(), e));
        self.ctx.request_repaint();
    }

    fn choose_release(&self, candidates: &[Release]) -> ReleaseChoice {
        let (reply, answer) = channel();
        let _ = self.tx.send(Msg::Choose { drive: self.drive.clone(), candidates: candidates.to_vec(), reply });
        self.ctx.request_repaint();
        loop {
            match answer.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => return c,
                Err(_) if self.cancelled() || self.stop_disc() => return ReleaseChoice::Skip,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return ReleaseChoice::Skip,
                Err(_) => {}
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn stop_disc(&self) -> bool {
        self.stop_disc.load(Ordering::Relaxed)
    }
}

#[derive(Clone, PartialEq)]
enum Status {
    Pending,
    Present,
    Reading { read: u32, fraction: f32 },
    Saved(String),
    /// Saved, but probably damaged (see Acceptance::RepeatDiffers).
    SavedWarn(String),
    Rejected(String),
}

struct Row {
    info: TrackInfo,
    status: Status,
    /// One short line per read ("CRC 1234ABCD AR 200").
    reads: Vec<String>,
}

#[derive(Default)]
struct Disc {
    drive: String,
    model: String,
    offset: i32,
    disc_id: String,
    release: Option<Release>,
    alternatives: usize,
    unknown_album: Option<String>,
    cover: Option<egui::TextureHandle>,
    in_accuraterip: Option<bool>,
    album_dir: String,
    rows: Vec<Row>,
    started: Option<Instant>,
    /// Sectors (1/75 s) of audio done / to do, for overall progress.
    done_secs: u32,
    total_secs: u32,
    result: Option<String>,
    /// The disc has left the drive (after ripping, or after Stop & eject).
    ejected: bool,
    /// Stop & eject was pressed.
    stopping: bool,
}

/// One drive's part of the Rip tab.
#[derive(Default)]
struct DriveView {
    disc: Option<Disc>,
    stage: String,
    /// While its rip loop runs: the flag that stops the current disc.
    stop_disc: Option<Arc<AtomicBool>>,
}

/// A per-drive button pressed in the drive strip (done after drawing).
enum DriveAction {
    StopDisc,
    Eject,
}

struct Choice {
    drive: String,
    candidates: Vec<Release>,
    reply: Sender<ReleaseChoice>,
    selected: usize,
    deadline: Option<Instant>,
}

struct App {
    ctx: egui::Context,
    settings: Settings,
    rx: Option<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
    running: bool,
    stage: String,
    /// The selected drive's disc and stage (swapped in from `drives` while
    /// its events are handled or the Rip tab is drawn).
    disc: Option<Disc>,
    choice: Option<Choice>,
    /// Release questions from other drives, asked one at a time.
    queued_choices: std::collections::VecDeque<Choice>,
    /// Per drive (path): its disc and stage, when not swapped in.
    drives: BTreeMap<String, DriveView>,
    /// The drive the Rip tab shows.
    view: Option<String>,
    /// Rip loops running, and whether the drive watcher is.
    loops: usize,
    watcher: bool,
    /// Whether the selected drive's view is in `self.disc`/`self.stage`.
    swapped: bool,
    /// A drive clicked in the drive strip (switched to after drawing).
    pending_view: Option<String>,
    session: Vec<(bool, String)>,
    log: Vec<String>,
    show_log: bool,
    prefs: Prefs,
    notifier: Arc<Notifier>,
    /// Results of phone notifications, for the log.
    notes: Receiver<String>,
    tab: Tab,
    phone: phone_tab::PhoneTab,
    review: review_tab::ReviewTab,
    /// Running Ejects: drive (None = every idle drive) and the result.
    eject: Vec<(Option<String>, Receiver<Vec<String>>)>,
    /// The music library, the player and their pages.
    library: library_ui::LibraryUi,
    drive_action: Option<(String, DriveAction)>,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let settings = Settings::load();
        let prefs = Prefs::load(&settings);
        let (note_tx, notes) = channel::<String>();
        let (config_dir, repaint) = (settings.config_dir.clone(), ctx.clone());
        let notifier = Notifier::new(
            prefs.notify_phone,
            Duration::from_secs(prefs.notify_after_secs),
            Box::new(move |msg: &str| {
                let note = match phone_notify::notify(&config_dir, msg) {
                    Ok(phone) => format!("Phone notification sent to {phone}: {msg}"),
                    Err(e) => format!("Couldn't notify the phone: {e:#}"),
                };
                let _ = note_tx.send(note);
                repaint.request_repaint();
            }),
        );
        let library = library_ui::LibraryUi::new(&settings, &ctx);
        let mut app = App {
            library,
            ctx,
            settings,
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            running: false,
            stage: String::new(),
            disc: None,
            choice: None,
            queued_choices: Default::default(),
            drives: BTreeMap::new(),
            view: None,
            loops: 0,
            watcher: false,
            swapped: false,
            pending_view: None,
            session: Vec::new(),
            log: Vec::new(),
            show_log: false,
            prefs,
            notifier,
            notes,
            tab: Tab::Library,
            phone: phone_tab::PhoneTab::new(),
            review: review_tab::ReviewTab::new(),
            eject: Vec::new(),
            drive_action: None,
        };
        if std::env::var_os("LOCALBEAT_DEMO").is_some() {
            // Made-up disc for working on the layout without a CD.
            let mut d1 = demo_disc();
            d1.cover = Some(demo_cover(&app.ctx));
            let mut d2 = demo_disc();
            d2.drive = "/dev/sr2".into();
            d2.model = "Apple SuperDrive".into();
            d2.offset = 6;
            d2.release = Some(Release { title: "Grace".into(), artist: "Jeff Buckley".into(), date: "1994-08-23".into(), country: "US".into(), media_count: 1, ..Default::default() });
            d2.done_secs = 300 * 75;
            app.drives.insert("/dev/sr0".into(), DriveView { disc: None, stage: "Waiting for a disc".into(), stop_disc: Some(Default::default()) });
            app.drives.insert("/dev/sr1".into(), DriveView { disc: Some(d1), stage: "Ripping track 5".into(), stop_disc: Some(Default::default()) });
            app.drives.insert("/dev/sr2".into(), DriveView { disc: Some(d2), stage: "Ripping track 2".into(), stop_disc: Some(Default::default()) });
            let demo = std::env::var("LOCALBEAT_DEMO").unwrap_or_default();
            app.view = Some(if demo == "sr2" { "/dev/sr2" } else { "/dev/sr1" }.into());
            app.running = true;
            app.stage = "Ripping on 2 drives".into();
            if demo == "choice" {
                let (reply, _keep) = channel();
                std::mem::forget(_keep);
                let r = |t: &str, c: &str, d: &str| Release { title: t.into(), artist: "Radiohead".into(), country: c.into(), date: d.into(), media_count: 1, ..Default::default() };
                app.choice = Some(Choice {
                    drive: "/dev/sr1".into(),
                    candidates: vec![r("OK Computer", "US", "1997-07-01"), r("OK Computer", "GB", "1997-06-16"), r("OK Computer", "JP", "1997-05-21")],
                    reply,
                    selected: 0,
                    deadline: Some(Instant::now() + Duration::from_secs(AUTO_CHOOSE_SECS)),
                });
            }
            app.library.set_demo_volume();
            app.tab = match demo.as_str() {
                "phone" => Tab::Phone,
                "review" => Tab::Review,
                "1" | "sr2" | "choice" => Tab::Rip,
                _ => Tab::Library,
            };
            app.library.demo = Some(demo.clone());
            app.session = vec![(true, "Radiohead – OK Computer: 12/12".into()), (false, "Lenny Kravitz – Let Love Rule: 11/12".into())];
        } else {
            app.start();
        }
        app
    }

    /// Starts a watcher that runs one rip loop per drive (drives plugged in
    /// later get one too), so several drives rip at the same time.
    fn start(&mut self) {
        let (tx, rx) = channel();
        self.stop = Arc::new(AtomicBool::new(false));
        let (stop, ctx, notifier, settings) = (self.stop.clone(), self.ctx.clone(), self.notifier.clone(), self.settings.clone());
        std::thread::spawn(move || {
            let mut started = std::collections::BTreeSet::new();
            while !stop.load(Ordering::Relaxed) {
                for path in localbeat_platform::drive::drive_paths() {
                    if !started.insert(path.clone()) {
                        continue;
                    }
                    let stop_disc = Arc::new(AtomicBool::new(false));
                    let fe = GuiFrontend {
                        drive: path.clone(),
                        tx: tx.clone(),
                        stop: stop.clone(),
                        stop_disc: stop_disc.clone(),
                        ctx: ctx.clone(),
                        notifier: notifier.clone(),
                    };
                    let (settings, tx) = (settings.clone(), tx.clone());
                    let _ = tx.send(Msg::LoopStarted(path.clone(), stop_disc));
                    std::thread::spawn(move || {
                        let opts = Options { drive: Some(path.clone()), ..Default::default() };
                        let r = engine::rip_loop(&settings, &opts, &fe);
                        let _ = tx.send(Msg::Stopped(path, r.err().map(|e| format!("{e:#}"))));
                        fe.ctx.request_repaint();
                    });
                }
                ctx.request_repaint();
                for _ in 0..15 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            let _ = tx.send(Msg::WatcherDone);
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.running = true;
        self.watcher = true;
        self.stage = "Starting".into();
        self.log("Started: waiting for discs".into());
    }

    /// Runs `f` with `drive`'s view swapped into `self.disc`/`self.stage`.
    fn with_view<R>(&mut self, drive: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        if self.swapped && self.view.as_deref() == Some(drive) {
            return f(self); // already swapped in (while drawing)
        }
        let mut v = self.drives.remove(drive).unwrap_or_default();
        std::mem::swap(&mut self.disc, &mut v.disc);
        std::mem::swap(&mut self.stage, &mut v.stage);
        let r = f(self);
        std::mem::swap(&mut self.disc, &mut v.disc);
        std::mem::swap(&mut self.stage, &mut v.stage);
        self.drives.insert(drive.to_string(), v);
        r
    }

    /// Moves the selected drive's view into `self.disc`/`self.stage` for
    /// drawing (`true`), or back into `drives` (`false`).
    fn swap_view(&mut self, into_self: bool) {
        if self.swapped == into_self {
            return;
        }
        if let Some(d) = self.view.clone() {
            let v = self.drives.entry(d).or_default();
            std::mem::swap(&mut self.disc, &mut v.disc);
            std::mem::swap(&mut self.stage, &mut v.stage);
        }
        self.swapped = into_self;
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.stage = "Stopping...".into();
        if let Some(c) = self.choice.take() {
            let _ = c.reply.send(ReleaseChoice::Skip);
        }
        for c in self.queued_choices.drain(..) {
            let _ = c.reply.send(ReleaseChoice::Skip);
        }
    }

    /// A per-drive button from the drive strip (views not swapped in).
    fn drive_action(&mut self, drive: String, action: DriveAction) {
        match action {
            DriveAction::StopDisc => {
                let Some(flag) = self.drives.get(&drive).and_then(|v| v.stop_disc.clone()) else { return };
                flag.store(true, Ordering::Relaxed);
                // Its release question, if any, goes away.
                if self.choice.as_ref().is_some_and(|c| c.drive == drive) {
                    if let Some(c) = self.choice.take() {
                        let _ = c.reply.send(ReleaseChoice::Skip);
                    }
                }
                let (theirs, rest): (Vec<_>, Vec<_>) = self.queued_choices.drain(..).partition(|c| c.drive == drive);
                self.queued_choices = rest.into();
                for c in theirs {
                    let _ = c.reply.send(ReleaseChoice::Skip);
                }
                self.with_view(&drive, |a| {
                    if let Some(d) = &mut a.disc {
                        d.stopping = true;
                    }
                    a.stage = "Stopping…".into();
                });
                self.log(format!("{drive}: stopping this disc"));
            }
            DriveAction::Eject => {
                let (tx, rx) = channel();
                let (settings, ctx, path) = (self.settings.clone(), self.ctx.clone(), drive.clone());
                std::thread::spawn(move || {
                    let line = match engine::eject_one(&settings, &path, true) {
                        Ok(engine::EjectOutcome::Ejected) => format!("Ejected {path}"),
                        Ok(engine::EjectOutcome::Busy) => format!("{path} is busy (being ripped or making phone copies): try again in a moment"),
                        Ok(engine::EjectOutcome::NoDisc) => format!("No CD in {path}"),
                        Err(e) => format!("Couldn't eject {path}: {e:#}"),
                    };
                    let _ = tx.send(vec![line]);
                    ctx.request_repaint();
                });
                self.eject.push((Some(drive), rx));
            }
        }
    }

    /// The left navigation: wordmark, devices, tools, log.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let frame = egui::Frame::new().fill(p.base).inner_margin(egui::Margin { left: 14, right: 14, top: 20, bottom: 14 });
        egui::Panel::left("nav").resizable(false).exact_size(236.0).frame(frame).show_separator_line(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                // The LocalBeat mark (as on the phone), then "LocalBeat" in
                // the prism and "Studio" quieter, on one baseline.
                let (mark, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                theme::logo(ui.painter(), mark, false);
                ui.add_space(3.0);
                let g = theme::prism_text(ui, "LocalBeat", theme::bold(22.0));
                let studio = ui.painter().layout_no_wrap("Studio".into(), theme::medium(15.0), p.dim);
                let size = egui::vec2(g.size().x + 7.0 + studio.size().x, g.size().y);
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                // Bottoms lined up, nudged so the baselines meet.
                let studio_y = rect.min.y + g.size().y - studio.size().y - 2.5;
                let x = rect.min.x + g.size().x + 7.0;
                ui.painter().galley(rect.min, g, p.text);
                ui.painter().galley(egui::pos2(x, studio_y), studio, p.dim);
            });
            ui.horizontal(|ui| {
                ui.add_space(9.0);
                ui.add_space(38.0);
                ui.label(egui::RichText::new("rip · verify · play · carry").small().color(p.faint));
            });
            ui.add_space(16.0);
            // Search: the library, as you type.
            let search = egui::Frame::new()
                .fill(p.card)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(10.0)
                .inner_margin(egui::Margin { left: 10, right: 8, top: 5, bottom: 5 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        theme::paint_icon(ui.painter(), r, theme::Icon::Search, p.faint);
                        let edit = egui::TextEdit::singleline(&mut self.library.search)
                            .id(self.library.search_id)
                            .hint_text("Search   Ctrl+F")
                            .frame(egui::Frame::NONE)
                            .desired_width(ui.available_width());
                        ui.add(edit)
                    })
                    .inner
                })
                .inner;
            if search.changed() && !self.library.search.is_empty() {
                self.tab = Tab::Library;
            }
            if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.library.search.clear();
            }
            ui.add_space(16.0);

            ui.horizontal(|ui| {
                ui.add_space(8.0);
                theme::section_label(ui, "Library");
            });
            let lib_counts = self.library.lib.as_ref().map(|l| (l.albums.len(), l.artists.len(), l.songs.len()));
            for (page, icon, label, count) in [
                (library_ui::Page::Albums, theme::Icon::Albums, "Albums", lib_counts.map(|c| c.0)),
                (library_ui::Page::Recent, theme::Icon::Clock, "Recently added", None),
                (library_ui::Page::Artists, theme::Icon::Artists, "Artists", lib_counts.map(|c| c.1)),
                (library_ui::Page::Songs, theme::Icon::Songs, "Songs", lib_counts.map(|c| c.2)),
            ] {
                let selected = self.tab == Tab::Library && self.library.page == page && self.library.search.is_empty();
                let n = count.map(|n| n.to_string()).unwrap_or_default();
                let status = (!n.is_empty()).then_some((n.as_str(), p.faint));
                if theme::nav_item(ui, icon, label, selected, status).clicked() {
                    self.tab = Tab::Library;
                    self.library.page = page;
                    self.library.album = None;
                    self.library.search.clear();
                }
            }
            ui.add_space(16.0);

            ui.horizontal(|ui| {
                ui.add_space(8.0);
                theme::section_label(ui, "Devices");
            });
            // The ripper's state at a glance.
            let ripping: Vec<f32> = self
                .drives
                .iter()
                .filter_map(|(path, v)| {
                    let d = if self.swapped && self.view.as_deref() == Some(path.as_str()) { &self.disc } else { &v.disc };
                    d.as_ref().filter(|d| d.result.is_none() && d.total_secs > 0).map(|d| d.done_secs as f32 / d.total_secs as f32)
                })
                .collect();
            let status = if !ripping.is_empty() {
                let pct = ripping.iter().sum::<f32>() / ripping.len() as f32 * 100.0;
                (format!("{pct:.0}%"), theme::prism_at(0.25))
            } else if self.running {
                (String::new(), theme::OK)
            } else {
                (String::new(), p.faint)
            };
            let hint = if !ripping.is_empty() {
                format!("{} disc(s) ripping", ripping.len())
            } else if self.running {
                "Waiting for a disc".to_string()
            } else {
                "Ripping is stopped".to_string()
            };
            if theme::nav_item(ui, theme::Icon::Disc, "CD Ripper", self.tab == Tab::Rip, Some((&status.0, status.1))).on_hover_text(hint).clicked() {
                self.tab = Tab::Rip;
            }
            if theme::nav_item(ui, theme::Icon::Phone, "Phone", self.tab == Tab::Phone, None).clicked() {
                self.tab = Tab::Phone;
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                theme::section_label(ui, "Tools");
            });
            if theme::nav_item(ui, theme::Icon::Tag, "Review tags", self.tab == Tab::Review, None).clicked() {
                self.tab = Tab::Review;
            }

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                if theme::nav_item(ui, theme::Icon::Log, "Activity log", self.show_log, None).clicked() {
                    self.show_log = !self.show_log;
                }
            });
        });
    }

    fn log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 5000 {
            self.log.drain(..1000);
        }
    }

    fn poll(&mut self) {
        let notes: Vec<String> = self.notes.try_iter().collect();
        for n in notes {
            self.log(n);
        }
        let Some(rx) = &self.rx else { return };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        for m in msgs {
            match m {
                Msg::Event(drive, e) => {
                    let starts = matches!(e, Event::DiscStart { .. });
                    self.with_view(&drive, |a| a.on_event(e));
                    // Show a drive when a disc starts there, unless the one
                    // shown is in the middle of a disc.
                    let busy = |v: &DriveView| v.disc.as_ref().is_some_and(|d| d.result.is_none());
                    let shown_busy = match &self.view {
                        Some(v) if self.swapped => self.disc.as_ref().is_some_and(|d| d.result.is_none()) && v != &drive,
                        Some(v) => v != &drive && self.drives.get(v).is_some_and(busy),
                        None => false,
                    };
                    if starts && !shown_busy {
                        self.select_view(drive);
                    }
                }
                Msg::LoopStarted(drive, stop_disc) => {
                    self.loops += 1;
                    let v = self.drives.entry(drive.clone()).or_default();
                    v.stage = "Waiting for a disc".into();
                    v.stop_disc = Some(stop_disc);
                    if self.view.is_none() {
                        self.select_view(drive);
                    }
                }
                Msg::Choose { drive, candidates, reply } => {
                    self.queued_choices.push_back(Choice { drive, candidates, reply, selected: 0, deadline: None });
                }
                Msg::Stopped(drive, err) => {
                    self.loops = self.loops.saturating_sub(1);
                    if let Some(v) = self.drives.get_mut(&drive) {
                        v.stop_disc = None;
                    }
                    let line = match &err {
                        Some(e) => format!("{drive}: stopped with an error: {e}"),
                        None => format!("{drive}: stopped"),
                    };
                    self.log(line);
                    self.with_view(&drive, |a| a.stage = err.map(|e| format!("Stopped: {e}")).unwrap_or_else(|| "Stopped".into()));
                }
                Msg::WatcherDone => self.watcher = false,
            }
        }
        if !self.watcher && self.loops == 0 && self.running {
            self.running = false;
            self.rx = None;
            self.log("Stopped".into());
        }
        // Ask the next release question once the previous one is answered.
        if self.choice.is_none() {
            if let Some(mut c) = self.queued_choices.pop_front() {
                if c.candidates.first().is_some_and(|r| !r.fuzzy) {
                    c.deadline = Some(Instant::now() + Duration::from_secs(AUTO_CHOOSE_SECS));
                }
                self.choice = Some(c);
            }
        }
    }

    /// Shows `drive` on the Rip tab.
    fn select_view(&mut self, drive: String) {
        let was = self.swapped;
        self.swap_view(false);
        self.view = Some(drive);
        self.swap_view(was);
    }

    fn row(&mut self, position: usize) -> Option<&mut Row> {
        self.disc.as_mut()?.rows.iter_mut().find(|r| r.info.position == position)
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Waiting { done } => {
                self.stage = if done == 0 { "Waiting for a disc".into() } else { format!("Waiting for the next disc ({done} done)") };
            }
            Event::DiscStart { drive, model, offset } => {
                self.log(format!("── New disc in {drive} ({model}, offset {offset:+})"));
                self.disc = Some(Disc { drive, model, offset, started: Some(Instant::now()), ..Default::default() });
            }
            Event::Stage(s) => self.stage = s,
            Event::Identified { disc_id, tracks } => {
                self.log(format!("{tracks} tracks, disc ID {disc_id}"));
                if let Some(d) = &mut self.disc {
                    d.disc_id = disc_id;
                }
            }
            Event::Release { release, alternatives } => {
                self.log(format!("Release: {}", release.summary()));
                if let Some(d) = &mut self.disc {
                    d.release = Some(*release);
                    d.alternatives = alternatives;
                }
            }
            Event::UnknownDisc { album } => {
                self.log(format!("Not identified: ripping as {album}"));
                if let Some(d) = &mut self.disc {
                    d.unknown_album = Some(album);
                }
            }
            Event::Cover(p) => {
                let tex = p.and_then(|p| texture(&self.ctx, &p));
                if let Some(d) = &mut self.disc {
                    d.cover = tex;
                }
            }
            Event::AccurateRip { in_database } => {
                if !in_database {
                    self.log("Disc not in AccurateRip: tracks verify by two identical reads".into());
                }
                if let Some(d) = &mut self.disc {
                    d.in_accuraterip = Some(in_database);
                }
            }
            Event::Plan { album_dir, tracks, .. } => {
                if let Some(d) = &mut self.disc {
                    d.album_dir = album_dir.display().to_string();
                    d.total_secs = tracks.iter().filter(|t| !t.exists).map(|t| t.seconds).sum();
                    d.rows = tracks
                        .into_iter()
                        .map(|info| Row {
                            status: if info.exists { Status::Present } else { Status::Pending },
                            info,
                            reads: Vec::new(),
                        })
                        .collect();
                }
            }
            Event::Skipped { position } => {
                if let Some(r) = self.row(position) {
                    r.status = Status::Present;
                }
            }
            Event::ReadStart { position, read } => {
                if let Some(r) = self.row(position) {
                    r.status = Status::Reading { read, fraction: 0.0 };
                }
            }
            Event::Progress { position, read, done, total } => {
                if let Some(r) = self.row(position) {
                    r.status = Status::Reading { read, fraction: done as f32 / total.max(1) as f32 };
                }
            }
            Event::ReadDone { position, read, crc, verdict, unreadable, repeat } => {
                let ar = match verdict {
                    Verdict::Match { confidence, .. } => format!("AccurateRip ✔ ({confidence})"),
                    Verdict::NoMatch => "no AR match".into(),
                    Verdict::NotInDatabase => "not in AR".into(),
                };
                let extra = match (unreadable, repeat) {
                    (0, true) => ", same as before".to_string(),
                    (0, false) => String::new(),
                    (n, _) => format!(", {n} unreadable sectors"),
                };
                let line = format!("read {read}: {crc:08X} {ar}{extra}");
                self.log(format!("Track {position} {line}"));
                if let Some(r) = self.row(position) {
                    r.reads.push(line);
                }
            }
            Event::Accepted { position, how } => {
                let text = match how {
                    Acceptance::AccurateRip { confidence } => format!("✔ AccurateRip ({confidence})"),
                    Acceptance::OtherPressing { confidence, shift } => format!("✔ AccurateRip ({confidence}, other pressing {shift:+})"),
                    Acceptance::Repeat { reads } => format!("✔ identical reads ({reads})"),
                    Acceptance::Voted { confidence, reads, sectors } => {
                        format!("✔ AccurateRip ({confidence}) by sector vote ({sectors} damaged sectors, {reads} reads)")
                    }
                    Acceptance::RepeatDiffers { reads } => {
                        self.log(format!(
                            "Track {position}: saved after {reads} reads, but it differs from AccurateRip's copy of this pressing: \
                             probably a damaged spot read the same wrong way each time. Clean the disc and re-rip it."
                        ));
                        format!("⚠ identical reads ({reads}), differs from AccurateRip: clean & re-rip")
                    }
                };
                let status = if matches!(how, Acceptance::RepeatDiffers { .. }) { Status::SavedWarn(text) } else { Status::Saved(text) };
                self.finish_track(position, status);
            }
            Event::Rejected { position, reads, .. } => {
                self.log(format!("Track {position} REJECTED after {reads} reads"));
                self.finish_track(position, Status::Rejected(format!("✘ rejected ({reads} reads)")));
            }
            Event::RipDone { accepted, rejected, skipped, elapsed } => {
                let text = format!(
                    "{accepted} saved, {} rejected, {skipped} already present, {} min",
                    rejected.len(),
                    elapsed.as_secs() / 60
                );
                self.log(format!("Done: {text}"));
                if let Some(d) = &mut self.disc {
                    d.result = Some(text);
                }
            }
            Event::Ejected { ok } => {
                if !ok {
                    self.log("Couldn't eject the disc".into());
                }
                if let Some(d) = &mut self.disc {
                    d.ejected = ok;
                }
            }
            Event::PhoneFile { name, error } => match error {
                None => self.log(format!("Phone copy: {name}")),
                Some(e) => self.log(format!("Phone copy FAILED: {name}: {e}")),
            },
            Event::PhoneDone { converted, failed } => {
                if converted + failed > 0 {
                    self.log(format!("Phone copies: {converted} made, {failed} failed"));
                }
            }
            Event::DiscDone { ok } => {
                // New songs for the library.
                self.library.rescan(&self.ctx.clone());
                let label = self.disc_label();
                let result = self.disc.as_ref().and_then(|d| d.result.clone()).unwrap_or_default();
                self.session.push((ok, format!("{label}: {result}")));
            }
            Event::DiscFailed(e) => {
                let label = self.disc_label();
                self.log(format!("Disc failed: {e}"));
                self.session.push((false, format!("{label}: {e}")));
                if let Some(d) = &mut self.disc {
                    d.result = Some(format!("Failed: {e}"));
                }
            }
            Event::DiscStopped => {
                let label = self.disc_label();
                self.log(format!("Stopped {label}: ejecting it"));
                self.session.push((false, format!("{label}: stopped")));
                if let Some(d) = &mut self.disc {
                    let saved = d.rows.iter().filter(|r| matches!(r.status, Status::Saved(_) | Status::SavedWarn(_))).count();
                    d.result = Some(format!("Stopped ({saved} tracks saved; put it back in to rip the rest)"));
                    for r in &mut d.rows {
                        if matches!(r.status, Status::Reading { .. }) {
                            r.status = Status::Pending;
                        }
                    }
                }
            }
            Event::Note(n) => self.log(n),
        }
    }

    fn finish_track(&mut self, position: usize, status: Status) {
        if let Some(d) = &mut self.disc {
            if let Some(r) = d.rows.iter_mut().find(|r| r.info.position == position) {
                d.done_secs += r.info.seconds;
                r.status = status;
            }
        }
    }

    fn disc_label(&self) -> String {
        match &self.disc {
            Some(d) => match (&d.release, &d.unknown_album) {
                (Some(r), _) => format!("{} – {}", r.artist, r.title),
                (None, Some(a)) => a.clone(),
                _ => format!("Disc {}", d.disc_id),
            },
            None => "Disc".into(),
        }
    }

    /// Overall fraction done and an ETA, counting the current read.
    fn overall(&self) -> Option<(f32, Option<Duration>)> {
        let d = self.disc.as_ref()?;
        if d.total_secs == 0 || d.result.is_some() {
            return None;
        }
        let current: f32 = d
            .rows
            .iter()
            .filter_map(|r| match r.status {
                Status::Reading { fraction, .. } => Some(fraction * r.info.seconds as f32),
                _ => None,
            })
            .sum();
        let frac = ((d.done_secs as f32 + current) / d.total_secs as f32).clamp(0.0, 1.0);
        let eta = d.started.and_then(|s| {
            let el = s.elapsed().as_secs_f32();
            (frac > 0.02).then(|| Duration::from_secs_f32(el / frac * (1.0 - frac)))
        });
        Some((frac, eta))
    }
}

fn texture(ctx: &egui::Context, p: &Picture) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(&p.data).ok()?.thumbnail(512, 512).to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("cover", color, egui::TextureOptions::LINEAR))
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        if std::env::var_os("LOCALBEAT_DEMO").is_none() {
            self.library.save_session();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.swap_view(true);
        self.draw(ui);
        self.swap_view(false);
        if let Some(v) = self.pending_view.take() {
            self.select_view(v);
        }
        if let Some((drive, action)) = self.drive_action.take() {
            self.drive_action(drive, action);
        }
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        for line in self.phone.poll(&self.settings, &mut self.prefs, ctx) {
            self.log(line);
        }
        for line in self.review.poll(ctx) {
            self.log(line);
        }
        for line in self.library.poll(ctx) {
            self.log(line);
        }
        if self.tab != Tab::Phone {
            self.library.keyboard(ctx);
        }
        let mut done = Vec::new();
        self.eject.retain(|(_, rx)| match rx.try_recv() {
            Ok(lines) => {
                done.extend(lines);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for l in done {
            self.log(l);
        }
        if self.running || self.choice.is_some() {
            // Keep the countdown and progress moving.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        let p = theme::pal(ui);
        egui::Panel::bottom("player")
            .exact_size(94.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.base))
            .show(ui, |ui| self.library.now_playing(ui));
        // egui reserves a little less than a bottom panel draws (the panels
        // drawn next overlap its top); the bar lays itself out below the
        // point where they really end.
        self.library.bar_top = Some(ui.available_rect_before_wrap().bottom());
        self.sidebar(ui);

        let p = theme::pal(ui);
        egui::Panel::bottom("bottom")
            .resizable(true)
            .frame(egui::Frame::new().fill(p.base).inner_margin(egui::Margin::symmetric(16, 10)))
            .show_collapsible(ui, &mut self.show_log, |ui| {
                ui.horizontal(|ui| {
                    theme::section_label(ui, "Activity log");
                });
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    for l in &self.log {
                        ui.label(egui::RichText::new(l).monospace().color(p.dim));
                    }
                });
            });

        let page = egui::Frame::new().fill(p.content).inner_margin(egui::Margin { left: 28, right: 28, top: 22, bottom: 16 });
        if self.tab == Tab::Library {
            if self.library.queue_open {
                egui::Panel::right("upnext")
                    .resizable(false)
                    .exact_size(330.0)
                    .frame(egui::Frame::new().fill(p.base).inner_margin(egui::Margin::same(16)))
                    .show(ui, |ui| self.library.queue_panel(ui));
            }
            egui::CentralPanel::default().frame(page).show(ui, |ui| self.library.page_ui(ui));
            self.choice_window(ctx);
            return;
        }
        if self.tab == Tab::Review {
            let mut lines = Vec::new();
            egui::CentralPanel::default().frame(page).show(ui, |ui| {
                theme::page_header(ui, "Review tags", "Albums the tagger wasn't sure about: accept a match or leave them as they are", |_| {});
                lines = self.review.ui(ui, &self.settings);
            });
            for l in lines {
                self.log(l);
            }
            self.choice_window(ctx);
            return;
        }
        if self.tab == Tab::Phone {
            egui::CentralPanel::default().frame(page).show(ui, |ui| {
                theme::page_header(ui, "Phone", "Pick the albums to keep on the phone, then sync", |_| {});
                self.phone.ui(ui, &self.settings, &mut self.prefs);
            });
            self.choice_window(ctx);
            return;
        }

        egui::Panel::top("riphead").show_separator_line(false).frame(page.inner_margin(egui::Margin { left: 28, right: 28, top: 22, bottom: 6 })).show(ui, |ui| {
            let subtitle = if self.running { self.stage.clone() } else { "Ripping is stopped".to_string() };
            theme::page_header(ui, "CD Ripper", &subtitle, |ui| {
                // Right to left: the all-drive controls, grouped.
                let hint = format!(
                    "Through KDE Connect (phone on the same Wi-Fi): once a drive has waited {} s for the \
                     next disc, say how the last one went. Problems are sent straight away.",
                    self.prefs.notify_after_secs
                );
                if theme::toggle(ui, &mut self.prefs.notify_phone, "Notify phone").on_hover_text(hint).changed() {
                    self.notifier.set_enabled(self.prefs.notify_phone);
                    let state = if self.prefs.notify_phone { "on" } else { "off" };
                    match self.prefs.save(&self.settings) {
                        Ok(()) => self.log(format!("Phone notifications {state}")),
                        Err(e) => self.log(format!("Phone notifications {state} (couldn't save the setting: {e:#})")),
                    }
                }
                ui.add_space(12.0);
                let ejecting = !self.eject.is_empty();
                let hint = "Eject the CD from every drive that isn't being ripped. To eject a disc that's being ripped, use its drive's Stop & eject.";
                let r = ui.add_enabled_ui(!ejecting, |ui| {
                    theme::button(ui, Some(theme::Icon::Eject), if ejecting { "Ejecting…" } else { "Eject all" }, theme::Kind::Secondary)
                });
                if r.inner.on_hover_text(hint).clicked() {
                    let (tx, rx) = channel();
                    let (settings, ctx) = (self.settings.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let r = engine::eject_idle(&settings);
                        let mut lines: Vec<String> = r.ejected.iter().map(|p| format!("Ejected {p}")).collect();
                        lines.extend(r.busy.iter().map(|p| format!("{p} is being ripped: use its Stop & eject")));
                        lines.extend(r.failed.iter().map(|(p, e)| format!("Couldn't eject {p}: {e}")));
                        if lines.is_empty() {
                            lines.push("No CD to eject".into());
                        }
                        let _ = tx.send(lines);
                        ctx.request_repaint();
                    });
                    self.eject.push((None, rx));
                    self.show_log = true;
                }
                if self.running {
                    if theme::button(ui, Some(theme::Icon::Stop), "Stop all", theme::Kind::Danger)
                        .on_hover_text("Stop every drive (discs stay in; saved tracks are kept)")
                        .clicked()
                    {
                        self.stop();
                    }
                } else if theme::button(ui, Some(theme::Icon::Play), "Start ripping", theme::Kind::Primary).clicked() {
                    self.start();
                }
            });
            if let Some((frac, eta)) = self.overall() {
                let text = match eta {
                    Some(e) => format!("{:.0}% · {} left", frac * 100.0, fmt_duration(e)),
                    None => format!("{:.0}%", frac * 100.0),
                };
                theme::progress(ui, frac, 260.0, Some(&text));
                ui.add_space(4.0);
            }
        });

        let time = ui.input(|i| i.time);
        let mut spinning = false;
        if !self.drives.is_empty() {
            let f = egui::Frame::new().fill(p.content).inner_margin(egui::Margin { left: 28, right: 28, top: 4, bottom: 14 });
            egui::Panel::top("drives").frame(f).show_separator_line(false).show(ui, |ui| {
                // As many cards per row as fit (at least 280 px each), all the same width.
                let gap = 12.0;
                let avail = ui.available_width();
                let n = self.drives.len();
                let per_row = (((avail + gap) / (280.0 + gap)).floor() as usize).clamp(1, n);
                let card_w = (avail - gap * (per_row - 1) as f32) / per_row as f32;
                let all_ejecting = self.eject.iter().any(|(d, _)| d.is_none());
                let drives: Vec<(&String, &DriveView)> = self.drives.iter().collect();
                for row in drives.chunks(per_row) {
                  ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
                    for &(path, v) in row {
                        let shown = self.view.as_deref() == Some(path.as_str());
                        let (disc, stage) = if shown { (&self.disc, &self.stage) } else { (&v.disc, &v.stage) };
                        // Ripping = its loop is on a disc that's still in the drive.
                        let ripping = v.stop_disc.is_some() && disc.as_ref().is_some_and(|d| d.result.is_none() && !d.ejected);
                        let stopping = disc.as_ref().is_some_and(|d| d.stopping) && ripping;
                        let ejecting = all_ejecting || self.eject.iter().any(|(d, _)| d.as_deref() == Some(path.as_str()));
                        spinning |= ripping;
                        let deck = Deck { path, disc: disc.as_ref(), stage, shown, ripping, stopping, ejecting, time };
                        match deck_card(ui, &deck, card_w) {
                            Some(DeckClick::Select) => self.pending_view = Some(path.clone()),
                            Some(DeckClick::StopDisc) => self.drive_action = Some((path.clone(), DriveAction::StopDisc)),
                            Some(DeckClick::Eject) => self.drive_action = Some((path.clone(), DriveAction::Eject)),
                            None => {}
                        }
                    }
                  });
                  ui.add_space(gap - ui.spacing().item_spacing.y);
                }
            });
        }

        let f = egui::Frame::new().fill(p.content).inner_margin(egui::Margin { left: 28, right: 16, top: 4, bottom: 16 });
        egui::Panel::left("album").resizable(false).exact_size(340.0).frame(f).show_separator_line(false).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let dark = ui.visuals().dark_mode;
                match &self.disc {
                    None => {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(284.0, 284.0), egui::Sense::hover());
                        theme::disc(ui.painter(), rect.center(), 136.0, None, None, dark);
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("No disc yet").font(theme::bold(24.0)).color(p.text));
                        ui.label(egui::RichText::new("Put a CD in any drive. It's ripped, verified, tagged, filed in the library and ejected.").color(p.dim));
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(format!("Library  {}", self.settings.flac_dir.display())).small().color(p.faint));
                    }
                    Some(d) => {
                        let ripping = d.result.is_none() && !d.ejected && self.running;
                        spinning |= ripping;
                        match &d.cover {
                            Some(t) => {
                                let shadow = egui::Shadow { offset: [0, 14], blur: 36, spread: 0, color: egui::Color32::from_black_alpha(140) };
                                egui::Frame::new().shadow(shadow).corner_radius(14.0).show(ui, |ui| {
                                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(284.0, 284.0)).corner_radius(14.0));
                                });
                            }
                            None => {
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(284.0, 284.0), egui::Sense::hover());
                                let spin = ripping.then_some((time * 1.6) as f32);
                                theme::disc(ui.painter(), rect.center(), 136.0, spin, None, dark);
                            }
                        }
                        ui.add_space(14.0);
                        match (&d.release, &d.unknown_album) {
                            (Some(r), _) => {
                                ui.label(egui::RichText::new(&r.title).font(theme::bold(24.0)).color(p.text));
                                ui.label(egui::RichText::new(&r.artist).font(theme::medium(16.0)).color(theme::prism_at(0.3)));
                                let mut bits = vec![r.date.clone(), r.country.clone()];
                                bits.extend(r.labels.first().cloned());
                                bits.retain(|b| !b.is_empty());
                                ui.label(egui::RichText::new(bits.join(" · ")).color(p.dim));
                                if d.alternatives > 0 {
                                    ui.label(egui::RichText::new(format!("{} other matching releases", d.alternatives)).small().color(p.faint));
                                }
                            }
                            (None, Some(a)) => {
                                ui.label(egui::RichText::new(a).font(theme::bold(24.0)).color(p.text));
                                ui.label(egui::RichText::new("Not in MusicBrainz").color(p.dim));
                            }
                            _ => {
                                ui.label(egui::RichText::new("Identifying…").font(theme::bold(24.0)).color(p.text));
                            }
                        }
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            if let Some(r) = &d.release {
                                if r.media_count > 1 {
                                    theme::badge(ui, &format!("Disc {} of {}", r.medium.position, r.media_count), theme::prism_at(0.1));
                                }
                            }
                            match d.in_accuraterip {
                                Some(true) => {
                                    theme::badge(ui, "AccurateRip", theme::OK).on_hover_text("This disc is in the AccurateRip database: tracks are checked against other people's rips");
                                }
                                Some(false) => {
                                    theme::badge(ui, "Not in AccurateRip", theme::WARN).on_hover_text("Verifying by repeat reads instead");
                                }
                                None => {}
                            }
                            theme::badge(ui, &format!("offset {:+}", d.offset), p.dim).on_hover_text(format!("{} · {}", d.drive.trim_start_matches(r"\\.\"), d.model));
                        });
                        if let Some(res) = &d.result {
                            ui.add_space(10.0);
                            theme::card(ui).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(egui::RichText::new(res).font(theme::medium(14.5)).color(p.text));
                            });
                        }
                    }
                }
                if !self.session.is_empty() {
                    ui.add_space(18.0);
                    theme::section_label(ui, "This session");
                    ui.add_space(2.0);
                    for (ok, s) in self.session.iter().rev() {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                            let (icon, color) = if *ok { (theme::Icon::Check, theme::OK) } else { (theme::Icon::Cross, theme::ERR) };
                            theme::paint_icon(ui.painter(), rect, icon, color);
                            ui.label(egui::RichText::new(s).color(p.dim));
                        });
                    }
                }
            });
        });

        let f = egui::Frame::new().fill(p.content).inner_margin(egui::Margin { left: 12, right: 28, top: 4, bottom: 16 });
        egui::CentralPanel::default().frame(f).show(ui, |ui| {
            let Some(d) = &self.disc else {
                // Nothing to show yet: say what's happening, centred.
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.28);
                    let (title, hint) = if self.running {
                        ("Ready for a disc", "Put a CD in any drive. It's ripped, checked against AccurateRip, tagged and ejected, and the next one starts by itself.")
                    } else {
                        ("Ripping is stopped", "Press Start ripping to watch the drives for discs.")
                    };
                    ui.label(egui::RichText::new(title).font(theme::bold(26.0)).color(p.text));
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(hint).color(p.dim));
                });
                return;
            };
            track_list(ui, d, time);
        });
        if spinning && self.tab == Tab::Rip {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        self.choice_window(ctx);
    }
}

impl App {
    fn choice_window(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.choice else { return };
        let mut answer: Option<ReleaseChoice> = None;
        if let Some(dl) = c.deadline {
            if Instant::now() >= dl {
                answer = Some(ReleaseChoice::Use(c.selected));
            }
        }
        let p = theme::pal_ctx(ctx);
        egui::Window::new("Which release is this?")
            .id(egui::Id::unique("release-choice"))
            .collapsible(false)
            .resizable(true)
            .title_bar(false)
            .default_width(600.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Which release is this?").font(theme::bold(22.0)).color(p.text));
                ui.label(egui::RichText::new(format!("Disc in {}", c.drive.trim_start_matches("/dev/"))).color(p.faint));
                ui.add_space(6.0);
                if c.candidates.is_empty() {
                    ui.label(egui::RichText::new("MusicBrainz doesn't know this disc.").color(p.text));
                    ui.label(egui::RichText::new("Rip it as \"Unknown Artist / Unknown Album\", or skip it (it will be ejected).").color(p.dim));
                } else {
                    let intro = if c.candidates[0].fuzzy {
                        "MusicBrainz doesn't have this exact disc. These releases have a similar track layout; the track names may not fit.".to_string()
                    } else {
                        format!("{} releases match this disc. The first is the best guess.", c.candidates.len())
                    };
                    ui.label(egui::RichText::new(intro).color(p.dim));
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                        for (i, r) in c.candidates.iter().enumerate() {
                            let sel = c.selected == i;
                            let resp = egui::Frame::new()
                                .fill(if sel { p.card_hover } else { p.card })
                                .stroke(egui::Stroke::new(if sel { 1.5 } else { 1.0 }, if sel { theme::ACCENT } else { p.line }))
                                .corner_radius(10.0)
                                .inner_margin(egui::Margin::symmetric(12, 8))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(r.summary()).color(p.text));
                                        if r.fuzzy {
                                            theme::badge(ui, "similar TOC", theme::WARN);
                                        }
                                        if i == 0 && !r.fuzzy {
                                            theme::badge(ui, "best guess", theme::OK);
                                        }
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if resp.clicked() {
                                c.selected = i;
                                c.deadline = None;
                            }
                            ui.add_space(4.0);
                        }
                    });
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if !c.candidates.is_empty() && theme::button(ui, Some(theme::Icon::Check), "Use this release", theme::Kind::Primary).clicked() {
                        answer = Some(ReleaseChoice::Use(c.selected));
                    }
                    if theme::button(ui, None, "Rip as unknown", theme::Kind::Secondary).clicked() {
                        answer = Some(ReleaseChoice::Unknown);
                    }
                    if theme::button(ui, Some(theme::Icon::Eject), "Skip disc", theme::Kind::Danger).clicked() {
                        answer = Some(ReleaseChoice::Skip);
                    }
                });
                if let Some(dl) = c.deadline {
                    let left = dl.saturating_duration_since(Instant::now()).as_secs_f32();
                    ui.add_space(6.0);
                    theme::progress(ui, 1.0 - left / AUTO_CHOOSE_SECS as f32, 200.0, Some(&format!("Using the first in {:.0} s unless you choose", left.ceil())));
                }
            });
        if let Some(a) = answer {
            if let Some(c) = self.choice.take() {
                let _ = c.reply.send(a);
            }
        }
    }
}

/// Counts for the line above the track list.
fn summary_line(d: &Disc) -> String {
    let n = d.rows.len();
    let count = |f: fn(&Status) -> bool| d.rows.iter().filter(|r| f(&r.status)).count();
    let saved = count(|s| matches!(s, Status::Saved(_) | Status::SavedWarn(_)));
    let warn = count(|s| matches!(s, Status::SavedWarn(_)));
    let present = count(|s| matches!(s, Status::Present));
    let rejected = count(|s| matches!(s, Status::Rejected(_)));
    let mut bits = vec![format!("{n} tracks")];
    if saved > 0 {
        bits.push(format!("{saved} saved"));
    }
    if warn > 0 {
        bits.push(format!("{warn} to check"));
    }
    if present > 0 {
        bits.push(format!("{present} already in library"));
    }
    if rejected > 0 {
        bits.push(format!("{rejected} rejected"));
    }
    let left = n - saved - present - rejected;
    if left > 0 {
        bits.push(format!("{left} to go"));
    }
    bits.join(" · ")
}

/// What a drive card shows.
struct Deck<'a> {
    path: &'a str,
    disc: Option<&'a Disc>,
    stage: &'a str,
    /// The drive the page shows.
    shown: bool,
    ripping: bool,
    stopping: bool,
    ejecting: bool,
    time: f64,
}

enum DeckClick {
    Select,
    StopDisc,
    Eject,
}

/// One drive: its disc (spinning while read), name, what it's doing, and
/// its own controls. Click the card to show that drive below.
fn deck_card(ui: &mut egui::Ui, d: &Deck, width: f32) -> Option<DeckClick> {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let mut click = None;
    let name = d.path.trim_start_matches(r"\\.\").trim_start_matches("/dev/");
    let frame = egui::Frame::new()
        .fill(if d.shown { p.card_hover } else { p.card })
        .stroke(egui::Stroke::new(if d.shown { 1.5 } else { 1.0 }, if d.shown { theme::ACCENT } else { p.line }))
        .corner_radius(16.0)
        .inner_margin(egui::Margin { left: 12, right: 14, top: 12, bottom: 12 });
    let r = frame.show(ui, |ui| {
        ui.set_width(width - 26.0);
        ui.set_height(98.0);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(72.0, 72.0), egui::Sense::hover());
            let has_disc = d.disc.is_some_and(|x| !x.ejected);
            if has_disc {
                let cover = d.disc.and_then(|x| x.cover.as_ref()).map(|t| t.id());
                let spin = d.ripping.then_some((d.time * 1.6) as f32);
                theme::disc(ui.painter(), rect.center(), 34.0, spin, cover, dark);
            } else {
                // Empty drive: a faint outline where the disc would be.
                ui.painter().circle_stroke(rect.center(), 34.0, egui::Stroke::new(1.5, p.line));
                ui.painter().circle_stroke(rect.center(), 8.0, egui::Stroke::new(1.0, p.line));
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.set_width((width - 26.0 - 76.0 - 56.0).max(100.0));
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(name).font(theme::bold(16.0)).color(p.text));
                    if let Some(m) = d.disc.map(|x| x.model.as_str()).filter(|m| !m.is_empty()) {
                        ui.add(egui::Label::new(egui::RichText::new(m).small().color(p.faint)).truncate());
                    }
                });
                let what = match d.disc {
                    Some(x) if x.result.is_none() => x
                        .release
                        .as_ref()
                        .map(|r| format!("{} – {}", r.artist, r.title))
                        .or(x.unknown_album.clone())
                        .unwrap_or_else(|| "New disc".into()),
                    _ => String::new(),
                };
                if !what.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(what).font(theme::medium(14.0)).color(p.text)).truncate());
                }
                ui.add(egui::Label::new(egui::RichText::new(d.stage).small().color(p.dim)).truncate());
                if let Some(x) = d.disc.filter(|_| d.ripping) {
                    if x.total_secs > 0 {
                        let f = x.done_secs as f32 / x.total_secs as f32;
                        let w = (ui.available_width() - 44.0).max(60.0);
                        theme::progress(ui, f, w, Some(&format!("{:.0}%", f * 100.0)));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if d.ripping {
                    let hint = "Stop ripping this disc and eject it. Tracks already saved stay in the library; \
                                put the disc back in later and only the missing tracks are ripped. \
                                This drive keeps watching for the next disc.";
                    let r = ui.add_enabled_ui(!d.stopping, |ui| theme::control_button(ui, theme::Icon::Stop, 38.0, theme::ERR));
                    if r.inner.on_hover_text(hint).clicked() {
                        click = Some(DeckClick::StopDisc);
                    }
                } else {
                    let r = ui.add_enabled_ui(!d.ejecting, |ui| theme::control_button(ui, theme::Icon::Eject, 38.0, theme::pal(ui).text));
                    if r.inner.on_hover_text(if d.ejecting { "Ejecting…" } else { "Eject this drive's disc" }).clicked() {
                        click = Some(DeckClick::Eject);
                    }
                }
            });
        });
    });
    let resp = r.response.interact(egui::Sense::click());
    if click.is_none() && resp.clicked() && !d.shown {
        click = Some(DeckClick::Select);
    }
    if resp.hovered() && !d.shown {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    click
}

/// The disc's tracks: one row per track with a status mark, title, status
/// and length; the reads underneath; a progress bar on the one being read.
fn track_list(ui: &mut egui::Ui, d: &Disc, time: f64) {
    let p = theme::pal(ui);
    ui.label(egui::RichText::new(summary_line(d)).font(theme::medium(15.0)).color(p.text));
    if !d.album_dir.is_empty() {
        ui.label(egui::RichText::new(&d.album_dir).small().color(p.faint)).on_hover_text("Where the FLAC files go");
    }
    ui.add_space(8.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for r in &d.rows {
            let reading = matches!(r.status, Status::Reading { .. });
            let frame = egui::Frame::new()
                .fill(if reading { p.card_hover } else { p.card })
                .stroke(egui::Stroke::new(if reading { 1.5 } else { 1.0 }, if reading { theme::ACCENT } else { p.line }))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(14, 10));
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                    let painter = ui.painter();
                    match &r.status {
                        Status::Pending => {
                            painter.circle_stroke(rect.center(), 6.0, egui::Stroke::new(1.5, p.faint));
                        }
                        Status::Present => theme::paint_icon(painter, rect.shrink(2.0), theme::Icon::Check, p.faint),
                        Status::Reading { .. } => theme::spinner(painter, rect.center(), 8.0, time),
                        Status::Saved(_) => {
                            painter.circle_filled(rect.center(), 10.0, theme::OK.gamma_multiply(0.18));
                            theme::paint_icon(painter, rect.shrink(5.0), theme::Icon::Check, theme::OK);
                        }
                        Status::SavedWarn(_) => {
                            painter.circle_filled(rect.center(), 10.0, theme::WARN.gamma_multiply(0.18));
                            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "!", theme::bold(14.0), theme::WARN);
                        }
                        Status::Rejected(_) => {
                            painter.circle_filled(rect.center(), 10.0, theme::ERR.gamma_multiply(0.18));
                            theme::paint_icon(painter, rect.shrink(6.0), theme::Icon::Cross, theme::ERR);
                        }
                    }
                    ui.add_sized([24.0, 22.0], egui::Label::new(egui::RichText::new(r.info.position.to_string()).monospace().color(p.faint)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(fmt_duration(Duration::from_secs(r.info.seconds as u64))).monospace().color(p.dim));
                        ui.add_space(6.0);
                        match &r.status {
                            Status::Pending => {
                                ui.label(egui::RichText::new("waiting").color(p.faint));
                            }
                            Status::Present => {
                                ui.label(egui::RichText::new("in library").color(p.faint));
                            }
                            Status::Reading { read, fraction } => {
                                ui.label(egui::RichText::new(format!("read {read} · {:.0}%", fraction * 100.0)).font(theme::medium(13.5)).color(theme::prism_at(0.35)));
                            }
                            Status::Saved(t) => {
                                theme::badge(ui, t.trim_start_matches("✔ "), theme::OK);
                            }
                            Status::SavedWarn(t) => {
                                theme::badge(ui, "saved · check", theme::WARN).on_hover_text(format!(
                                    "{}\n\nAccurateRip knows this pressing, but the track doesn't match it: probably a damaged spot that reads \
                                     wrong the same way every time. The audio is kept; clean the disc and re-rip this track.",
                                    t.trim_start_matches("⚠ ")
                                ));
                            }
                            Status::Rejected(t) => {
                                theme::badge(ui, t.trim_start_matches("✘ "), theme::ERR);
                            }
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let title = egui::RichText::new(&r.info.title).font(if reading { theme::medium(15.0) } else { egui::FontId::proportional(15.0) }).color(p.text);
                            ui.add(egui::Label::new(title).truncate());
                        });
                    });
                });
                if let Status::Reading { fraction, .. } = &r.status {
                    ui.horizontal(|ui| {
                        ui.add_space(58.0);
                        let w = ui.available_width();
                        theme::progress(ui, *fraction, w, None);
                    });
                }
                for line in &r.reads {
                    ui.horizontal(|ui| {
                        ui.add_space(58.0);
                        ui.label(egui::RichText::new(line).small().color(p.faint));
                    });
                }
            });
            ui.add_space(4.0);
        }
    });
}

/// A made-up cover for the demo: soft diagonal colour fields.
fn demo_cover(ctx: &egui::Context) -> egui::TextureHandle {
    let n = 256;
    let mut px = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 / n as f32, y as f32 / n as f32);
            let band = ((fx * 3.0 + fy * 5.0).sin() * 0.5 + 0.5) * 0.6;
            let c = theme::lerp(theme::prism_at(fx * 0.8), egui::Color32::from_rgb(12, 14, 30), 0.35 + band * 0.5 + fy * 0.2);
            px.push(c);
        }
    }
    let img = egui::ColorImage { size: [n, n], pixels: px, source_size: egui::vec2(n as f32, n as f32) };
    ctx.load_texture("demo-cover", img, Default::default())
}

/// A made-up disc in every state, for LOCALBEAT_DEMO=1.
fn demo_disc() -> Disc {
    let titles = [
        ("Airbag", 284), ("Paranoid Android", 383), ("Subterranean Homesick Alien", 267), ("Exit Music (For a Film)", 264),
        ("Let Down", 299), ("Karma Police", 264), ("Fitter Happier", 117), ("Electioneering", 230),
        ("Climbing Up the Walls", 285), ("No Surprises", 229), ("Lucky", 259), ("The Tourist", 324),
    ];
    let rows = titles
        .iter()
        .enumerate()
        .map(|(i, (t, secs))| {
            let position = i + 1;
            let (status, reads) = match position {
                1 => (Status::Present, vec![]),
                2 | 3 => (Status::Saved("✔ AccurateRip (200)".into()), vec!["read 1: 1A2B3C4D AccurateRip ✔ (200)".to_string()]),
                4 => (
                    Status::SavedWarn("⚠ identical reads (2), differs from AccurateRip: clean & re-rip".into()),
                    vec!["read 1: 5E6F7A8B no AR match".to_string(), "read 2: 5E6F7A8B no AR match, same as before".to_string()],
                ),
                5 => (Status::Reading { read: 2, fraction: 0.42 }, vec!["read 1: 9C0D1E2F no AR match".to_string()]),
                6 => (Status::Rejected("✘ rejected (4 reads)".into()), vec!["read 1: 11111111 no AR match, 3 unreadable sectors".to_string()]),
                _ => (Status::Pending, vec![]),
            };
            Row { info: TrackInfo { position, title: t.to_string(), artist: "Radiohead".into(), seconds: *secs, exists: position == 1 }, status, reads }
        })
        .collect();
    Disc {
        drive: "/dev/sr1".into(),
        model: "hp DVDRAM GTA0N".into(),
        offset: 102,
        release: Some(Release {
            title: "OK Computer".into(),
            artist: "Radiohead".into(),
            date: "1997-07-01".into(),
            country: "US".into(),
            labels: vec!["Capitol Records".into()],
            media_count: 1,
            ..Default::default()
        }),
        in_accuraterip: Some(true),
        album_dir: "~/Music/FLAC/Radiohead/OK Computer".into(),
        rows,
        total_secs: 3405 * 75,
        done_secs: 1500 * 75,
        ..Default::default()
    }
}
