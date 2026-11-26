//! Admin "Invite a device" dialog (docs/onboarding.md).
//!
//! Prompts once per session for the admin password (held in memory only,
//! never written to `localbeat.json`), then lets an admin create 7-day
//! single-use invites with a rendered QR code (for phones) + copyable
//! `localbeat://join/...` link (for texting or another Studio), and revoke
//! active unclaimed invites.

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Vec2};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{AdminSession, InviteSummary, Role};
use localbeat_engine::join as engine_join;
use qrcode::{Color as QrColor, QrCode};

use crate::theme;

enum Msg {
    LoggedIn(Result<(AdminSession, Vec<String>, Vec<InviteSummary>), String>),
    Created(Result<InviteSummary, String>),
    Refreshed(Result<(Vec<String>, Vec<InviteSummary>), String>),
    Revoked(String, Result<(), String>),
}

pub struct InviteWindow {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub open: bool,
    password: String,
    session: Option<AdminSession>,
    users: Vec<String>,
    selected_user: String,
    as_admin: bool,
    invites: Vec<InviteSummary>,
    active_qr: Option<InviteSummary>,
    copied_id: Option<String>,
    busy: Option<&'static str>,
    error: Option<String>,
}

impl InviteWindow {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        InviteWindow {
            tx,
            rx,
            open: false,
            password: String::new(),
            session: None,
            users: Vec::new(),
            selected_user: String::new(),
            as_admin: false,
            invites: Vec::new(),
            active_qr: None,
            copied_id: None,
            busy: None,
            error: None,
        }
    }

    /// Opens the dialog; if already unlocked this session, refreshes invites.
    pub fn open(&mut self, s: &Settings, prefs: &Prefs, ctx: &egui::Context) {
        self.open = true;
        self.error = None;
        if let Some(sess) = &self.session {
            self.refresh(s, prefs, &sess.session_token.clone(), ctx);
        }
    }

    /// Pre-populates demo data for `LOCALBEAT_DEMO=invite`.
    pub fn open_demo(&mut self) {
        self.open = true;
        self.session = Some(AdminSession {
            session_token: "demo".into(),
            expires_at: "2026-11-24T23:59:59Z".into(),
        });
        self.users = vec!["jim".into(), "sarah".into()];
        self.selected_user = "sarah".into();
        let demo_inv = InviteSummary {
            id: "inv-demo1".into(),
            user: "sarah".into(),
            role: Role::Listener,
            created_at: "2026-11-24T13:00:00Z".into(),
            expires_at: "2026-12-01T13:00:00Z".into(),
            uri: "localbeat://join/eyJ2IjoxLCJ0IjoiZGVtby10b2tlbi0xMjM0NTY3ODkwIiwiayI6IkJQUklWIiwicCI6IkJQU0siLCJhIjoiMTAuOC4yNTAuMi8zMiIsInMiOiJTUFVCIiwiZSI6InZwbi5leGFtcGxlLmNvbTo1MTgyMCIsImwiOiIxOTIuMTY4LjEuMTA6NTE4MjAiLCJyIjoibGlzdGVuZXIifQ".into(),
        };
        self.invites = vec![demo_inv.clone()];
        self.active_qr = Some(demo_inv);
    }

    fn spawn(&self, ctx: &egui::Context, f: impl FnOnce() -> Msg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn login(&mut self, s: &Settings, prefs: &Prefs, ctx: &egui::Context) {
        let pw = std::mem::take(&mut self.password);
        if pw.is_empty() {
            self.error = Some("Enter the server's admin password.".into());
            return;
        }
        self.busy = Some("Unlocking admin invites…");
        self.error = None;
        let (s, prefs) = (s.clone(), prefs.clone());
        self.spawn(ctx, move || {
            let res = (|| -> anyhow::Result<_> {
                let sess = engine_join::admin_login(&s, &prefs, &pw)?;
                let users = engine_join::list_users(&s, &prefs, &sess.session_token).unwrap_or_default();
                let invites = engine_join::list_invites(&s, &prefs, &sess.session_token).unwrap_or_default();
                Ok((sess, users, invites))
            })()
            .map_err(|e| format!("{e:#}"));
            Msg::LoggedIn(res)
        });
    }

    fn refresh(&mut self, s: &Settings, prefs: &Prefs, token: &str, ctx: &egui::Context) {
        self.busy = Some("Loading invites…");
        self.error = None;
        let (s, prefs, token) = (s.clone(), prefs.clone(), token.to_string());
        self.spawn(ctx, move || {
            let res = (|| -> anyhow::Result<_> {
                let users = engine_join::list_users(&s, &prefs, &token)?;
                let invites = engine_join::list_invites(&s, &prefs, &token)?;
                Ok((users, invites))
            })()
            .map_err(|e| format!("{e:#}"));
            Msg::Refreshed(res)
        });
    }

    fn create(&mut self, s: &Settings, prefs: &Prefs, ctx: &egui::Context) {
        let Some(sess) = &self.session else { return };
        let user = self.selected_user.trim().to_string();
        let role = if self.as_admin { Role::Admin } else { Role::Listener };
        let token = sess.session_token.clone();
        self.busy = Some("Creating invite…");
        self.error = None;
        let (s, prefs) = (s.clone(), prefs.clone());
        self.spawn(ctx, move || {
            Msg::Created(
                engine_join::create_invite(&s, &prefs, &token, &user, role).map_err(|e| format!("{e:#}")),
            )
        });
    }

    fn revoke(&mut self, s: &Settings, prefs: &Prefs, id: String, ctx: &egui::Context) {
        let Some(sess) = &self.session else { return };
        let token = sess.session_token.clone();
        self.busy = Some("Revoking invite…");
        self.error = None;
        let (s, prefs, id_clone) = (s.clone(), prefs.clone(), id.clone());
        self.spawn(ctx, move || {
            Msg::Revoked(
                id_clone.clone(),
                engine_join::revoke_invite(&s, &prefs, &token, &id_clone).map_err(|e| format!("{e:#}")),
            )
        });
    }

    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.busy = None;
            match msg {
                Msg::LoggedIn(Ok((sess, users, invites))) => {
                    self.session = Some(sess);
                    self.users = users;
                    self.invites = invites;
                    if self.selected_user.is_empty()
                        && let Some(first) = self.users.first()
                    {
                        self.selected_user = first.clone();
                    }
                }
                Msg::Refreshed(Ok((users, invites))) => {
                    self.users = users;
                    self.invites = invites;
                }
                Msg::Created(Ok(inv)) => {
                    if !inv.user.is_empty() && !self.users.contains(&inv.user) {
                        self.users.push(inv.user.clone());
                    }
                    self.active_qr = Some(inv.clone());
                    self.invites.insert(0, inv);
                }
                Msg::Revoked(id, Ok(())) => {
                    self.invites.retain(|i| i.id != id);
                    if self.active_qr.as_ref().is_some_and(|q| q.id == id) {
                        self.active_qr = None;
                    }
                }
                Msg::LoggedIn(Err(e))
                | Msg::Created(Err(e))
                | Msg::Refreshed(Err(e))
                | Msg::Revoked(_, Err(e)) => {
                    if e.contains("session expired") {
                        self.session = None;
                    }
                    self.error = Some(e);
                }
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, s: &Settings, prefs: &Prefs) {
        self.poll();
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Invite a device")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(540.0)
            .show(ctx, |ui| {
                self.body(ui, s, prefs);
            });
        self.open = open;
    }

    fn body(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &Prefs) {
        let p = theme::pal(ui);
        let ctx = ui.ctx().clone();

        if let Some(busy) = self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(busy);
            });
            ui.add_space(8.0);
        }

        if let Some(err) = &self.error {
            ui.colored_label(theme::ERR, err);
            ui.add_space(6.0);
        }

        if self.session.is_none() {
            ui.label(
                RichText::new("Enter your server's admin password to create or revoke invites. It is kept in memory for this session only.")
                    .color(p.dim),
            );
            ui.add_space(8.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .desired_width(280.0)
                    .hint_text("Admin password"),
            );
            let submitted = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(8.0);
            if (submitted
                || ui
                    .add_enabled_ui(self.busy.is_none(), |ui| {
                        theme::button(ui, None, "Unlock", theme::Kind::Primary)
                    })
                    .inner
                    .clicked())
                && self.busy.is_none()
            {
                self.login(s, prefs, &ctx);
            }
            return;
        }

        // Create invite form.
        ui.label(RichText::new("New single-use invite (expires in 7 days)").font(theme::medium(14.0)).color(p.text));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("User").color(p.dim));
            ui.add(
                egui::TextEdit::singleline(&mut self.selected_user)
                    .desired_width(180.0)
                    .hint_text("e.g. sarah"),
            );
            if !self.users.is_empty() {
                egui::ComboBox::from_id_salt("invite_user_pick")
                    .selected_text("Existing…")
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for u in &self.users {
                            if ui.selectable_label(self.selected_user == *u, u).clicked() {
                                self.selected_user = u.clone();
                            }
                        }
                    });
            }
        });
        ui.add_space(4.0);
        ui.checkbox(&mut self.as_admin, "Admin access (can rip CDs and send them to the server)");
        ui.add_space(6.0);
        let user_ok = self.selected_user.trim().is_empty()
            || localbeat_core::join::valid_name(self.selected_user.trim());
        if !user_ok {
            ui.colored_label(theme::WARN, "User name: lowercase letters, digits and -, e.g. sarah");
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(user_ok && self.busy.is_none(), |ui| {
                    theme::button(ui, Some(theme::Icon::Server), "Create invite", theme::Kind::Primary)
                })
                .inner
                .clicked()
            {
                self.create(s, prefs, &ctx);
            }
            if let Some(sess) = &self.session {
                let tok = sess.session_token.clone();
                if ui
                    .add_enabled_ui(self.busy.is_none(), |ui| {
                        theme::button(ui, Some(theme::Icon::Refresh), "Refresh", theme::Kind::Secondary)
                    })
                    .inner
                    .clicked()
                {
                    self.refresh(s, prefs, &tok, &ctx);
                }
            }
        });

        // Active QR code preview.
        if let Some(inv) = self.active_qr.clone() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                draw_qr(ui, &inv.uri, 180.0);
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    let who = if inv.user.is_empty() { "new user" } else { inv.user.as_str() };
                    ui.label(
                        RichText::new(format!("Invite for {who} ({})", inv.role.as_str()))
                            .font(theme::medium(15.0))
                            .color(p.text),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Scan this QR code in LocalBeat on a phone, or copy the link to text or paste into another LocalBeat Studio.")
                            .small()
                            .color(p.dim),
                    );
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("Single-use · expires {}", inv.expires_at)).small().color(p.faint));
                    ui.add_space(10.0);
                    let copied = self.copied_id.as_deref() == Some(inv.id.as_str());
                    let label = if copied { "Copied!" } else { "Copy invite link" };
                    if theme::button(ui, None, label, theme::Kind::Primary).clicked() {
                        ctx.copy_text(inv.uri.clone());
                        self.copied_id = Some(inv.id.clone());
                    }
                });
            });
        }

        // Active unclaimed invites list.
        if !self.invites.is_empty() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new("Active unclaimed invites").font(theme::medium(13.0)).color(p.dim));
            ui.add_space(4.0);
            let mut to_revoke: Option<String> = None;
            egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                for inv in &self.invites {
                    ui.horizontal(|ui| {
                        let who = if inv.user.is_empty() { "(unassigned)" } else { inv.user.as_str() };
                        ui.label(RichText::new(format!("{who} · {}", inv.role.as_str())).color(p.text));
                        ui.label(RichText::new(format!("expires {}", inv.expires_at)).small().color(p.faint));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::button(ui, None, "Revoke", theme::Kind::Secondary).clicked() {
                                to_revoke = Some(inv.id.clone());
                            }
                            if theme::button(ui, None, "Show QR", theme::Kind::Secondary).clicked() {
                                self.active_qr = Some(inv.clone());
                            }
                            if theme::button(ui, None, "Copy", theme::Kind::Secondary).clicked() {
                                ctx.copy_text(inv.uri.clone());
                                self.copied_id = Some(inv.id.clone());
                            }
                        });
                    });
                }
            });
            if let Some(id) = to_revoke {
                self.revoke(s, prefs, id, &ctx);
            }
        }
    }
}

/// Paints a crisp QR code on a white quiet-zone square directly with egui.
fn draw_qr(ui: &mut egui::Ui, data: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), Color32::WHITE);
    let Ok(code) = QrCode::new(data.as_bytes()) else {
        return;
    };
    let width = code.width();
    let quiet = 3usize;
    let total = (width + quiet * 2) as f32;
    let cell = size / total;
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == QrColor::Dark {
                let min = Pos2::new(rect.left() + (x + quiet) as f32 * cell, rect.top() + (y + quiet) as f32 * cell);
                let max = Pos2::new(min.x + cell.ceil(), min.y + cell.ceil());
                painter.rect_filled(Rect::from_min_max(min, max), CornerRadius::ZERO, Color32::BLACK);
            }
        }
    }
}
