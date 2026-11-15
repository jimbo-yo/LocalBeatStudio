//! The audio callback's side: takes stereo frames from the ring buffer the
//! engine fills, applies volume and pause, and empties the ring on request
//! (seek, new song) without any locks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// State the engine thread and the callback share.
#[derive(Default)]
pub struct Shared {
    pub paused: AtomicBool,
    /// Set by the engine; the callback empties the ring and clears it.
    pub flush: AtomicBool,
    /// Frames taken from the ring so far (the playback clock).
    pub consumed: AtomicU64,
    /// Volume as f32 bits (0..1).
    pub volume: AtomicU32,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        let s = Shared::default();
        s.volume.store(1.0f32.to_bits(), Ordering::Relaxed);
        Arc::new(s)
    }
}

pub struct Renderer {
    pub consumer: rtrb::Consumer<f32>,
    pub shared: Arc<Shared>,
    /// Channels of the output stream (the ring is always stereo).
    pub channels: usize,
    scratch: Vec<f32>,
}

impl Renderer {
    pub fn new(consumer: rtrb::Consumer<f32>, shared: Arc<Shared>, channels: usize) -> Self {
        Renderer { consumer, shared, channels: channels.max(1), scratch: Vec::new() }
    }

    /// Fills `out` (interleaved, `channels` per frame). Returns how many
    /// frames came from the ring; the rest is silence.
    pub fn render(&mut self, out: &mut [f32]) -> usize {
        let sh = Arc::clone(&self.shared);
        if sh.flush.load(Ordering::Acquire) {
            let n = self.consumer.slots();
            if let Ok(c) = self.consumer.read_chunk(n) {
                c.commit_all();
            }
            sh.flush.store(false, Ordering::Release);
        }
        if sh.paused.load(Ordering::Relaxed) {
            out.fill(0.0);
            return 0;
        }
        let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
        let frames_wanted = out.len() / self.channels;
        let frames = (self.consumer.slots() / 2).min(frames_wanted);
        if self.channels == 2 {
            self.read_into(frames, |i, s| out[i] = s * vol);
        } else {
            // Stereo into the first two channels (mono: the average).
            self.scratch.resize(frames * 2, 0.0);
            let mut tmp = std::mem::take(&mut self.scratch);
            self.read_into(frames, |i, s| tmp[i] = s);
            let ch = self.channels;
            for f in 0..frames {
                let (l, r) = (tmp[2 * f] * vol, tmp[2 * f + 1] * vol);
                let frame = &mut out[f * ch..f * ch + ch];
                frame.fill(0.0);
                if ch == 1 {
                    frame[0] = 0.5 * (l + r);
                } else {
                    frame[0] = l;
                    frame[1] = r;
                }
            }
            self.scratch = tmp;
        }
        out[frames * self.channels..].fill(0.0);
        sh.consumed.fetch_add(frames as u64, Ordering::Release);
        frames
    }

    fn read_into(&mut self, frames: usize, mut put: impl FnMut(usize, f32)) {
        if frames == 0 {
            return;
        }
        if let Ok(chunk) = self.consumer.read_chunk(frames * 2) {
            let (a, b) = chunk.as_slices();
            for (i, s) in a.iter().chain(b.iter()).enumerate() {
                put(i, *s);
            }
            chunk.commit_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(channels: usize) -> (rtrb::Producer<f32>, Renderer) {
        let (p, c) = rtrb::RingBuffer::new(64);
        (p, Renderer::new(c, Shared::new(), channels))
    }

    #[test]
    fn plays_then_pads_with_silence() {
        let (mut p, mut r) = setup(2);
        for s in [0.1, 0.2, 0.3, 0.4] {
            p.push(s).unwrap();
        }
        let mut out = [9.0f32; 8];
        assert_eq!(r.render(&mut out), 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(r.shared.consumed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pause_keeps_the_audio_and_flush_drops_it() {
        let (mut p, mut r) = setup(2);
        for _ in 0..8 {
            p.push(0.5).unwrap();
        }
        r.shared.paused.store(true, Ordering::Relaxed);
        let mut out = [1.0f32; 4];
        assert_eq!(r.render(&mut out), 0);
        assert_eq!(out, [0.0; 4]);
        r.shared.paused.store(false, Ordering::Relaxed);
        r.shared.volume.store(0.5f32.to_bits(), Ordering::Relaxed);
        assert_eq!(r.render(&mut out), 2);
        assert_eq!(out, [0.25; 4]);
        r.shared.flush.store(true, Ordering::Relaxed);
        assert_eq!(r.render(&mut out), 0);
        assert!(!r.shared.flush.load(Ordering::Relaxed));
    }

    #[test]
    fn stereo_into_six_channels() {
        let (mut p, mut r) = setup(6);
        p.push(0.1).unwrap();
        p.push(0.2).unwrap();
        let mut out = [9.0f32; 6];
        assert_eq!(r.render(&mut out), 1);
        assert_eq!(out, [0.1, 0.2, 0.0, 0.0, 0.0, 0.0]);
    }
}
