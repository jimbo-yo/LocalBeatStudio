//! ripshelf's music player engine (no UI): plays a queue of files, FLAC
//! first but also MP3, AAC/ALAC (.m4a), Ogg Vorbis, WAV and AIFF, without
//! gaps between songs, with seeking, pause and volume.
//!
//! ```no_run
//! let p = ripshelf_player::Player::new();
//! p.load(vec!["song.flac".into()], 0);
//! let s = p.status(); // state, position, format...
//! ```

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Sender, channel};
use std::sync::Arc;
use std::thread::JoinHandle;

mod decode;
mod engine;
mod output;
mod render;
mod resample;

pub use decode::Format;
use engine::{Cmd, Common, Engine};
use output::{Backend, CpalBackend};
use render::Shared;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// A snapshot for the UI.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: State,
    /// The song being heard (index into the queue).
    pub index: Option<usize>,
    pub path: Option<PathBuf>,
    /// Seconds into the song.
    pub position: f64,
    pub duration: Option<f64>,
    pub format: Option<Format>,
    pub queue_len: usize,
    pub volume: f32,
    /// The last problem (a song that wouldn't play, no sound device).
    pub error: Option<String>,
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    common: Arc<Common>,
    thread: Option<JoinHandle<()>>,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    /// A player on the system's default sound output.
    pub fn new() -> Self {
        Self::with_backend(Box::new(CpalBackend))
    }

    fn with_backend(backend: Box<dyn Backend>) -> Self {
        let (tx, rx) = channel();
        let shared = Shared::new();
        let common = Arc::new(Common::default());
        let (sh, co) = (shared.clone(), common.clone());
        // Built on its own thread: the output stream it opens stays there.
        let thread = std::thread::Builder::new()
            .name("player".into())
            .spawn(move || Engine::new(backend, sh, co).run(rx))
            .expect("player thread");
        Player { tx, shared, common, thread: Some(thread) }
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    /// Replaces the queue and plays from `start`.
    pub fn load(&self, queue: Vec<PathBuf>, start: usize) {
        self.send(Cmd::Load(queue, start));
    }
    /// Sets the queue with song `index` at `at` seconds ready to play
    /// (paused, nothing opened): Play starts there. For restoring a session.
    pub fn cue(&self, queue: Vec<PathBuf>, index: usize, at: f64) {
        self.send(Cmd::Cue(queue, index, at));
    }
    /// Adds songs to the end of the queue.
    pub fn append(&self, songs: Vec<PathBuf>) {
        self.send(Cmd::Append(songs));
    }
    /// Puts songs right after the current one.
    pub fn insert_next(&self, songs: Vec<PathBuf>) {
        self.send(Cmd::InsertNext(songs));
    }
    /// Plays song `i` of the queue.
    pub fn jump(&self, i: usize) {
        self.send(Cmd::Jump(i));
    }
    pub fn play(&self) {
        self.send(Cmd::Play);
    }
    pub fn pause(&self) {
        self.send(Cmd::Pause);
    }
    pub fn toggle(&self) {
        self.send(Cmd::Toggle);
    }
    pub fn stop(&self) {
        self.send(Cmd::Stop);
    }
    pub fn next(&self) {
        self.send(Cmd::Next);
    }
    /// Back to the start of the song, or the previous song if within 3 s.
    pub fn prev(&self) {
        self.send(Cmd::Prev);
    }
    pub fn seek(&self, secs: f64) {
        self.send(Cmd::Seek(secs));
    }
    /// Play the queue again from the top when it ends.
    pub fn set_repeat(&self, on: bool) {
        self.send(Cmd::Repeat(on));
    }
    /// 0..1, applied as is (the UI picks the curve).
    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    pub fn queue(&self) -> Vec<PathBuf> {
        self.common.info.lock().unwrap().queue.clone()
    }

    pub fn status(&self) -> Status {
        let info = self.common.info.lock().unwrap().clone();
        let volume = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let mut s = Status { state: info.state, queue_len: info.queue.len(), volume, error: info.error, ..Default::default() };
        if info.state == State::Stopped {
            s.index = info.stopped_index.filter(|&i| i < info.queue.len());
            s.position = info.resume_at;
        } else {
            let segs = self.common.segments.lock().unwrap();
            let c = self.shared.consumed.load(Ordering::Acquire);
            if let Some(seg) = segs.iter().rev().find(|x| x.start <= c).or(segs.front()) {
                let pos = seg.base + c.saturating_sub(seg.start) as f64 / seg.rate as f64;
                s.index = Some(seg.index);
                s.position = seg.duration.map_or(pos, |d| pos.min(d));
                s.duration = seg.duration;
                s.format = Some(seg.format.clone());
            }
        }
        s.path = s.index.and_then(|i| info.queue.get(i).cloned());
        s
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::tests::{tmp, write_wav};
    use crate::output::fake::FakeBackend;
    use std::time::{Duration, Instant};

    fn wait(p: &Player, what: &str, f: impl Fn(&Status) -> bool) -> Status {
        let t = Instant::now();
        loop {
            let s = p.status();
            if f(&s) {
                return s;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "timed out waiting for {what}: {s:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Sample values (as 16-bit integers) of what was played, left channel.
    fn left(played: &[f32]) -> Vec<i32> {
        played.chunks(2).map(|f| (f[0] * 32768.0).round() as i32).collect()
    }

    #[test]
    fn plays_a_queue_without_gaps() {
        let (a, b) = (tmp("gap-a.wav"), tmp("gap-b.wav"));
        write_wav(&a, 8000, 2, 3000, |n| (n + 1) as i16);
        write_wav(&b, 8000, 2, 2000, |n| -((n + 1) as i16));
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.load(vec![a.clone(), b.clone()], 0);
        wait(&p, "playing", |s| s.state == State::Playing);
        let s = wait(&p, "second song", |s| s.index == Some(1));
        assert_eq!(s.path.as_deref(), Some(b.as_path()));
        wait(&p, "the end", |s| s.state == State::Stopped);
        let got = left(&fake.played.lock().unwrap());
        let want: Vec<i32> = (1..=3000).chain((1..=2000).map(|n: i32| -n)).collect();
        assert_eq!(got, want, "every frame of both songs, back to back");
        assert_eq!(fake.opened.lock().unwrap().len(), 1, "one stream for both");
    }

    #[test]
    fn seek_and_position() {
        let a = tmp("seek-a.wav");
        write_wav(&a, 8000, 2, 80_000, |n| (n % 20_000) as i16);
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.load(vec![a], 0);
        wait(&p, "playing", |s| s.state == State::Playing && s.position > 0.0);
        p.pause();
        wait(&p, "paused", |s| s.state == State::Paused);
        fake.played.lock().unwrap().clear();
        p.seek(5.0);
        p.play();
        let s = wait(&p, "position after seek", |s| s.position >= 5.0);
        assert!(s.position < 6.0, "{}", s.position);
        assert_eq!(s.duration, Some(10.0));
        // The first frame heard after the seek is frame 40000 (= 0 here).
        let t = Instant::now();
        while fake.played.lock().unwrap().is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(left(&fake.played.lock().unwrap())[..3], [0, 1, 2]);
    }

    #[test]
    fn bad_files_are_skipped_and_reported() {
        let (bad, good) = (tmp("bad.flac"), tmp("good.wav"));
        std::fs::write(&bad, b"not audio").unwrap();
        write_wav(&good, 8000, 2, 400, |n| n as i16);
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.load(vec![bad, good], 0);
        let s = wait(&p, "the good one", |s| s.index == Some(1));
        assert!(s.error.unwrap().contains("bad.flac"));
        wait(&p, "the end", |s| s.state == State::Stopped);
    }

    #[test]
    fn another_rate_reopens_after_the_first_song() {
        let (a, b) = (tmp("rate-a.wav"), tmp("rate-b.wav"));
        write_wav(&a, 8000, 2, 800, |_| 100);
        write_wav(&b, 16000, 2, 1600, |_| 200);
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.load(vec![a, b], 0);
        wait(&p, "playing", |s| s.state == State::Playing);
        wait(&p, "the end", |s| s.state == State::Stopped);
        assert_eq!(*fake.opened.lock().unwrap(), vec![8000, 16000]);
        let got = left(&fake.played.lock().unwrap());
        assert_eq!(got.len(), 2400);
        assert!(got[..800].iter().all(|&v| v == 100) && got[800..].iter().all(|&v| v == 200));
    }

    #[test]
    fn resamples_when_the_device_cant_follow() {
        let a = tmp("rs-a.wav");
        write_wav(&a, 8000, 2, 8000, |_| 1000);
        let fake = FakeBackend { only_rate: Some(16000), ..Default::default() };
        let p = Player::with_backend(Box::new(fake.clone()));
        p.load(vec![a], 0);
        wait(&p, "playing", |s| s.state == State::Playing);
        wait(&p, "the end", |s| s.state == State::Stopped);
        let n = fake.played.lock().unwrap().len() / 2;
        assert!((n as i64 - 16000).abs() <= 2, "{n}");
    }

    #[test]
    fn repeat_wraps_to_the_top() {
        let (a, b) = (tmp("rep-a.wav"), tmp("rep-b.wav"));
        write_wav(&a, 8000, 2, 400, |_| 5);
        write_wav(&b, 8000, 2, 400, |_| 6);
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.set_repeat(true);
        p.load(vec![a, b], 0);
        let t = Instant::now();
        while fake.played.lock().unwrap().len() < 2 * 2000 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let got = left(&fake.played.lock().unwrap());
        assert!(got.len() >= 2000, "kept playing: {}", got.len());
        assert_eq!(&got[..1600], &[vec![5; 400], vec![6; 400], vec![5; 400], vec![6; 400]].concat()[..]);
        assert_eq!(p.status().state, State::Playing);
        p.set_repeat(false);
    }

    #[test]
    fn cue_waits_then_plays_from_the_spot() {
        let a = tmp("cue-a.wav");
        let b = tmp("cue-b.wav");
        write_wav(&a, 8000, 2, 8000, |_| 1);
        write_wav(&b, 8000, 2, 80_000, |n| (n % 20_000) as i16);
        let fake = FakeBackend::default();
        let p = Player::with_backend(Box::new(fake.clone()));
        p.cue(vec![a, b.clone()], 1, 2.0);
        let s = wait(&p, "cued", |s| s.index == Some(1));
        assert_eq!(s.state, State::Stopped);
        assert_eq!(s.position, 2.0);
        assert!(fake.opened.lock().unwrap().is_empty(), "no sound device until Play");
        p.play();
        wait(&p, "playing b", |s| s.state == State::Playing && s.index == Some(1));
        let t = Instant::now();
        while fake.played.lock().unwrap().is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(left(&fake.played.lock().unwrap())[0], 16000);
    }

    #[test]
    fn next_prev_and_insert_next() {
        let (a, b, c) = (tmp("n-a.wav"), tmp("n-b.wav"), tmp("n-c.wav"));
        for f in [&a, &b, &c] {
            write_wav(f, 8000, 2, 80_000, |_| 1);
        }
        let p = Player::with_backend(Box::new(FakeBackend::default()));
        p.load(vec![a.clone(), b.clone()], 0);
        wait(&p, "a", |s| s.index == Some(0) && s.state == State::Playing);
        p.insert_next(vec![c.clone()]);
        p.next();
        let s = wait(&p, "c next", |s| s.index == Some(1));
        assert_eq!(s.path.as_deref(), Some(c.as_path()));
        assert_eq!(p.queue(), vec![a, c, b]);
        p.prev();
        wait(&p, "back to a", |s| s.index == Some(0));
        p.stop();
        let s = wait(&p, "stopped", |s| s.state == State::Stopped);
        assert_eq!(s.index, Some(0));
    }
}
