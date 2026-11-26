//! Settings: music server connection (docs/onboarding.md), music folders,
//! the phone, optional tools (adb / rsync / OpenSSH for admins), and CD drives.

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, RichText};
use localbeat_core::config::{self, Prefs, Settings};
use localbeat_core::join::Role;
use localbeat_core::subsonic::Subsonic;
use localbeat_core::tunnel::{EndpointKind, LinkState};

use crate::phone_tab::PhoneTab;
use crate::theme;

type Answer<T> = Result<T, String>;

enum Msg {
    Checks(Checks),
    Installed(&'static str, Answer<String>),
    Drives(Answer<Vec<DriveRow>>),
    Connection(Answer<()>),
}

/// Optional tools installed on this computer (checked in the background).
#[derive(Clone)]
struct Checks {
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
    status: crate::server_status::StatusWindow,
    /// Set by a button: the app starts itself again (new folders).
    pub restart_wanted: bool,
    /// Set by a button: open the "Join with an invite" modal.
    pub open_invite_modal: bool,
    /// Set by a button: open the Admin "Invite a device" QR/link window.
    pub open_admin_invites: bool,
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
        adb: localbeat_platform::phone::adb_version(),
        rsync,
        ssh_keygen,
    }
}

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

#[cfg(windows)]
fn msys_rsync() -> Answer<String> {
    let bash = std::path::Path::new(r"C:\msys64\usr\bin\bash.exe");
    if !bash.is_file() {
        winget("MSYS2.MSYS2")?;
    }
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
    pub fn new(s: &Settings, _prefs: &Prefs) -> Self {
        let (tx, rx) = channel();
        SettingsTab {
            tx,
            rx,
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
            status: Default::default(),
            restart_wanted: false,
            open_invite_modal: false,
            open_admin_invites: false,
        }
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

    pub fn poll(&mut self, _s: &Settings, _prefs: &mut Prefs, _ctx: &egui::Context) -> Vec<String> {
        let mut log = Vec::new();
        for m in self.rx.try_iter() {
            match m {
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
            if prefs.is_admin() {
                ui.add_space(12.0);
                self.needs_card(ui, prefs);
                ui.add_space(12.0);
                self.drives_card(ui, s);
            }
        });
        self.status.show(&ctx, &prefs.server);
    }

    fn server_card(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) {
        let ctx = ui.ctx().clone();
        let p = theme::pal(ui);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Music server").font(theme::medium(16.0)).color(p.text));
            ui.add_space(6.0);
            let joined = prefs.role.is_some() && !prefs.server.login.is_empty();
            if joined {
                let role = prefs
                    .role
                    .map(|r| if r == Role::Admin { "an admin: rips CDs and sends them to the server" } else { "a listener" })
                    .unwrap_or("");
                ui.label(RichText::new(format!("This computer is {} on the server, {role}.", prefs.server.login)).color(p.text));
                ui.label(RichText::new(format!("Music server: {}", prefs.server.url)).color(p.dim));
                if prefs.server.is_local_loopback() {
                    ui.colored_label(theme::OK, "Connection: this computer (local server)");
                } else if prefs.server.tunnel.is_some() {
                    match localbeat_core::tunnel::active_link_state() {
                        Some(LinkState::Connected(EndpointKind::Lan)) => {
                            ui.colored_label(theme::OK, "Built-in tunnel: connected (home network)");
                        }
                        Some(LinkState::Connected(EndpointKind::Public)) => {
                            ui.colored_label(theme::OK, "Built-in tunnel: connected (away from home)");
                        }
                        Some(LinkState::Stalled) => {
                            ui.colored_label(
                                theme::WARN,
                                "Can't reach the music server — if this device was removed, ask for a new invite.",
                            );
                        }
                        Some(LinkState::Connecting) | None => {
                            ui.label(RichText::new("Built-in tunnel: ready (connects automatically)").color(p.dim));
                        }
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if prefs.is_admin()
                        && theme::button(ui, Some(theme::Icon::Server), "Invite a device…", theme::Kind::Primary)
                            .on_hover_text("Create a QR code or link to connect a phone or another computer")
                            .clicked()
                    {
                        self.open_admin_invites = true;
                    }
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
                    if theme::button(ui, Some(theme::Icon::Server), "Server status", theme::Kind::Secondary)
                        .on_hover_text("The server computer, its disks and the space the music takes")
                        .clicked()
                    {
                        self.status.open(&ctx, &prefs.server);
                    }
                    if theme::button(ui, None, "Join with a new invite…", theme::Kind::Secondary).clicked() {
                        self.open_invite_modal = true;
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
                ui.add_space(8.0);
                let disconnect = ui
                    .add(egui::Label::new(RichText::new("Disconnect from this server…").small().color(p.faint)).sense(egui::Sense::click()))
                    .on_hover_text("Forgets this computer's server login and built-in tunnel here.");
                if disconnect.clicked() {
                    prefs.role = None;
                    prefs.server.login.clear();
                    prefs.server.secret.clear();
                    prefs.server.tunnel = None;
                    let _ = prefs.save(s);
                    localbeat_core::tunnel::reset_active_tunnel();
                }
            } else {
                ui.label("Connect to your music server with a one-time invite link from the server admin (works at home and away).");
                ui.add_space(8.0);
                if theme::button(ui, Some(theme::Icon::Server), "Join with an invite link…", theme::Kind::Primary).clicked() {
                    self.open_invite_modal = true;
                }
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
                Some(Err(_)) => ui.small("adb isn't installed: needed only for USB/Wi-Fi file sync."),
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
                ui.label(RichText::new("Optional tools (admin)").font(theme::medium(16.0)).color(p.text));
                if self.checking {
                    ui.spinner();
                } else if theme::icon_button(ui, theme::Icon::Refresh, 22.0, "Check again").clicked() {
                    self.refresh_checks(&ctx);
                }
            });
            let Some(c) = self.checks.clone() else { return };
            let linux = |pkg: &str| format!("Fedora: sudo dnf install {pkg}   Ubuntu: sudo apt install {pkg}");
            let mut rows: Vec<(&'static str, &str, bool, String)> = vec![
                ("adb", "adb: optional USB/Wi-Fi phone sync", c.adb.is_ok(), if cfg!(windows) { String::new() } else { linux("android-tools") }),
            ];
            if !prefs.server.is_local_loopback() {
                rows.push(("rsync", "rsync: sending rips to a remote server", c.rsync.is_ok(), c.rsync.clone().err().unwrap_or_default()));
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
