//! Sending rips to the LocalBeat server in the background
//! (docs/server-library.md): after each disc, every few minutes, or when
//! asked, whatever is waiting in the outbox goes to the server if this
//! computer is on the home network. Results go to the activity log; the
//! sidebar shows how many are waiting.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use localbeat_core::config::{Prefs, Settings};
use localbeat_core::outbox::{Outbox, State};
use localbeat_engine::push::{self, PushEvent};

/// How often to look again while albums wait (coming home, the server back on).
const EVERY: Duration = Duration::from_secs(300);

#[derive(Clone, Default)]
pub struct Status {
    /// A server is set up (else nothing is shown).
    pub enabled: bool,
    pub waiting: usize,
    /// Albums whose last try failed (not just "away from home").
    pub failed: usize,
    /// What's happening, for the sidebar's tooltip.
    pub line: String,
    pub sending: bool,
}

pub struct Pusher {
    kick: Sender<()>,
    status: Arc<Mutex<Status>>,
}

impl Pusher {
    pub fn start(settings: Settings, ctx: egui::Context, log: Sender<String>) -> Pusher {
        let (kick, rx) = channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        std::thread::spawn(move || run(settings, ctx, log, rx, st));
        let p = Pusher { kick, status };
        p.kick();
        p
    }

    /// Look now (a disc finished, or the sidebar item was clicked).
    pub fn kick(&self) {
        let _ = self.kick.send(());
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

fn counts(s: &Settings) -> (usize, usize) {
    let o = Outbox::load(s);
    let failed = o.albums.iter().filter(|e| matches!(e.state, State::Failed { .. })).count();
    (o.pending().len(), failed)
}

fn run(s: Settings, ctx: egui::Context, log: Sender<String>, rx: Receiver<()>, status: Arc<Mutex<Status>>) {
    // Said once until it changes, not every five minutes.
    let mut last_away: Option<String> = None;
    loop {
        let prefs = Prefs::load(&s);
        let (waiting, failed) = counts(&s);
        {
            let mut st = status.lock().unwrap();
            st.enabled = prefs.server.enabled();
            st.waiting = waiting;
            st.failed = failed;
            if st.line.is_empty() {
                st.line = if waiting == 0 { "Everything ripped is on the server".into() } else { format!("{waiting} waiting") };
            }
        }
        ctx.request_repaint();
        if prefs.server.enabled() && waiting > 0 {
            status.lock().unwrap().sending = true;
            let set = |line: String| {
                status.lock().unwrap().line = line;
                ctx.request_repaint();
            };
            let r = push::push_pending(&s, &prefs.server, &mut |e| match e {
                PushEvent::Sending(a) => set(format!("Sending {a}")),
                PushEvent::Sent(a) => {
                    let _ = log.send(format!("Sent to the server: {a}"));
                    last_away = None;
                }
                PushEvent::Failed { album, error } => {
                    let _ = log.send(format!("Couldn't send {album} to the server: {error}"));
                }
                PushEvent::NotHome { waiting, why } => {
                    if last_away.as_deref() != Some(why.as_str()) {
                        let _ = log.send(format!("{waiting} album(s) waiting for the home network ({why})"));
                        last_away = Some(why.clone());
                    }
                    set(format!("{waiting} waiting for the home network"));
                }
                PushEvent::Busy(a) => set(format!("{a}: after the disc being ripped")),
                PushEvent::Scan(Ok(())) => {
                    let _ = log.send("The server is adding the new albums".into());
                }
                PushEvent::Scan(Err(_)) => {} // no login yet: it finds them within 10 minutes
            });
            let (waiting, failed) = counts(&s);
            let mut st = status.lock().unwrap();
            st.sending = false;
            st.waiting = waiting;
            st.failed = failed;
            match r {
                Ok(sum) if sum.already_running => st.line = "Another push is running".into(),
                Ok(_) if waiting == 0 => st.line = "Everything ripped is on the server".into(),
                Ok(_) if failed > 0 => st.line = format!("{waiting} waiting, {failed} failed (see the activity log)"),
                Ok(_) => {}
                Err(e) => {
                    let _ = log.send(format!("Sending to the server: {e:#}"));
                    st.line = format!("{e:#}");
                }
            }
            drop(st);
            ctx.request_repaint();
        }
        match rx.recv_timeout(EVERY) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {
                // Several kicks at once: one round.
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
