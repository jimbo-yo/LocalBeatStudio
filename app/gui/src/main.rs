//! ripshelf's window: runs the rip loop in the background (disc after disc)
//! and shows what it's doing.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use ripshelf_core::accuraterip::Verdict;
use ripshelf_core::config::{Prefs, Settings};
use ripshelf_core::flac_encode::Picture;
use ripshelf_core::musicbrainz::Release;
use ripshelf_core::rip::Acceptance;
use ripshelf_engine::notify::Notifier;
use ripshelf_engine::{self as engine, Event, Frontend, Options, ReleaseChoice, TrackInfo};
use ripshelf_platform::phone_notify;

mod phone_tab;
mod review_tab;
mod theme;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
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
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]).with_min_inner_size([760.0, 480.0]).with_title("ripshelf"),
        ..Default::default()
    };
    eframe::run_native("ripshelf", options, Box::new(|cc| {
        theme::apply(&cc.egui_ctx);
        Ok(Box::new(App::new(cc.egui_ctx.clone())))
    }))
}

/// Messages from the engine thread.
enum Msg {
    /// An event from the rip loop of a drive (path).
    Event(String, Event),
    Choose { drive: String, candidates: Vec<Release>, reply: Sender<ReleaseChoice> },
    /// A drive's rip loop started / ended.
    LoopStarted(String),
    Stopped(String, Option<String>),
    /// The drive watcher ended (after Stop).
    WatcherDone,
}

struct GuiFrontend {
    drive: String,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
    notifier: Arc<Notifier>,
}

impl Frontend for GuiFrontend {
    fn event(&self, e: Event) {
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
                Err(_) if self.cancelled() => return ReleaseChoice::Skip,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return ReleaseChoice::Skip,
                Err(_) => {}
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
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
}

/// One drive's part of the Rip tab.
#[derive(Default)]
struct DriveView {
    disc: Option<Disc>,
    stage: String,
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
    /// Result of a running Eject (button off meanwhile).
    eject: Option<Receiver<Vec<String>>>,
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
        let mut app = App {
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
            tab: Tab::Rip,
            phone: phone_tab::PhoneTab::new(),
            review: review_tab::ReviewTab::new(),
            eject: None,
        };
        if std::env::var_os("RIPSHELF_DEMO").is_some() {
            // Made-up disc for working on the layout without a CD.
            app.drives.insert("/dev/sr1".into(), DriveView { disc: Some(demo_disc()), stage: "Ripping track 5 (demo)".into() });
            app.drives.insert("/dev/sr2".into(), DriveView { disc: None, stage: "Waiting for a disc (demo)".into() });
            app.view = Some("/dev/sr1".into());
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
                for path in ripshelf_platform::drive::drive_paths() {
                    if !started.insert(path.clone()) {
                        continue;
                    }
                    let fe = GuiFrontend { drive: path.clone(), tx: tx.clone(), stop: stop.clone(), ctx: ctx.clone(), notifier: notifier.clone() };
                    let (settings, tx) = (settings.clone(), tx.clone());
                    let _ = tx.send(Msg::LoopStarted(path.clone()));
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
                Msg::LoopStarted(drive) => {
                    self.loops += 1;
                    self.drives.entry(drive.clone()).or_default().stage = "Waiting for a disc".into();
                    if self.view.is_none() {
                        self.select_view(drive);
                    }
                }
                Msg::Choose { drive, candidates, reply } => {
                    self.queued_choices.push_back(Choice { drive, candidates, reply, selected: 0, deadline: None });
                }
                Msg::Stopped(drive, err) => {
                    self.loops = self.loops.saturating_sub(1);
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.swap_view(true);
        self.draw(ui);
        self.swap_view(false);
        if let Some(v) = self.pending_view.take() {
            self.select_view(v);
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
        if let Some(lines) = self.eject.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.eject = None;
            for l in lines {
                self.log(l);
            }
        }
        if self.running || self.choice.is_some() {
            // Keep the countdown and progress moving.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        let header = egui::Frame::new().fill(ui.visuals().extreme_bg_color).inner_margin(egui::Margin::symmetric(14, 10));
        egui::Panel::top("top").frame(header).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("ripshelf").size(20.0).strong().color(theme::ACCENT));
                ui.add_space(12.0);
                ui.selectable_value(&mut self.tab, Tab::Rip, theme::tab("💿 Rip"));
                ui.selectable_value(&mut self.tab, Tab::Phone, theme::tab("📱 Phone"));
                ui.selectable_value(&mut self.tab, Tab::Review, theme::tab("🏷 Review tags"));
                ui.separator();
                // The rip loop's controls only on the Rip tab (on the Phone tab
                // they looked like sync controls); there, just its status.
                if self.tab != Tab::Rip {
                    ui.weak(format!("Ripping: {}", if self.running { self.stage.as_str() } else { "stopped" }));
                } else {
                    if self.running {
                        if ui.button("⏹ Stop ripping").clicked() {
                            self.stop();
                        }
                    } else if ui.button("▶ Start ripping").clicked() {
                        self.start();
                    }
                    let ejecting = self.eject.is_some();
                    let hint = "Eject the CD from every drive that isn't being ripped. To eject a disc that's being ripped, stop ripping first.";
                    if ui.add_enabled(!ejecting, egui::Button::new(if ejecting { "⏏ Ejecting…" } else { "⏏ Eject" })).on_hover_text(hint).clicked() {
                        let (tx, rx) = channel();
                        let (settings, ctx) = (self.settings.clone(), ctx.clone());
                        std::thread::spawn(move || {
                            let r = engine::eject_idle(&settings);
                            let mut lines: Vec<String> = r.ejected.iter().map(|p| format!("Ejected {p}")).collect();
                            lines.extend(r.busy.iter().map(|p| format!("{p} is being ripped: stop ripping first to eject it")));
                            lines.extend(r.failed.iter().map(|(p, e)| format!("Couldn't eject {p}: {e}")));
                            if lines.is_empty() {
                                lines.push("No CD to eject".into());
                            }
                            let _ = tx.send(lines);
                            ctx.request_repaint();
                        });
                        self.eject = Some(rx);
                        self.show_log = true;
                    }
                    ui.separator();
                    ui.label(&self.stage);
                }
                if let (Tab::Rip, Some((frac, eta))) = (self.tab, self.overall()) {
                    ui.separator();
                    let text = match eta {
                        Some(e) => format!("{:.0}% · {} left", frac * 100.0, fmt_duration(e)),
                        None => format!("{:.0}%", frac * 100.0),
                    };
                    ui.add(egui::ProgressBar::new(frac).text(text).desired_width(170.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(&mut self.show_log, "Log");
                    let hint = format!(
                        "Through KDE Connect (phone on the same Wi-Fi): once the drive has waited {} s for the \
                         next disc, say how the last one went. Problems are sent straight away.",
                        self.prefs.notify_after_secs
                    );
                    if ui.checkbox(&mut self.prefs.notify_phone, "Notify phone").on_hover_text(hint).changed() {
                        self.notifier.set_enabled(self.prefs.notify_phone);
                        let state = if self.prefs.notify_phone { "on" } else { "off" };
                        match self.prefs.save(&self.settings) {
                            Ok(()) => self.log(format!("Phone notifications {state}")),
                            Err(e) => self.log(format!("Phone notifications {state} (couldn't save the setting: {e:#})")),
                        }
                    }
                });
            });
        });

        egui::Panel::bottom("bottom").resizable(true).show_collapsible(ui, &mut self.show_log, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l);
                }
            });
        });

        if self.tab == Tab::Review {
            let mut lines = Vec::new();
            egui::CentralPanel::default().show(ui, |ui| {
                lines = self.review.ui(ui, &self.settings);
            });
            for l in lines {
                self.log(l);
            }
            self.choice_window(ctx);
            return;
        }
        if self.tab == Tab::Phone {
            egui::CentralPanel::default().show(ui, |ui| {
                self.phone.ui(ui, &self.settings, &mut self.prefs);
            });
            self.choice_window(ctx);
            return;
        }

        if self.drives.len() > 1 {
            egui::Panel::top("drives").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Drives:");
                    for (path, v) in &self.drives {
                        let shown = self.view.as_deref() == Some(path.as_str());
                        let (disc, stage) = if shown { (&self.disc, &self.stage) } else { (&v.disc, &v.stage) };
                        let name = path.trim_start_matches(r"\\.\").trim_start_matches("/dev/");
                        let what = match disc {
                            Some(d) if d.result.is_none() => {
                                let album = d.release.as_ref().map(|r| r.title.clone()).or(d.unknown_album.clone()).unwrap_or_else(|| "new disc".into());
                                format!("{album} · {stage}")
                            }
                            _ => stage.clone(),
                        };
                        let model = disc.as_ref().map(|d| d.model.clone()).filter(|m| !m.is_empty());
                        let label = egui::RichText::new(format!("{name}: {what}"));
                        let r = ui.selectable_label(shown, label);
                        let r = match model {
                            Some(m) => r.on_hover_text(m),
                            None => r,
                        };
                        if r.clicked() && !shown {
                            self.pending_view = Some(path.clone());
                        }
                    }
                });
            });
        }

        egui::Panel::left("album").resizable(false).exact_size(300.0).show(ui, |ui| {
            ui.add_space(8.0);
            match &self.disc {
                None => {
                    ui.heading("No disc yet");
                    ui.label("Put a CD in a drive. Each disc is ripped, verified, filed in the library and ejected.");
                    ui.add_space(8.0);
                    ui.small(format!("Library: {}", self.settings.flac_dir.display()));
                    ui.small(format!("Phone copies: {}", self.settings.phone_dir.display()));
                }
                Some(d) => {
                    if let Some(t) = &d.cover {
                        ui.add(egui::Image::new(t).max_width(284.0).corner_radius(4.0));
                    } else {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(284.0, 284.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "♪",
                            egui::FontId::proportional(64.0),
                            ui.visuals().weak_text_color(),
                        );
                    }
                    ui.add_space(6.0);
                    match (&d.release, &d.unknown_album) {
                        (Some(r), _) => {
                            ui.heading(&r.title);
                            ui.label(egui::RichText::new(&r.artist).strong());
                            let mut bits = vec![r.date.clone(), r.country.clone()];
                            bits.extend(r.labels.first().cloned());
                            bits.retain(|b| !b.is_empty());
                            ui.small(bits.join(" · "));
                            if r.media_count > 1 {
                                ui.label(format!("Disc {} of {}", r.medium.position, r.media_count));
                            }
                            if d.alternatives > 0 {
                                ui.small(format!("({} other matching releases)", d.alternatives));
                            }
                        }
                        (None, Some(a)) => {
                            ui.heading(a);
                            ui.label("Not in MusicBrainz");
                        }
                        _ => {
                            ui.heading("Identifying…");
                        }
                    }
                    ui.add_space(6.0);
                    ui.small(format!("{} · {} · offset {:+}", d.drive.trim_start_matches(r"\\.\"), d.model, d.offset));
                    if d.in_accuraterip == Some(false) {
                        ui.small("Not in AccurateRip: verifying by repeat reads");
                    }
                    if let Some(res) = &d.result {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(res).strong());
                    }
                }
            }
            if !self.session.is_empty() {
                ui.add_space(12.0);
                ui.separator();
                ui.label(egui::RichText::new("This session").strong());
                for (ok, s) in self.session.iter().rev() {
                    ui.small(format!("{} {s}", if *ok { "✔" } else { "✘" }));
                }
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(d) = &self.disc else {
                // Nothing to show yet: say what's happening, centred.
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.3);
                    ui.label(egui::RichText::new("💿").size(64.0).color(ui.visuals().weak_text_color()));
                    ui.add_space(8.0);
                    let (title, hint) = if self.running {
                        ("Ready for a disc", "Put a CD in the drive. It's ripped, checked against AccurateRip, tagged and ejected.")
                    } else {
                        ("Ripping is stopped", "Press ▶ Start ripping to watch the drives for a disc.")
                    };
                    ui.heading(title);
                    ui.weak(hint);
                });
                return;
            };
            track_list(ui, d);
        });

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
        egui::Window::new(format!("Which release is this? ({})", c.drive))
            .id(egui::Id::unique("release-choice"))
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if c.candidates.is_empty() {
                    ui.label("MusicBrainz doesn't know this disc.");
                    ui.label("Rip it as \"Unknown Artist / Unknown Album\", or skip it (it will be ejected).");
                } else {
                    if c.candidates[0].fuzzy {
                        ui.label("MusicBrainz doesn't have this exact disc. These releases have a similar track layout; the track names may not fit.");
                    } else {
                        ui.label(format!("{} releases match this disc. The first is the best guess.", c.candidates.len()));
                    }
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, r) in c.candidates.iter().enumerate() {
                            let mut text = r.summary();
                            if r.fuzzy {
                                text.push_str("  [similar TOC]");
                            }
                            if ui.radio(c.selected == i, text).clicked() {
                                c.selected = i;
                                c.deadline = None;
                            }
                        }
                    });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if !c.candidates.is_empty() && ui.button("Use this release").clicked() {
                        answer = Some(ReleaseChoice::Use(c.selected));
                    }
                    if ui.button("Rip as unknown").clicked() {
                        answer = Some(ReleaseChoice::Unknown);
                    }
                    if ui.button("Skip disc").clicked() {
                        answer = Some(ReleaseChoice::Skip);
                    }
                });
                if let Some(dl) = c.deadline {
                    let left = dl.saturating_duration_since(Instant::now()).as_secs();
                    ui.small(format!("Using the first in {left} s unless you choose."));
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

/// The disc's tracks: one card per track with a status mark, title, the
/// result of each read, and a progress bar on the one being read.
fn track_list(ui: &mut egui::Ui, d: &Disc) {
    const GREEN: egui::Color32 = egui::Color32::from_rgb(70, 170, 95);
    const AMBER: egui::Color32 = egui::Color32::from_rgb(230, 170, 60);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(summary_line(d)).strong());
        if !d.album_dir.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(&d.album_dir).on_hover_text("Where the FLAC files go");
            });
        }
    });
    ui.add_space(6.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for r in &d.rows {
            let reading = matches!(r.status, Status::Reading { .. });
            let v = ui.visuals();
            let frame = egui::Frame::new()
                .fill(if reading { v.faint_bg_color } else { v.panel_fill })
                .stroke(if reading { egui::Stroke::new(1.5, theme::ACCENT) } else { egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color) })
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(12, 8));
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (mark, colour) = match &r.status {
                        Status::Pending => ("○", ui.visuals().weak_text_color()),
                        Status::Present => ("✓", ui.visuals().weak_text_color()),
                        Status::Reading { .. } => ("⟳", theme::ACCENT),
                        Status::Saved(_) => ("✔", GREEN),
                        Status::SavedWarn(_) => ("⚠", AMBER),
                        Status::Rejected(_) => ("✘", ui.visuals().error_fg_color),
                    };
                    if reading {
                        ui.add_sized([22.0, 22.0], egui::Spinner::new().size(16.0).color(theme::ACCENT));
                    } else {
                        ui.add_sized([22.0, 22.0], egui::Label::new(egui::RichText::new(mark).size(18.0).color(colour)));
                    }
                    ui.add_sized([26.0, 22.0], egui::Label::new(egui::RichText::new(format!("{:>2}", r.info.position)).monospace().weak()));
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let title = egui::RichText::new(&r.info.title);
                            ui.label(if reading { title.strong() } else { title });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.weak(fmt_duration(Duration::from_secs(r.info.seconds as u64)));
                                ui.add_space(10.0);
                                match &r.status {
                                    Status::Pending => {
                                        ui.weak("waiting");
                                    }
                                    Status::Present => {
                                        ui.weak("already in library");
                                    }
                                    Status::Reading { read, fraction } => {
                                        ui.colored_label(theme::ACCENT, format!("read {read} · {:.0}%", fraction * 100.0));
                                    }
                                    Status::SavedWarn(t) => {
                                        ui.colored_label(AMBER, t.trim_start_matches("⚠ "))
                                            .on_hover_text("AccurateRip knows this pressing, but the track doesn't match it: probably a damaged spot that reads wrong the same way every time. The audio is kept; clean the disc and re-rip this track.");
                                    }
                                    Status::Saved(t) => {
                                        ui.colored_label(GREEN, t.trim_start_matches("✔ "));
                                    }
                                    Status::Rejected(t) => {
                                        ui.colored_label(ui.visuals().error_fg_color, t.trim_start_matches("✘ "));
                                    }
                                }
                            });
                        });
                        if let Status::Reading { fraction, .. } = &r.status {
                            ui.add(egui::ProgressBar::new(*fraction).desired_height(6.0).fill(theme::ACCENT));
                        }
                        for line in &r.reads {
                            ui.small(egui::RichText::new(line).weak());
                        }
                    });
                });
            });
            ui.add_space(1.0);
        }
    });
}

/// A made-up disc in every state, for RIPSHELF_DEMO=1.
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
