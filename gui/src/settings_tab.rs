//! Settings: joining the LocalBeat server (docs/joining.md), music folders,
//! the phone, what this computer needs installed and, for an admin, the CD
//! drives. Opens by itself on a new install (no settings file yet).
//!
//! Joining: this computer asks as a listener (plays the server's library);
//! a quiet "Admin access" link asks as an admin instead (rips CDs and sends
//! them). The request shows a code; the admin approves it on the server's
//! admin page; then the tunnel, login and (admin) the sending key are set up
//! here. Work runs on background threads; `poll` carries on while another
//! page is open.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use localbeat_core::config::{self, Prefs, Settings};
use localbeat_core::join::{self, Pending, Role, Status};
use localbeat_core::subsonic::Subsonic;
use localbeat_engine::join as joining;
use localbeat_platform::tunnel;

use crate::phone_tab::PhoneTab;
use crate::theme;

type Answer<T> = Result<T, String>;

enum Msg {
    Asked(Answer<Pending>),
    Checked(Pending, Answer<Status>),
    Finished(Answer<(Vec<String>, Prefs)>),
    Checks(Checks),
    Installed(&'static str, Answer<String>),
    Drives(Answer<Vec<DriveRow>>),
    Connection(Answer<()>),
}

/// What's installed (worked out in the background: each is a program run).
#[derive(Clone)]
struct Checks {
    tunnel: tunnel::State,
    wireguard: Answer<()>,
    adb: Answer<String>,
    rsync: Answer<String>,
    ssh_keygen: Answer<()>,
}

struct DriveRow {
    path: String,
    model: String,
    key: String,
    offset: i32,
    saved: Option<i32>,
    /// The offset known for this model (accuraterip.com's list, checked here).
    known: Option<i32>,
}

pub struct SettingsTab {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    name: String,
    server: String,
    /// Whose computer this is (a user on the server; empty: the admin decides).
    user: String,
    as_admin: bool,
    pending: Option<Pending>,
    busy: Option<&'static str>,
    next_check: Option<Instant>,
    join_error: Option<String>,
    done: Vec<String>,
    checks: Option<Checks>,
    checking: bool,
    installing: Option<&'static str>,
    install_result: Option<String>,
    flac: String,
    misc: String,
    folders_saved: bool,
    drives: Option<Answer<Vec<DriveRow>>>,
    finding_drives: bool,
    connection: Option<Answer<()>>,
    /// Set by a button: the app starts itself again (new folders, joined).
    pub restart_wanted: bool,
}

fn run_checks() -> Checks {
    let rsync = match localbeat_engine::push::Rsync::find() {
        Ok(r) if cfg!(windows) => Ok(r.exe.display().to_string()),
        Ok(r) => match r.command().arg("--version").output() {
            Ok(o) if o.status.success() => Ok("rsync".into()),
            _ => Err("rsync isn't installed".into()),
        },
        Err(e) => Err(format!("{e:#}")),
    };
    let ssh_keygen = match localbeat_platform::command("ssh-keygen").arg("-?").output() {
        Ok(_) => Ok(()),
        Err(_) => Err("OpenSSH (ssh-keygen) isn't installed".into()),
    };
    Checks {
        tunnel: tunnel::state(),
        wireguard: tunnel::available(),
        adb: localbeat_platform::phone::adb_version(),
        rsync,
        ssh_keygen,
    }
}

/// Windows: installs with winget (no console window; winget asks for
/// permission itself when a package needs it).
#[cfg(windows)]
fn winget(id: &str) -> Answer<String> {
    let out = localbeat_platform::command("winget")
        .args(["install", "--id", id, "-e", "--silent", "--accept-source-agreements", "--accept-package-agreements"])
        .output()
        .map_err(|e| format!("running winget: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() || text.contains("already installed") {
        Ok(format!("{id} installed"))
    } else {
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim().to_string();
        Err(format!("winget couldn't install {id}: {last}"))
    }
}

/// Windows: MSYS2's rsync + ssh in C:\msys64, as tools\install-windows.ps1.
#[cfg(windows)]
fn msys_rsync() -> Answer<String> {
    let bash = std::path::Path::new(r"C:\msys64\usr\bin\bash.exe");
    if !bash.is_file() {
        winget("MSYS2.MSYS2")?;
    }
    // Twice: the first update can replace pacman/the runtime and stop early.
    for cmd in ["pacman -Syu --noconfirm", "pacman -Syu --noconfirm", "pacman -S --needed --noconfirm rsync openssh"] {
        let _ = localbeat_platform::command(bash).args(["-lc", cmd]).output();
    }
    if std::path::Path::new(r"C:\msys64\usr\bin\rsync.exe").is_file() {
        Ok("rsync installed (MSYS2)".into())
    } else {
        Err("rsync didn't install in MSYS2".into())
    }
}

fn install(what: &'static str) -> Answer<String> {
    #[cfg(windows)]
    {
        match what {
            "wireguard" => winget("WireGuard.WireGuard"),
            "adb" => winget("Google.PlatformTools"),
            "rsync" => msys_rsync(),
            _ => Err(format!("nothing to install for {what}")),
        }
    }
    #[cfg(not(windows))]
    {
        Err(format!("install {what} with the package manager"))
    }
}

fn find_drives(s: &Settings) -> Answer<Vec<DriveRow>> {
    let file = s.offsets_file();
    let drives = localbeat_platform::drive::list_drives().map_err(|e| format!("{e:#}"))?;
    Ok(drives
        .into_iter()
        .filter_map(|d| {
            let id = d.identity?;
            let key = id.key();
            let saved = config::lookup_offset(&file, &key, &id.serial);
            let model = id.model();
            let known = config::known_offset(&model);
            Some(DriveRow { path: d.path, offset: saved.or(known).unwrap_or(0), model, key, saved, known })
        })
        .collect())
}

fn ok_mark(ui: &mut egui::Ui, ok: bool) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let (icon, c) = if ok { (theme::Icon::Check, theme::OK) } else { (theme::Icon::Cross, theme::WARN) };
    theme::paint_icon(ui.painter(), r, icon, c);
}

impl SettingsTab {
    pub fn new(s: &Settings, prefs: &Prefs) -> Self {
        let (tx, rx) = channel();
        let pending = Pending::load(&s.config_dir);
        SettingsTab {
            tx,
            rx,
            name: if prefs.server.login.is_empty() { join::suggested_name() } else { prefs.server.login.clone() },
            server: if prefs.server.host.is_empty() { join::DEFAULT_SERVER.into() } else { prefs.server.host.clone() },
            user: String::new(),
            as_admin: pending.as_ref().is_some_and(|p| p.role == Role::Admin),
            next_check: pending.as_ref().map(|_| Instant::now()),
            pending,
            busy: None,
            join_error: None,
            done: Vec::new(),
            checks: None,
            checking: false,
            installing: None,
            install_result: None,
            flac: s.flac_dir.display().to_string(),
            misc: s.misc_dir.display().to_string(),
            folders_saved: false,
            drives: None,
            finding_drives: false,
            connection: None,
            restart_wanted: false,
        }
    }

    /// A join request is out (for the sidebar).
    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    fn spawn(&self, ctx: &egui::Context, f: impl FnOnce() -> Msg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn refresh_checks(&mut self, ctx: &egui::Context) {
        if !self.checking {
            self.checking = true;
            self.spawn(ctx, || Msg::Checks(run_checks()));
        }
    }

    fn finish(&mut self, s: &Settings, prefs: &Prefs, ctx: &egui::Context) {
        let Some(p) = self.pending.clone() else { return };
        self.busy = Some("Setting up this computer (Windows asks for permission once, for the tunnel)…");
        self.join_error = None;
        let (s, mut prefs) = (s.clone(), prefs.clone());
        self.spawn(ctx, move || {
            Msg::Finished(joining::finish(&s, &p, &mut prefs).map(|lines| (lines, prefs)).map_err(|e| format!("{e:#}")))
        });
    }

    /// Takes in background results (every frame, whichever page is open);
    /// returns lines for the activity log.
    pub fn poll(&mut self, s: &Settings, prefs: &mut Prefs, ctx: &egui::Context) -> Vec<String> {
        let mut log = Vec::new();
        let msgs: Vec<Msg> = self.rx.try_iter().collect();
        for m in msgs {
            match m {
                Msg::Asked(Ok(p)) => {
                    log.push(format!("Asked the server to join as {} ({}), code {}", p.name, p.role.as_str(), p.code));
                    self.pending = Some(p);
                    self.busy = None;
                    self.next_check = Some(Instant::now() + Duration::from_secs(2));
                }
                Msg::Asked(Err(e)) | Msg::Finished(Err(e)) => {
                    self.busy = None;
                    self.join_error = Some(e);
                }
                Msg::Checked(p, r) => {
                    self.busy = None;
                    match r {
                        Ok(Status::Waiting) => {
                            self.pending = Some(p);
                            self.next_check = Some(Instant::now() + Duration::from_secs(3));
                        }
                        Ok(Status::Denied) => {
                            joining::cancel(s, &p);
                            self.pending = None;
                            self.join_error = Some("The admin said no.".into());
                            log.push("Joining the server: the admin said no".into());
                        }
                        Ok(Status::Approved(_)) => {
                            self.pending = Some(p);
                            log.push("Joining the server: approved".into());
                            self.finish(s, prefs, ctx);
                        }
                        Err(e) => {
                            if e.contains("no longer knows") {
                                Pending::remove(&s.config_dir);
                                self.pending = None;
                            } else {
                                self.pending = Some(p);
                                self.next_check = Some(Instant::now() + Duration::from_secs(10));
                            }
                            self.join_error = Some(e);
                        }
                    }
                }
                Msg::Finished(Ok((lines, new))) => {
                    self.busy = None;
                    self.pending = None;
                    *prefs = new;
                    log.extend(lines.iter().map(|l| format!("Joining the server: {l}")));
                    self.done = lines;
                    self.checks = None;
                }
                Msg::Checks(c) => {
                    self.checking = false;
                    self.checks = Some(c);
                }
                Msg::Installed(what, r) => {
                    self.installing = None;
                    let line = match r {
                        Ok(l) => l,
                        Err(e) => e,
                    };
                    log.push(format!("Settings: {what}: {line}"));
                    self.install_result = Some(line);
                    self.checks = None;
                }
                Msg::Drives(r) => {
                    self.finding_drives = false;
                    self.drives = Some(r);
                }
                Msg::Connection(r) => self.connection = Some(r),
            }
        }
        // Waiting for the admin: ask now and then.
        if let (Some(p), None, Some(t)) = (&self.pending, self.busy, self.next_check)
            && p.approval.is_none()
        {
            if Instant::now() >= t {
                self.next_check = None;
                let (s, mut p) = (s.clone(), p.clone());
                self.spawn(ctx, move || {
                    let r = joining::check(&s, &mut p).map_err(|e| format!("{e:#}"));
                    Msg::Checked(p, r)
                });
            } else {
                ctx.request_repaint_after(t - Instant::now());
            }
        }
        log
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs, phone: &mut PhoneTab) {
        let ctx = ui.ctx().clone();
        if self.checks.is_none() {
            self.refresh_checks(&ctx);
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            self.server_card(ui, s, prefs);
            ui.add_space(12.0);
            self.folders_card(ui, s, prefs);
            ui.add_space(12.0);
            self.phone_card(ui, phone);
            ui.add_space(12.0);
            self.needs_card(ui, prefs);
            if prefs.is_admin() {
                ui.add_space(12.0);
                self.drives_card(ui, s);
            }
        });
    }

    fn server_card(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) {
        let ctx = ui.ctx().clone();
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("The LocalBeat server").font(theme::medium(16.0)).color(p.text));
            ui.add_space(6.0);
            let joined = prefs.role.is_some() && !prefs.server.login.is_empty();
            if let Some(busy) = self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(busy);
                });
            } else if let Some(pend) = self.pending.clone() {
                if pend.approval.is_none() {
                    ui.label(format!("Asked to join as {} ({}). On the server's admin page, approve it if the code matches:", pend.name, pend.role.as_str()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&pend.code).font(theme::bold(34.0)).color(theme::BRAND_GOLD));
                        ui.add_space(12.0);
                        ui.spinner();
                        ui.label(RichText::new("waiting for the admin…").color(p.dim));
                    });
                    ui.hyperlink_to(format!("Open the admin page ({})", pend.admin_url), &pend.admin_url);
                    ui.small("The browser warns about the page's certificate once: it's the server's own.");
                } else {
                    ui.label("Approved, but setting this computer up didn't finish.");
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if pend.approval.is_some() && theme::button(ui, Some(theme::Icon::Refresh), "Try again", theme::Kind::Primary).clicked() {
                        self.finish(s, prefs, &ctx);
                    }
                    if theme::button(ui, None, "Withdraw", theme::Kind::Secondary).clicked() {
                        joining::cancel(s, &pend);
                        self.pending = None;
                        self.join_error = None;
                    }
                });
            } else if joined {
                let role = prefs.role.map(|r| if r == Role::Admin { "an admin: rips CDs and sends them to the server" } else { "a listener" }).unwrap_or("");
                ui.label(RichText::new(format!("This computer is {} on the server, {role}.", prefs.server.login)).color(p.text));
                ui.label(RichText::new(format!("Music server: {}", prefs.server.url)).color(p.dim));
                let tunnel = match self.checks.as_ref().map(|c| c.tunnel) {
                    Some(tunnel::State::Up) => "Tunnel: running (for playing away from home; Check the connection to be sure)",
                    Some(tunnel::State::Down) => "Tunnel: installed, not connected right now",
                    Some(tunnel::State::Missing) => "Tunnel: not on this computer (the library plays at home only)",
                    None => "Tunnel: …",
                };
                ui.label(RichText::new(tunnel).color(p.dim));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Some(theme::Icon::Refresh), "Check the connection", theme::Kind::Secondary).clicked() {
                        self.connection = None;
                        let sp = prefs.server.clone();
                        self.spawn(&ctx, move || {
                            Msg::Connection(match Subsonic::new(&sp) {
                                Some(c) => c.ping().map_err(|e| format!("{e:#}")),
                                None => Err("no login set".into()),
                            })
                        });
                    }
                    match &self.connection {
                        Some(Ok(())) => {
                            ui.colored_label(theme::OK, "The music server answers.");
                        }
                        Some(Err(e)) => {
                            ui.colored_label(theme::WARN, e);
                        }
                        None => {}
                    }
                });
                if !self.done.is_empty() {
                    ui.add_space(6.0);
                    for l in &self.done {
                        ui.label(RichText::new(format!("✓ {l}")).small().color(p.dim));
                    }
                    if theme::button(ui, None, "Restart Studio to load the server's library", theme::Kind::Primary).clicked() {
                        self.restart_wanted = true;
                    }
                }
                ui.add_space(8.0);
                let again = ui
                    .add(egui::Label::new(RichText::new("Set this computer up again…").small().color(p.faint)).sense(egui::Sense::click()))
                    .on_hover_text("Forgets this computer's login here. Ask the admin to remove its old name first, or pick a new one.");
                if again.clicked() {
                    prefs.role = None;
                    prefs.server.login.clear();
                    prefs.server.secret.clear();
                    let _ = prefs.save(s);
                    self.done.clear();
                }
            } else {
                ui.label("Ask the server to let this computer play its library. Joining works on the home network; the admin approves it on the server's admin page.");
                ui.add_space(6.0);
                egui::Grid::new("join").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label(RichText::new("This computer's name").color(p.dim));
                    ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(260.0));
                    ui.end_row();
                    ui.label(RichText::new("Server").color(p.dim));
                    ui.add(egui::TextEdit::singleline(&mut self.server).desired_width(260.0));
                    ui.end_row();
                    ui.label(RichText::new("Whose computer").color(p.dim));
                    ui.add(egui::TextEdit::singleline(&mut self.user).hint_text("your user, e.g. jim (optional)").desired_width(260.0));
                    ui.end_row();
                });
                if !join::valid_name(&self.name) {
                    ui.small("Name: lowercase letters, digits and -, e.g. sam-laptop.");
                }
                let user = self.user.trim().to_string();
                let user_ok = user.is_empty() || join::valid_name(&user);
                if !user_ok {
                    ui.small("Whose computer: a user name, lowercase letters, digits and -, e.g. jim.");
                } else {
                    ui.small("All of a user's devices share their playlists. The admin can change it when approving.");
                }
                ui.add_space(6.0);
                let label = if self.as_admin { "Ask to join as an admin" } else { "Ask to join" };
                let can = join::valid_name(&self.name) && user_ok && !self.server.trim().is_empty();
                if ui.add_enabled_ui(can, |ui| theme::button(ui, Some(theme::Icon::Server), label, theme::Kind::Primary)).inner.clicked() {
                    self.join_error = None;
                    self.busy = Some("Asking the server…");
                    let (s, server, name) = (s.clone(), self.server.trim().to_string(), self.name.clone());
                    let role = if self.as_admin { Role::Admin } else { Role::Listener };
                    self.spawn(&ctx, move || Msg::Asked(joining::ask(&s, &server, &name, &user, role).map_err(|e| format!("{e:#}"))));
                }
                if self.as_admin {
                    ui.small("An admin rips CDs on this computer and sends them to the server: the admin page shows that it asks for this.");
                }
                // Admin access: deliberately quiet.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let text = if self.as_admin { "Join as a listener instead" } else { "Admin access" };
                    if ui.add(egui::Label::new(RichText::new(text).small().color(p.faint)).sense(egui::Sense::click())).clicked() {
                        self.as_admin = !self.as_admin;
                    }
                });
            }
            if let Some(e) = &self.join_error {
                ui.colored_label(theme::ERR, e);
            }
        });
    }

    fn folders_card(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) {
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Music on this computer").font(theme::medium(16.0)).color(p.text));
            ui.small("Albums here play alongside the server's. None yet is fine: the server's library plays.");
            ui.add_space(6.0);
            for (label, v) in [("FLAC library", &mut self.flac), ("Miscellaneous", &mut self.misc)] {
                ui.horizontal(|ui| {
                    ui.add_sized([130.0, 24.0], egui::Label::new(RichText::new(label).color(p.dim)));
                    ui.add(egui::TextEdit::singleline(v).desired_width(460.0));
                    let exists = std::path::Path::new(v.trim()).is_dir();
                    ui.label(RichText::new(if exists { "found" } else { "not there yet" }).small().color(if exists { theme::OK } else { p.faint }));
                });
            }
            let changed = self.flac.trim() != s.flac_dir.display().to_string() || self.misc.trim() != s.misc_dir.display().to_string();
            ui.horizontal(|ui| {
                if ui.add_enabled_ui(changed, |ui| theme::button(ui, None, "Save", theme::Kind::Secondary)).inner.clicked() {
                    let path = |t: &str| (!t.trim().is_empty()).then(|| std::path::PathBuf::from(t.trim()));
                    prefs.folders = config::Folders { flac: path(&self.flac), misc: path(&self.misc) };
                    self.folders_saved = prefs.save(s).is_ok();
                }
                if self.folders_saved {
                    ui.label("Saved: used from the next start.");
                    if theme::button(ui, Some(theme::Icon::Refresh), "Restart now", theme::Kind::Primary).clicked() {
                        self.restart_wanted = true;
                    }
                }
            });
        });
    }

    fn phone_card(&mut self, ui: &mut egui::Ui, phone: &mut PhoneTab) {
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Phone").font(theme::medium(16.0)).color(p.text));
            match self.checks.as_ref().map(|c| &c.adb) {
                Some(Ok(v)) => ui.small(format!("adb found ({v}): sync over USB, or Wi-Fi after pairing once.")),
                Some(Err(_)) => ui.small("adb isn't installed (see below): it's what syncs the phone."),
                None => ui.small("…"),
            };
            phone.wireless_ui(ui);
            let (status, error) = phone.status();
            ui.small(status);
            if let Some(e) = error {
                ui.colored_label(theme::ERR, e);
            }
        });
    }

    fn needs_card(&mut self, ui: &mut egui::Ui, prefs: &Prefs) {
        let ctx = ui.ctx().clone();
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("What this computer needs").font(theme::medium(16.0)).color(p.text));
                if self.checking {
                    ui.spinner();
                } else if theme::icon_button(ui, theme::Icon::Refresh, 22.0, "Check again").clicked() {
                    self.refresh_checks(&ctx);
                }
            });
            let Some(c) = self.checks.clone() else { return };
            let linux = |pkg: &str| format!("Fedora: sudo dnf install {pkg}   Ubuntu: sudo apt install {pkg}");
            let mut rows: Vec<(&'static str, &str, bool, String)> = vec![
                ("wireguard", "WireGuard: the tunnel, to play the library away from home", c.wireguard.is_ok(), c.wireguard.clone().err().unwrap_or_default()),
                ("adb", "adb: syncing the phone", c.adb.is_ok(), if cfg!(windows) { String::new() } else { linux("android-tools") }),
            ];
            if prefs.is_admin() {
                rows.push(("rsync", "rsync: sending rips to the server", c.rsync.is_ok(), c.rsync.clone().err().unwrap_or_default()));
                rows.push(("ssh", "OpenSSH: the key that sends rips", c.ssh_keygen.is_ok(), if cfg!(windows) { "Settings > System > Optional features > OpenSSH Client".into() } else { linux("openssh-clients") }));
            }
            for (what, label, ok, hint) in rows {
                ui.horizontal(|ui| {
                    ok_mark(ui, ok);
                    ui.label(label);
                    if !ok {
                        let can_install = cfg!(windows) && what != "ssh";
                        if can_install {
                            let busy = self.installing.is_some();
                            if ui.add_enabled_ui(!busy, |ui| theme::button(ui, None, "Install", theme::Kind::Secondary)).inner.clicked() {
                                self.installing = Some(what);
                                self.install_result = None;
                                self.spawn(&ctx, move || Msg::Installed(what, install(what)));
                            }
                            if self.installing == Some(what) {
                                ui.spinner();
                            }
                        } else if !hint.is_empty() {
                            ui.label(RichText::new(hint).small().color(p.faint));
                        }
                    }
                });
            }
            if let Some(r) = &self.install_result {
                ui.small(r);
            }
        });
    }

    fn drives_card(&mut self, ui: &mut egui::Ui, s: &Settings) {
        let ctx = ui.ctx().clone();
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("CD drives").font(theme::medium(16.0)).color(p.text));
                if self.finding_drives {
                    ui.spinner();
                } else if theme::button(ui, Some(theme::Icon::Disc), "Find drives", theme::Kind::Secondary).clicked() {
                    self.finding_drives = true;
                    let s = s.clone();
                    self.spawn(&ctx, move || Msg::Drives(find_drives(&s)));
                }
            });
            ui.small("Each drive's read offset (AccurateRip's convention). If every track of a good disc fails AccurateRip, suspect the offset first.");
            match &mut self.drives {
                None => {}
                Some(Err(e)) => {
                    ui.colored_label(theme::ERR, e.as_str());
                }
                Some(Ok(rows)) if rows.is_empty() => {
                    ui.label("No CD drives found.");
                }
                Some(Ok(rows)) => {
                    let file = s.offsets_file();
                    egui::Grid::new("drives").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
                        for r in rows.iter_mut() {
                            ui.label(RichText::new(&r.model).color(p.text)).on_hover_text(format!("{}\n{}", r.path, r.key));
                            let plus = if r.offset > 0 { "+" } else { "" };
                            ui.add(egui::DragValue::new(&mut r.offset).range(-3000..=3000).prefix(plus));
                            let note = match (r.saved, r.known) {
                                (None, Some(k)) => format!("not saved yet (+{k} is this model's)"),
                                (None, None) => "not saved yet".into(),
                                (Some(v), Some(k)) if v != k => format!("saved {v:+}, but this model's is {k:+}"),
                                (Some(v), _) => format!("saved {v:+}"),
                            };
                            ui.label(RichText::new(note).small().color(p.dim));
                            if ui.add_enabled_ui(r.saved != Some(r.offset), |ui| theme::button(ui, None, "Save", theme::Kind::Secondary)).inner.clicked()
                                && config::save_offset(&file, &r.key, r.offset).is_ok()
                            {
                                r.saved = Some(r.offset);
                            }
                            ui.end_row();
                        }
                    });
                }
            }
        });
    }
}
