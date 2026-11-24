//! First-run Welcome screen and invite-claim flow (docs/onboarding.md).
//!
//! Shown on a fresh install (no `localbeat.json` yet) so non-technical users
//! see one simple action ("Paste your invite link") plus a quiet "Use without
//! a server" option, instead of the full Settings page. Also reused as a
//! modal when an already-joined device clicks "Join with a new invite…" (with
//! an explicit confirmation guard before replacing the existing setup).

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, RichText};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{self, Invite, Role};
use localbeat_engine::join::{self as engine_join, ClaimOutcome};

use crate::theme;

pub enum WelcomeEvent {
    /// Invite claimed and saved to disk; `prefs` is updated and ready for live
    /// library reload (no restart needed).
    Claimed(ClaimOutcome),
    /// User chose "Use without a server" (local-only mode saved).
    LocalOnly,
    /// Modal dismissed without changes.
    Cancelled,
}

enum Msg {
    Claimed(Result<(ClaimOutcome, Prefs), String>),
}

pub struct WelcomeUi {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    input: String,
    busy: bool,
    error: Option<String>,
    /// Set when the user is already joined and needs to confirm replacing.
    confirm_replace: Option<String>,
    /// True when shown as a modal over an existing session rather than the
    /// full-window first-run screen.
    pub modal_open: bool,
}

impl WelcomeUi {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        WelcomeUi {
            tx,
            rx,
            input: String::new(),
            busy: false,
            error: None,
            confirm_replace: None,
            modal_open: false,
        }
    }

    /// Pre-fills the fullscreen Welcome screen (e.g. launched via `localbeat://join/...`).
    pub fn prefill(&mut self, uri: String) {
        self.input = uri;
        self.error = None;
    }

    /// Opens the invite-claim modal (optionally pre-filled with a `localbeat://join/...` URI).
    pub fn open_modal(&mut self, prefill: Option<String>, prefs: &Prefs) {
        self.modal_open = true;
        self.error = None;
        self.confirm_replace = None;
        if let Some(uri) = prefill {
            self.input = uri;
            let already_joined = prefs.role.is_some() && !prefs.server.login.is_empty();
            if already_joined && join::extract_invite_uri(&self.input).is_some() {
                self.confirm_replace = Some(self.input.clone());
            }
        }
    }

    fn start_claim(&mut self, s: &Settings, prefs: &Prefs, ctx: &egui::Context, text: String) {
        self.busy = true;
        self.error = None;
        self.confirm_replace = None;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let (s, mut prefs) = (s.clone(), prefs.clone());
        std::thread::spawn(move || {
            let res = engine_join::claim(&s, &text, &mut prefs)
                .map(|out| (out, prefs))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Claimed(res));
            ctx.request_repaint();
        });
    }

    /// Polls background claim completion.
    pub fn poll(&mut self, prefs: &mut Prefs) -> Option<WelcomeEvent> {
        let mut event = None;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Claimed(Ok((outcome, new_prefs))) => {
                    self.busy = false;
                    self.input.clear();
                    self.error = None;
                    self.confirm_replace = None;
                    self.modal_open = false;
                    *prefs = new_prefs;
                    event = Some(WelcomeEvent::Claimed(outcome));
                }
                Msg::Claimed(Err(e)) => {
                    self.busy = false;
                    self.error = Some(e);
                }
            }
        }
        event
    }

    /// Full-window first-run Welcome screen.
    pub fn show_fullscreen(&mut self, ui: &mut egui::Ui, s: &Settings, prefs: &mut Prefs) -> Option<WelcomeEvent> {
        let p = theme::pal(ui);
        let mut out = None;

        // Allow pressing Ctrl+V anywhere on the Welcome screen to paste an invite.
        self.capture_global_paste(ui.ctx());

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.content))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let avail_h = ui.available_height();
                    ui.add_space((avail_h * 0.14).max(28.0));

                    ui.label(RichText::new("Welcome to LocalBeat Studio").font(theme::bold(28.0)).color(p.text));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Paste your invite link to connect to your music server.")
                            .font(theme::medium(14.0))
                            .color(p.dim),
                    );
                    ui.add_space(24.0);

                    egui::Frame::new()
                        .fill(p.card)
                        .stroke(egui::Stroke::new(1.0, p.line))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::same(24))
                        .show(ui, |ui| {
                            ui.set_width(520.0);
                            if let Some(ev) = self.claim_form(ui, s, prefs, false) {
                                out = Some(ev);
                            }
                        });

                    ui.add_space(18.0);
                    if !self.busy {
                        let local = ui
                            .add(
                                egui::Label::new(
                                    RichText::new("Use without a server (play local music only)")
                                        .small()
                                        .color(p.faint),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Skip server setup for now; you can always join with an invite later in Settings.");
                        if local.clicked() {
                            prefs.role = None;
                            let _ = prefs.save(s);
                            out = Some(WelcomeEvent::LocalOnly);
                        }
                    }
                });
            });

        out
    }

    /// Modal dialog for re-joining or claiming an invite while already running.
    pub fn show_modal(&mut self, ctx: &egui::Context, s: &Settings, prefs: &mut Prefs) -> Option<WelcomeEvent> {
        if !self.modal_open {
            return None;
        }
        self.capture_global_paste(ctx);
        let mut out = None;
        let mut open = self.modal_open;
        egui::Window::new("Join with an invite")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([520.0, 0.0])
            .show(ctx, |ui| {
                if let Some(ev) = self.claim_form(ui, s, prefs, true) {
                    out = Some(ev);
                }
            });
        if !open && !self.busy {
            self.modal_open = false;
            self.confirm_replace = None;
            self.error = None;
            return Some(WelcomeEvent::Cancelled);
        }
        out
    }

    fn capture_global_paste(&mut self, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        let pasted: Option<String> = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(s) if join::extract_invite_uri(s).is_some() => Some(s.clone()),
                _ => None,
            })
        });
        if let Some(text) = pasted {
            self.input = text;
            self.error = None;
        }
    }

    fn claim_form(
        &mut self,
        ui: &mut egui::Ui,
        s: &Settings,
        prefs: &Prefs,
        is_modal: bool,
    ) -> Option<WelcomeEvent> {
        let p = theme::pal(ui);
        let ctx = ui.ctx().clone();

        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(6.0);
                ui.label(RichText::new("Connecting to your music server…").color(p.text));
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("Setting up your built-in WireGuard tunnel and loading your library.")
                    .small()
                    .color(p.dim),
            );
            return None;
        }

        // Already-joined confirmation guard.
        if let Some(pending_text) = self.confirm_replace.clone() {
            ui.label(
                RichText::new(format!(
                    "This computer is already connected as {}. Replace it with this new invite?",
                    prefs.server.login
                ))
                .color(p.text),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::button(ui, Some(theme::Icon::Server), "Replace connection", theme::Kind::Primary).clicked() {
                    self.start_claim(s, prefs, &ctx, pending_text);
                }
                if theme::button(ui, None, "Cancel", theme::Kind::Secondary).clicked() {
                    self.confirm_replace = None;
                }
            });
            return None;
        }

        ui.label(RichText::new("Invite link").color(p.dim));
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Paste your localbeat://join/... link (or the whole text message)"),
        );
        ui.add_space(6.0);

        let parsed = if self.input.trim().is_empty() {
            None
        } else {
            Some(Invite::parse(&self.input))
        };

        match &parsed {
            Some(Ok(inv)) => {
                let role_label = match inv.role {
                    Role::Listener => "Listener invite ready",
                    Role::Admin => "Admin invite ready (can rip CDs and send them to the server)",
                };
                ui.colored_label(theme::OK, format!("✓ {role_label}"));
            }
            Some(Err(e)) => {
                ui.colored_label(theme::WARN, format!("{e:#}"));
            }
            None => {
                ui.label(
                    RichText::new("Ask your server admin for an invite link — you can paste the entire message and Studio will find the link automatically.")
                        .small()
                        .color(p.dim),
                );
            }
        }

        if let Some(err) = &self.error {
            ui.add_space(4.0);
            ui.colored_label(theme::ERR, err);
        }

        ui.add_space(12.0);
        let can_connect = matches!(parsed, Some(Ok(_)));
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(can_connect, |ui| {
                    theme::button(ui, Some(theme::Icon::Server), "Connect", theme::Kind::Primary)
                })
                .inner
                .clicked()
            {
                let already_joined = prefs.role.is_some() && !prefs.server.login.is_empty();
                if already_joined {
                    self.confirm_replace = Some(self.input.clone());
                } else {
                    let text = self.input.clone();
                    self.start_claim(s, prefs, &ctx, text);
                }
            }
            if is_modal && theme::button(ui, None, "Cancel", theme::Kind::Secondary).clicked() {
                self.modal_open = false;
                return;
            }
        });
        None
    }
}
