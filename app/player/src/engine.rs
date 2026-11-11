//! The engine thread: owns the queue, the decoder and the output stream,
//! keeps the ring buffer topped up, and moves from song to song without a
//! gap (when the next song has the same sample rate; otherwise the output
//! is reopened at the new rate once the last song has finished playing).
//!
//! The playback clock is the callback's `consumed` frame counter; each song
//! (or seek) starts a [`Segment`] at the frame count where its audio begins
//! in the stream, so the position is exact even with ~0.75 s buffered.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::decode::{Format, Source};
use crate::output::Backend;
use crate::render::{Renderer, Shared};
use crate::resample::Linear;
use crate::State;

pub enum Cmd {
    Load(Vec<PathBuf>, usize),
    Append(Vec<PathBuf>),
    InsertNext(Vec<PathBuf>),
    Jump(usize),
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    Seek(f64),
    Quit,
}

/// Where a song's audio starts in the output stream.
#[derive(Clone, Debug)]
pub struct Segment {
    /// Output frame count at which it starts.
    pub start: u64,
    pub index: usize,
    /// Song position (s) at `start` (after a seek: where it landed).
    pub base: f64,
    /// Output rate while it plays.
    pub rate: u32,
    pub duration: Option<f64>,
    pub format: Format,
}

/// What the engine shares with [`crate::Player`] besides the callback state.
#[derive(Default)]
pub struct Common {
    pub segments: Mutex<VecDeque<Segment>>,
    pub info: Mutex<Info>,
}

#[derive(Default, Clone)]
pub struct Info {
    pub state: State,
    pub queue: Vec<PathBuf>,
    /// The last problem (a file that wouldn't play, no sound device).
    pub error: Option<String>,
    /// The song to resume from after Stop / the end of the queue.
    pub stopped_index: Option<usize>,
}

struct Out {
    _handle: Box<dyn std::any::Any>,
    producer: rtrb::Producer<f32>,
    /// Rate the stream runs at, and the rate it was asked for (a device
    /// that can't do a rate isn't reopened for every song at that rate).
    rate: u32,
    requested: u32,
}

pub struct Engine {
    backend: Box<dyn Backend>,
    shared: Arc<Shared>,
    common: Arc<Common>,
    out: Option<Out>,
    queue: Vec<PathBuf>,
    /// The song being decoded (can be ahead of the one heard).
    dec_index: usize,
    src: Option<Source>,
    resampler: Option<Linear>,
    /// Decoded audio not yet in the ring, and how much of it is.
    pending: Vec<f32>,
    pending_pos: usize,
    decoded: Vec<f32>,
    /// Frames pushed into the ring so far (same clock as `consumed`).
    written: u64,
    state: State,
    /// The queue has been decoded to the end: stop once it's heard.
    draining: bool,
    /// The next song needs another output rate: switch when the ring is empty.
    reopen_pending: Option<f64>,
}

impl Engine {
    pub fn new(backend: Box<dyn Backend>, shared: Arc<Shared>, common: Arc<Common>) -> Self {
        Engine {
            backend,
            shared,
            common,
            out: None,
            queue: Vec::new(),
            dec_index: 0,
            src: None,
            resampler: None,
            pending: Vec::new(),
            pending_pos: 0,
            decoded: Vec::new(),
            written: 0,
            state: State::Stopped,
            draining: false,
            reopen_pending: None,
        }
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    self.handle(cmd);
                    while let Ok(cmd) = rx.try_recv() {
                        if matches!(cmd, Cmd::Quit) {
                            return;
                        }
                        self.handle(cmd);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.fill();
            self.tick();
        }
    }

    fn consumed(&self) -> u64 {
        self.shared.consumed.load(Ordering::Acquire)
    }

    /// The song being heard and its position.
    fn current(&self) -> Option<(usize, f64)> {
        let segs = self.common.segments.lock().unwrap();
        let c = self.consumed();
        let s = segs.iter().rev().find(|s| s.start <= c).or(segs.front())?;
        Some((s.index, s.base + c.saturating_sub(s.start) as f64 / s.rate as f64))
    }

    fn set_error(&self, e: String) {
        self.common.info.lock().unwrap().error = Some(e);
    }

    fn publish(&self) {
        let mut info = self.common.info.lock().unwrap();
        info.state = self.state;
        info.queue = self.queue.clone();
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load(queue, start) => {
                self.queue = queue;
                self.common.info.lock().unwrap().error = None;
                self.play_from(start, 0.0);
            }
            Cmd::Append(v) => {
                let at = self.queue.len();
                self.queue.extend(v);
                if self.state != State::Stopped && self.src.is_none() && self.draining {
                    // The queue had run out but the last song is still playing:
                    // carry on into the new ones.
                    self.draining = false;
                    self.begin(at, 0.0, true);
                }
            }
            Cmd::InsertNext(v) => {
                let at = if self.queue.is_empty() { 0 } else { (self.dec_index + 1).min(self.queue.len()) };
                let n = v.len();
                self.queue.splice(at..at, v);
                if self.state != State::Stopped && self.src.is_none() && self.draining {
                    self.draining = false;
                    self.begin(at, 0.0, true);
                } else if self.dec_index >= at && self.src.is_some() {
                    self.dec_index += n;
                }
            }
            Cmd::Jump(i) => self.play_from(i, 0.0),
            Cmd::Play => match self.state {
                State::Stopped => {
                    let i = self.common.info.lock().unwrap().stopped_index.unwrap_or(0);
                    self.play_from(i, 0.0);
                }
                _ => self.set_paused(false),
            },
            Cmd::Pause => {
                if self.state == State::Playing {
                    self.set_paused(true);
                }
            }
            Cmd::Toggle => match self.state {
                State::Playing => self.set_paused(true),
                State::Paused => self.set_paused(false),
                State::Stopped => self.handle(Cmd::Play),
            },
            Cmd::Stop => self.stop(),
            Cmd::Next => {
                if let Some((i, _)) = self.current() {
                    if i + 1 < self.queue.len() {
                        self.play_from(i + 1, 0.0);
                    } else {
                        self.stop();
                    }
                }
            }
            Cmd::Prev => {
                if let Some((i, pos)) = self.current() {
                    let to = if pos > 3.0 || i == 0 { i } else { i - 1 };
                    self.play_from(to, 0.0);
                }
            }
            Cmd::Seek(secs) => {
                if let Some((i, _)) = self.current() {
                    if i == self.dec_index && self.src.is_some() && self.reopen_pending.is_none() {
                        self.flush();
                        let src = self.src.as_mut().unwrap();
                        match src.seek(secs) {
                            Ok(base) => {
                                if let Some(r) = &mut self.resampler {
                                    *r = Linear::new(src.info.sample_rate, self.out.as_ref().map_or(src.info.sample_rate, |o| o.rate));
                                }
                                self.push_segment(base);
                            }
                            Err(e) => {
                                self.set_error(format!("{e:#}"));
                                self.play_from(i, 0.0);
                            }
                        }
                    } else {
                        self.play_from(i, secs);
                    }
                }
            }
            Cmd::Quit => {}
        }
        self.publish();
    }

    fn set_paused(&mut self, p: bool) {
        self.shared.paused.store(p, Ordering::Relaxed);
        self.state = if p { State::Paused } else { State::Playing };
    }

    /// Starts song `i` at `at` seconds, dropping whatever is buffered.
    fn play_from(&mut self, i: usize, at: f64) {
        if i >= self.queue.len() {
            self.stop();
            return;
        }
        self.flush();
        self.draining = false;
        self.reopen_pending = None;
        self.shared.paused.store(false, Ordering::Relaxed);
        self.state = State::Playing;
        self.begin(i, at, false);
    }

    /// Opens song `i` (or the next one that plays) for decoding. `gapless`:
    /// it follows the song still in the buffer.
    fn begin(&mut self, mut i: usize, at: f64, gapless: bool) {
        let mut at = at;
        let src = loop {
            if i >= self.queue.len() {
                break None;
            }
            match Source::open(&self.queue[i]) {
                Ok(mut s) => {
                    if at > 0.0 {
                        at = s.seek(at).unwrap_or(0.0);
                    }
                    break Some(s);
                }
                Err(e) => {
                    let name = self.queue[i].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.set_error(format!("{name}: {e:#}"));
                    i += 1;
                    at = 0.0;
                }
            }
        };
        let Some(src) = src else {
            self.src = None;
            if gapless {
                self.draining = true;
            } else {
                self.stop();
            }
            return;
        };
        let rate = src.info.sample_rate;
        self.dec_index = i;
        self.src = Some(src);
        let same_rate = self.out.as_ref().is_some_and(|o| o.requested == rate);
        if same_rate {
            self.set_resampler();
            self.push_segment(at);
        } else if gapless && self.out.is_some() {
            // Let the previous song finish at its rate first.
            self.reopen_pending = Some(at);
        } else {
            self.reopen(at);
        }
    }

    /// (Re)opens the output at the current song's rate.
    fn reopen(&mut self, at: f64) {
        let Some(rate) = self.src.as_ref().map(|s| s.info.sample_rate) else { return };
        self.out = None;
        let cap = (rate as usize * 2 * 3 / 4).max(8192);
        let (producer, consumer) = rtrb::RingBuffer::new(cap);
        let shared = self.shared.clone();
        let mut consumer = Some(consumer);
        let mut make = |channels: usize| Renderer::new(consumer.take().expect("one renderer"), shared.clone(), channels);
        match self.backend.open(rate, &mut make) {
            Ok((real, handle)) => {
                self.written = self.consumed();
                self.out = Some(Out { _handle: handle, producer, rate: real, requested: rate });
                self.set_resampler();
                self.push_segment(at);
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.src = None;
                self.stop();
            }
        }
    }

    fn set_resampler(&mut self) {
        let (Some(src), Some(out)) = (&self.src, &self.out) else { return };
        let r = src.info.sample_rate;
        self.resampler = (r != out.rate).then(|| Linear::new(r, out.rate));
    }

    fn push_segment(&mut self, base: f64) {
        let (Some(src), Some(out)) = (&self.src, &self.out) else { return };
        let seg = Segment {
            start: self.written,
            index: self.dec_index,
            base,
            rate: out.rate,
            duration: src.duration,
            format: src.info.clone(),
        };
        self.common.segments.lock().unwrap().push_back(seg);
    }

    /// Drops everything buffered (the callback empties the ring).
    fn flush(&mut self) {
        if self.out.is_some() {
            self.shared.flush.store(true, Ordering::Release);
            let t = Instant::now();
            while self.shared.flush.load(Ordering::Acquire) && t.elapsed() < Duration::from_millis(300) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.shared.flush.store(false, Ordering::Release);
        }
        self.written = self.consumed();
        self.pending.clear();
        self.pending_pos = 0;
        self.common.segments.lock().unwrap().clear();
    }

    fn stop(&mut self) {
        let at = self.current().map(|(i, _)| i).or_else(|| self.common.info.lock().unwrap().stopped_index);
        self.flush();
        self.src = None;
        self.out = None; // releases the sound device
        self.draining = false;
        self.reopen_pending = None;
        self.shared.paused.store(false, Ordering::Relaxed);
        self.state = State::Stopped;
        self.common.info.lock().unwrap().stopped_index = at;
        self.publish();
    }

    /// Keeps the ring topped up.
    fn fill(&mut self) {
        loop {
            if let Some(at) = self.reopen_pending {
                if self.consumed() >= self.written {
                    self.reopen_pending = None;
                    self.reopen(at);
                }
                return;
            }
            let Some(out) = &mut self.out else { return };
            if self.pending_pos < self.pending.len() {
                let n = (out.producer.slots().min(self.pending.len() - self.pending_pos)) & !1;
                if n == 0 {
                    return;
                }
                if let Ok(mut chunk) = out.producer.write_chunk(n) {
                    let (a, b) = chunk.as_mut_slices();
                    let src = &self.pending[self.pending_pos..self.pending_pos + n];
                    a.copy_from_slice(&src[..a.len()]);
                    b.copy_from_slice(&src[a.len()..]);
                    chunk.commit_all();
                }
                self.pending_pos += n;
                self.written += (n / 2) as u64;
                continue;
            }
            self.pending.clear();
            self.pending_pos = 0;
            if out.producer.slots() < 8192 {
                return;
            }
            let Some(src) = &mut self.src else { return };
            self.decoded.clear();
            match src.next(&mut self.decoded) {
                Ok(true) => match &mut self.resampler {
                    Some(r) => r.process(&self.decoded, &mut self.pending),
                    None => std::mem::swap(&mut self.pending, &mut self.decoded),
                },
                Ok(false) => self.after_song(),
                Err(e) => {
                    self.set_error(format!("{e:#}"));
                    self.after_song();
                }
            }
        }
    }

    fn after_song(&mut self) {
        let next = self.dec_index + 1;
        if next < self.queue.len() {
            self.begin(next, 0.0, true);
        } else {
            self.src = None;
            self.draining = true;
        }
    }

    fn tick(&mut self) {
        let c = self.consumed();
        if self.draining && self.state != State::Stopped && c >= self.written {
            // Everything has been heard.
            self.common.info.lock().unwrap().stopped_index = Some(0);
            let keep_err = self.common.info.lock().unwrap().error.clone();
            self.stop();
            self.common.info.lock().unwrap().error = keep_err;
            self.common.info.lock().unwrap().stopped_index = Some(0);
        }
        // Forget songs that have been heard (keep the one playing).
        let mut segs = self.common.segments.lock().unwrap();
        while segs.len() > 1 && segs[1].start <= c {
            segs.pop_front();
        }
    }
}
