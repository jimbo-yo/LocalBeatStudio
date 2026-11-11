//! Phone notifications for the rip loop. Once a drive has waited `after` for
//! the next disc, one message says how the last disc went ("OK Computer: all
//! 12 tracks in the library (8:12). Ready for the next disc."). Problems
//! (unreadable tracks, a failed disc) are sent straight away. Swapping discs
//! within `after` sends nothing.
//!
//! Feed it every [`Event`] with [`Notifier::observe`]; a background thread
//! does the timing. Sending is a closure, so front ends choose how (KDE
//! Connect in practice) and what to do when it fails.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::Event;

pub type Send = Box<dyn Fn(&str) + std::marker::Send + Sync>;

pub struct Notifier {
    enabled: AtomicBool,
    after: Duration,
    send: Send,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    label: String,
    total: usize,
    /// How the last disc went, once it's done.
    summary: Option<String>,
    waiting_since: Option<Instant>,
    sent: bool,
}

impl Notifier {
    /// Starts the timing thread; it ends when the last `Arc` is dropped.
    pub fn new(enabled: bool, after: Duration, send: Send) -> Arc<Self> {
        let n = Arc::new(Notifier { enabled: AtomicBool::new(enabled), after, send, state: Mutex::default() });
        let weak: Weak<Notifier> = Arc::downgrade(&n);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                match weak.upgrade() {
                    Some(n) => n.tick(Instant::now()),
                    None => break,
                }
            }
        });
        n
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn observe(&self, e: &Event) {
        let mut now_msg = None;
        {
            let mut st = self.state.lock().unwrap();
            match e {
                Event::DiscStart { .. } => *st = State::default(),
                Event::Plan { label, tracks, .. } => {
                    st.label = label.clone();
                    st.total = tracks.len();
                }
                Event::RipDone { rejected, elapsed, .. } => {
                    let label = if st.label.is_empty() { "The disc".to_string() } else { st.label.clone() };
                    let took = fmt_duration(*elapsed);
                    if rejected.is_empty() {
                        st.summary = Some(format!("{label}: all {} tracks in the library ({took})", st.total));
                    } else {
                        let list = rejected.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
                        let ok = st.total.saturating_sub(rejected.len());
                        st.summary = Some(format!("{label}: {ok} of {} tracks; couldn't read {list} ({took})", st.total));
                        now_msg = Some(format!(
                            "{label}: couldn't read track(s) {list}. Clean the disc and rip it again later: only those get re-read."
                        ));
                    }
                }
                Event::DiscFailed(msg) => {
                    let label = if st.label.is_empty() { "A disc".to_string() } else { st.label.clone() };
                    st.summary = Some(format!("{label} failed: {msg}"));
                    now_msg = Some(format!("{label} failed: {msg}"));
                }
                Event::DiscStopped => {
                    let label = if st.label.is_empty() { "The disc".to_string() } else { st.label.clone() };
                    st.summary = Some(format!("{label}: stopped and ejected"));
                }
                Event::Waiting { done } if *done > 0 => {
                    st.waiting_since = Some(Instant::now());
                    st.sent = false;
                }
                _ => {}
            }
        }
        if let Some(m) = now_msg.filter(|_| self.enabled()) {
            (self.send)(&m);
        }
    }

    fn tick(&self, now: Instant) {
        let msg = {
            let mut st = self.state.lock().unwrap();
            let due = st.waiting_since.is_some_and(|t| now.duration_since(t) >= self.after);
            if !due || st.sent || !self.enabled() {
                return;
            }
            st.sent = true;
            match &st.summary {
                Some(s) => format!("{s}. Ready for the next disc."),
                None => "Ready for the next disc.".to_string(),
            }
        };
        (self.send)(&msg);
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrackInfo;
    use std::path::PathBuf;

    fn notifier(after: Duration) -> (Arc<Notifier>, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let n = Notifier::new(true, after, Box::new(move |m| s.lock().unwrap().push(m.to_string())));
        (n, sent)
    }

    fn disc(n: &Notifier, label: &str, tracks: usize, rejected: Vec<usize>) {
        n.observe(&Event::DiscStart { drive: "/dev/sr1".into(), model: "hp".into(), offset: 102 });
        let tracks = (1..=tracks)
            .map(|p| TrackInfo { position: p, title: String::new(), artist: String::new(), seconds: 60, exists: false })
            .collect();
        n.observe(&Event::Plan { album_dir: PathBuf::new(), label: label.into(), tracks });
        n.observe(&Event::RipDone { accepted: 0, rejected, skipped: 0, elapsed: Duration::from_secs(492) });
        n.observe(&Event::DiscDone { ok: true });
        n.observe(&Event::Waiting { done: 1 });
    }

    #[test]
    fn summary_after_waiting_once() {
        let (n, sent) = notifier(Duration::from_secs(60));
        disc(&n, "OK Computer", 12, vec![]);
        let t0 = n.state.lock().unwrap().waiting_since.unwrap();
        n.tick(t0 + Duration::from_secs(30));
        assert!(sent.lock().unwrap().is_empty(), "too early");
        n.tick(t0 + Duration::from_secs(61));
        n.tick(t0 + Duration::from_secs(120));
        assert_eq!(*sent.lock().unwrap(), ["OK Computer: all 12 tracks in the library (8:12). Ready for the next disc."]);
    }

    #[test]
    fn swapping_quickly_sends_nothing() {
        let (n, sent) = notifier(Duration::from_secs(60));
        disc(&n, "A", 10, vec![]);
        let t0 = n.state.lock().unwrap().waiting_since.unwrap();
        n.observe(&Event::DiscStart { drive: "/dev/sr1".into(), model: "hp".into(), offset: 102 });
        n.tick(t0 + Duration::from_secs(90));
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn problems_right_away_and_off_means_off() {
        let (n, sent) = notifier(Duration::from_secs(60));
        disc(&n, "Zeppelin", 15, vec![11, 15]);
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(sent.lock().unwrap()[0].contains("couldn't read track(s) 11, 15"));
        n.set_enabled(false);
        disc(&n, "B", 10, vec![3]);
        let t0 = n.state.lock().unwrap().waiting_since.unwrap();
        n.tick(t0 + Duration::from_secs(61));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
