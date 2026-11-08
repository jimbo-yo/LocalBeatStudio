//! ripshelf's window: runs the rip loop in the background (disc after disc)
//! and shows what it's doing.

// No console window behind the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use ripshelf_core::accuraterip::Verdict;
use ripshelf_core::config::Settings;
use ripshelf_core::flac_encode::Picture;
use ripshelf_core::musicbrainz::Release;
use ripshelf_core::rip::Acceptance;
use ripshelf_engine::{self as engine, Event, Frontend, Options, ReleaseChoice, TrackInfo};

/// Seconds before the best exact release is used if nobody answers.
const AUTO_CHOOSE_SECS: u64 = 30;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 680.0]).with_title("ripshelf"),
        ..Default::default()
    };
    eframe::run_native("ripshelf", options, Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))))
}

/// Messages from the engine thread.
enum Msg {
    Event(Event),
    Choose { candidates: Vec<Release>, reply: Sender<ReleaseChoice> },
    Stopped(Option<String>),
}

struct GuiFrontend {
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Frontend for GuiFrontend {
    fn event(&self, e: Event) {
        let _ = self.tx.send(Msg::Event(e));
        self.ctx.request_repaint();
    }

    fn choose_release(&self, candidates: &[Release]) -> ReleaseChoice {
        let (reply, answer) = channel();
        let _ = self.tx.send(Msg::Choose { candidates: candidates.to_vec(), reply });
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

struct Choice {
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
    disc: Option<Disc>,
    choice: Option<Choice>,
    session: Vec<(bool, String)>,
    log: Vec<String>,
    show_log: bool,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let mut app = App {
            ctx,
            settings: Settings::load(),
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            running: false,
            stage: String::new(),
            disc: None,
            choice: None,
            session: Vec::new(),
            log: Vec::new(),
            show_log: false,
        };
        app.start();
        app
    }

    fn start(&mut self) {
        let (tx, rx) = channel();
        self.stop = Arc::new(AtomicBool::new(false));
        let fe = GuiFrontend { tx: tx.clone(), stop: self.stop.clone(), ctx: self.ctx.clone() };
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let r = engine::rip_loop(&settings, &Options::default(), &fe);
            let _ = tx.send(Msg::Stopped(r.err().map(|e| format!("{e:#}"))));
            fe.ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.running = true;
        self.stage = "Starting".into();
        self.log("Started: waiting for discs".into());
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.stage = "Stopping...".into();
        if let Some(c) = self.choice.take() {
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
        let Some(rx) = &self.rx else { return };
        let msgs: Vec<Msg> = rx.try_iter().collect();
        for m in msgs {
            match m {
                Msg::Event(e) => self.on_event(e),
                Msg::Choose { candidates, reply } => {
                    let auto = candidates.first().is_some_and(|r| !r.fuzzy);
                    self.choice = Some(Choice {
                        candidates,
                        reply,
                        selected: 0,
                        deadline: auto.then(|| Instant::now() + Duration::from_secs(AUTO_CHOOSE_SECS)),
                    });
                }
                Msg::Stopped(err) => {
                    self.running = false;
                    self.rx = None;
                    match err {
                        Some(e) => {
                            self.stage = format!("Stopped: {e}");
                            self.log(format!("Stopped with an error: {e}"));
                        }
                        None => {
                            self.stage = "Stopped".into();
                            self.log("Stopped".into());
                        }
                    }
                    break;
                }
            }
        }
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
                    Acceptance::Repeat { reads } => format!("✔ identical reads ({reads})"),
                };
                self.finish_track(position, Status::Saved(text));
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
        let ctx = &ui.ctx().clone();
        self.poll();
        if self.running || self.choice.is_some() {
            // Keep the countdown and progress moving.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.running {
                    if ui.button("⏹ Stop").clicked() {
                        self.stop();
                    }
                } else if ui.button("▶ Start").clicked() {
                    self.start();
                }
                ui.separator();
                ui.label(&self.stage);
                if let Some((frac, eta)) = self.overall() {
                    ui.separator();
                    let text = match eta {
                        Some(e) => format!("{:.0}% · {} left", frac * 100.0, fmt_duration(e)),
                        None => format!("{:.0}%", frac * 100.0),
                    };
                    ui.add(egui::ProgressBar::new(frac).text(text).desired_width(260.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(&mut self.show_log, "Log");
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
            let Some(d) = &self.disc else { return };
            if !d.album_dir.is_empty() {
                ui.small(&d.album_dir);
                ui.add_space(4.0);
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("tracks").striped(true).num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
                    for r in &d.rows {
                        ui.label(format!("{:>2}", r.info.position));
                        ui.vertical(|ui| {
                            ui.label(&r.info.title);
                            for line in &r.reads {
                                ui.small(line);
                            }
                        });
                        ui.label(fmt_duration(Duration::from_secs(r.info.seconds as u64)));
                        match &r.status {
                            Status::Pending => {
                                ui.weak("waiting");
                            }
                            Status::Present => {
                                ui.weak("already in library");
                            }
                            Status::Reading { read, fraction } => {
                                ui.add(
                                    egui::ProgressBar::new(*fraction)
                                        .desired_width(180.0)
                                        .text(format!("read {read} · {:.0}%", fraction * 100.0)),
                                );
                            }
                            Status::Saved(t) => {
                                ui.colored_label(egui::Color32::from_rgb(60, 160, 80), t);
                            }
                            Status::Rejected(t) => {
                                ui.colored_label(ui.visuals().error_fg_color, t);
                            }
                        }
                        ui.end_row();
                    }
                });
            });
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
        egui::Window::new("Which release is this?")
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
