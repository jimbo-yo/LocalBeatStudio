//! Sample-rate conversion for the rare case the sound device can't run at
//! the file's own rate (PipeWire and PulseAudio take any rate, so on Linux
//! this normally never runs). Linear interpolation, streaming across
//! packets. Windows' shared mode will need it more; a better filter can
//! replace this then.

pub struct Linear {
    /// Input frames per output frame.
    step: f64,
    /// Position between `prev` (0) and the next input frame (1).
    t: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Linear {
    pub fn new(from: u32, to: u32) -> Self {
        Linear { step: from as f64 / to as f64, t: 0.0, prev: [0.0; 2], primed: false }
    }

    /// Converts interleaved stereo `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for frame in input.chunks_exact(2) {
            let x = [frame[0], frame[1]];
            if !self.primed {
                self.prev = x;
                self.primed = true;
                continue;
            }
            while self.t < 1.0 {
                let t = self.t as f32;
                out.push(self.prev[0] + (x[0] - self.prev[0]) * t);
                out.push(self.prev[1] + (x[1] - self.prev[1]) * t);
                self.t += self.step;
            }
            self.t -= 1.0;
            self.prev = x;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubling_the_rate_interpolates() {
        let mut r = Linear::new(1, 2);
        let mut out = Vec::new();
        r.process(&[0.0, 0.0, 1.0, 1.0, 2.0, 2.0], &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.5, 1.5]);
    }

    #[test]
    fn length_follows_the_ratio_across_chunks() {
        let mut r = Linear::new(44_100, 48_000);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..44_100 * 2).map(|i| (i / 2) as f32).collect();
        for c in input.chunks(2 * 1000) {
            r.process(c, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
    }
}
