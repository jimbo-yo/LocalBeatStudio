//! The Server page: albums waiting for the LocalBeat server (the outbox),
//! what was sent, and the connection settings (docs/server-library.md).
//! The sending itself is `server_push` (after each disc, every 5 minutes,
//! on Send now); this page shows it and edits `Prefs::server`.

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use localbeat_core::config::{Prefs, ServerPrefs, Settings};
use localbeat_core::outbox::{self, Entry, Outbox, State};
use localbeat_core::subsonic::Subsonic;
use localbeat_engine::push;

use crate::server_push::{Pusher, Status};
use crate::theme;

type KeyScan = Result<(String, String), String>;

pub struct ServerTab {
    /// The settings as being edited (saved into `Prefs::server`).
    edit: Option<ServerPrefs>,
    outbox: Outbox,
    loaded: Option<Instant>,
    key_rx: Option<Receiver<KeyScan>>,
    scanned_key: Option<KeyScan>,
    login_rx: Option<Receiver<Result<(), String>>>,
    login: Option<Result<(), String>>,
    message: Option<String>,
}

impl ServerTab {
    pub fn new(s: &Settings) -> Self {
        ServerTab {
            edit: None,
            outbox: Outbox::load(s),
            loaded: Some(Instant::now()),
            key_rx: None,
            scanned_key: None,
            login_rx: None,
            login: None,
            message: None,
        }
    }

    /// Re-reads the outbox now and then (the sender and rips change it).
    pub fn poll(&mut self, s: &Settings) {
        if self.loaded.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.outbox = Outbox::load(s);
            self.loaded = Some(Instant::now());
        }
        if let Some(rx) = &self.key_rx
            && let Ok(r) = rx.try_recv()
        {
            self.scanned_key = Some(r);
            self.key_rx = None;
        }
        if let Some(rx) = &self.login_rx
            && let Ok(r) = rx.try_recv()
        {
            self.login = Some(r);
            self.login_rx = None;
        }
    }

    /// One line for an album folder on the Rip page, and its colour.
    pub fn album_line(&self, s: &Settings, st: &Status, album_dir: &str) -> Option<(String, Color32)> {
        if !st.enabled || album_dir.is_empty() {
            return None;
        }
        let name = outbox::album_name(s, std::path::Path::new(album_dir)).ok()?;
        let e = self.outbox.albums.iter().find(|e| e.album == name)?;
        Some(match &e.state {
            State::Sent { at } => (format!("On the server (sent {at})"), theme::OK),
            _ if st.current.as_deref() == Some(name.as_str()) => ("Sending to the server…".into(), theme::ACCENT),
            _ if st.away => ("Waiting for the home network, then it goes to the server".into(), theme::DARK.faint),
            State::Failed { error, .. } => (format!("Not on the server yet: {error}"), theme::WARN),
            State::Waiting => ("Waiting to go to the server".into(), theme::DARK.faint),
        })
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs, pusher: &Pusher) {
        let p = theme::pal(ui);
        let st = pusher.status();
        let pending: Vec<Entry> = self.outbox.albums.iter().filter(|e| e.pending()).cloned().collect();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // Where things stand, and Send now.
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (text, color) = if !st.enabled {
                        ("Not set up: rips stay on this computer (settings below)".to_string(), p.faint)
                    } else if st.sending {
                        (if st.line.is_empty() { "Sending…".to_string() } else { format!("{}…", st.line) }, theme::ACCENT)
                    } else if pending.is_empty() {
                        ("Everything ripped here is on the server".to_string(), theme::OK)
                    } else if st.away {
                        (format!("{} album(s) waiting for the home network", pending.len()), p.dim)
                    } else {
                        (format!("{} album(s) waiting", pending.len()), theme::WARN)
                    };
                    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, color);
                    ui.label(RichText::new(text).font(theme::medium(15.0)).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can = st.enabled && !st.sending && !pending.is_empty();
                        if ui
                            .add_enabled_ui(can, |ui| theme::button(ui, Some(theme::Icon::Refresh), "Send now", theme::Kind::Primary))
                            .inner
                            .on_hover_text("Send the waiting albums now (home network only). It also happens by itself after each disc and every five minutes.")
                            .clicked()
                        {
                            pusher.kick();
                        }
                    });
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Each album is copied, compared file by file, then the server scans and Studio checks it's in the catalogue. \
                         The copy here always stays: the server's library has no backup yet.",
                    )
                    .small()
                    .color(p.faint),
                );
            });
            ui.add_space(12.0);

            theme::section_label(ui, "Waiting for the server");
            ui.add_space(4.0);
            if pending.is_empty() {
                ui.label(RichText::new("Nothing waiting.").color(p.faint));
            }
            for e in &pending {
                theme::card(ui).inner_margin(egui::Margin::symmetric(14, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&e.album).color(p.text));
                    let mut sub = format!("queued {}", e.added);
                    if let State::Failed { at, .. } = &e.state {
                        sub.push_str(&format!(" · last try {at}"));
                    }
                    ui.label(RichText::new(sub).small().color(p.faint));
                    if let State::Failed { error, .. } = &e.state {
                        ui.add(egui::Label::new(RichText::new(error).small().color(theme::WARN)).wrap());
                    }
                });
                ui.add_space(4.0);
            }

            let mut sent: Vec<(&str, &str)> = self
                .outbox
                .albums
                .iter()
                .filter_map(|e| match &e.state {
                    State::Sent { at } => Some((at.as_str(), e.album.as_str())),
                    _ => None,
                })
                .collect();
            if !sent.is_empty() {
                sent.sort_by(|a, b| b.cmp(a));
                ui.add_space(12.0);
                theme::section_label(ui, "Recently sent");
                ui.add_space(4.0);
                for (at, album) in sent.iter().take(15) {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        theme::paint_icon(ui.painter(), r, theme::Icon::Check, theme::OK);
                        ui.label(RichText::new(*album).color(p.dim));
                        ui.label(RichText::new(*at).small().color(p.faint));
                    });
                }
            }

            ui.add_space(16.0);
            self.settings_ui(ui, s, prefs, pusher);
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs, pusher: &Pusher) {
        let p = theme::pal(ui);
        let saved = prefs.server.clone();
        let edit = self.edit.get_or_insert_with(|| {
            // Not set up yet: start from the home server's usual values,
            // filled in (grey hints alone looked filled in but saved nothing).
            let mut e = saved.clone();
            for (f, v) in [
                (&mut e.host, "192.168.1.10"),
                (&mut e.user, "musicuser"),
                (&mut e.key, "~/.ssh/localbeat-studio"),
                (&mut e.url, "http://192.168.1.10:4533"),
            ] {
                if f.is_empty() && saved.host.is_empty() {
                    *f = v.to_string();
                }
            }
            e
        });
        theme::section_label(ui, "Settings");
        ui.add_space(4.0);
        let mut save = false;
        let mut scan_key = false;
        let mut try_login = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let field = |ui: &mut egui::Ui, label: &str, hint: &str, v: &mut String, secret: bool| {
                ui.label(RichText::new(label).color(p.dim));
                ui.add(egui::TextEdit::singleline(v).hint_text(hint).password(secret).desired_width(360.0));
                ui.end_row();
            };
            ui.label(RichText::new("Sending (rsync over SSH, home network only)").font(theme::medium(14.0)).color(p.text));
            egui::Grid::new("server-ssh").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                field(ui, "Server's address", "192.168.1.10", &mut edit.host, false);
                field(ui, "SSH user", "musicuser", &mut edit.user, false);
                field(ui, "This computer's key", "~/.ssh/localbeat-studio", &mut edit.key, false);
            });
            if !edit.key.is_empty() && !edit.key_path().is_file() {
                ui.label(
                    RichText::new(format!(
                        "No key there yet. Make one with: ssh-keygen -t ed25519 -f {} -N ''  and add it on the server with localbeat-writers.",
                        edit.key
                    ))
                    .small()
                    .color(theme::WARN),
                );
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Scanning (this computer's music server login)").font(theme::medium(14.0)).color(p.text));
            egui::Grid::new("server-subsonic").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                field(ui, "Music server", "http://192.168.1.10:4533", &mut edit.url, false);
                field(ui, "Login", "", &mut edit.login, false);
                field(ui, "Secret", "", &mut edit.secret, true);
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                save = ui.add_enabled_ui(*edit != saved, |ui| theme::button(ui, None, "Save", theme::Kind::Primary)).inner.clicked();
                let can_login = Subsonic::new(&saved).is_some() && self.login_rx.is_none();
                try_login = ui.add_enabled_ui(can_login, |ui| theme::button(ui, None, "Try the login", theme::Kind::Secondary)).inner.clicked();
                match &self.login {
                    Some(Ok(())) => {
                        ui.label(RichText::new("Login works").color(theme::OK));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(theme::WARN));
                    }
                    None if self.login_rx.is_some() => {
                        ui.label(RichText::new("Trying…").color(p.faint));
                    }
                    None => {}
                }
                if let Some(m) = &self.message {
                    ui.label(RichText::new(m).color(p.dim));
                }
            });

            // Pinning the server's host key.
            ui.add_space(12.0);
            ui.label(RichText::new("The server's host key").font(theme::medium(14.0)).color(p.text));
            let pinned = push::known_hosts(s).is_file();
            ui.label(
                RichText::new(if pinned {
                    "Pinned: Studio only sends to the computer with this key."
                } else {
                    "Not pinned here (a key accepted by sshing in by hand works too). At home, read it and compare it with the server's own: \
                     ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub on the server."
                })
                .small()
                .color(p.faint),
            );
            ui.horizontal(|ui| {
                let label = if pinned { "Read it again" } else { "Read the host key" };
                scan_key = ui
                    .add_enabled_ui(self.key_rx.is_none() && !saved.host.is_empty(), |ui| theme::button(ui, Some(theme::Icon::Lock), label, theme::Kind::Secondary))
                    .inner
                    .clicked();
                if self.key_rx.is_some() {
                    ui.label(RichText::new("Asking the server…").color(p.faint));
                }
            });
            let mut done = false;
            match self.scanned_key.clone() {
                Some(Ok((lines, fp))) => {
                    ui.label(RichText::new(&fp).monospace().color(p.text));
                    ui.horizontal(|ui| {
                        if theme::button(ui, Some(theme::Icon::Check), "Same as the server's: pin it", theme::Kind::Primary).clicked() {
                            self.message = Some(match push::save_host_key(s, &lines) {
                                Ok(()) => {
                                    pusher.kick();
                                    "Host key pinned.".into()
                                }
                                Err(e) => format!("Couldn't save it: {e:#}"),
                            });
                            done = true;
                        }
                        if theme::button(ui, None, "Cancel", theme::Kind::Secondary).clicked() {
                            done = true;
                        }
                    });
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(theme::WARN));
                }
                None => {}
            }
            if done {
                self.scanned_key = None;
            }
        });

        if save && let Some(edit) = &self.edit {
            let mut e = edit.clone();
            for f in [&mut e.host, &mut e.user, &mut e.key, &mut e.url, &mut e.login] {
                *f = f.trim().to_string();
            }
            prefs.server = e;
            self.message = Some(match prefs.save(s) {
                Ok(()) => {
                    pusher.kick();
                    "Saved.".into()
                }
                Err(e) => format!("Couldn't save: {e:#}"),
            });
            self.edit = None;
        }
        if try_login {
            let (tx, rx) = channel();
            let sp = prefs.server.clone();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let r = Subsonic::new(&sp).map(|c| c.ping().map_err(|e| format!("{e:#}"))).unwrap_or(Err("no login set".into()));
                let _ = tx.send(r);
                ctx.request_repaint();
            });
            self.login_rx = Some(rx);
            self.login = None;
        }
        if scan_key {
            let (tx, rx) = channel();
            let (s2, sp) = (s.clone(), prefs.server.clone());
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(push::scan_host_key(&s2, &sp).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            });
            self.key_rx = Some(rx);
            self.scanned_key = None;
        }
    }
}
