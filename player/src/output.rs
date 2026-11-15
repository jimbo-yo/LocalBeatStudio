//! Audio output. [`Backend`] opens a stream that pulls from a [`Renderer`];
//! the real one is cpal (ALSA on Linux, which PipeWire serves; WASAPI on
//! Windows), tests use a fake.

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::render::Renderer;

pub trait Backend: Send {
    /// Starts output, ideally at `rate`. Returns the rate the stream really
    /// runs at, its channel count, and a handle that stops it when dropped.
    /// `make` builds the renderer once the channel count is known.
    fn open(&mut self, rate: u32, make: &mut dyn FnMut(usize) -> Renderer) -> Result<(u32, Box<dyn std::any::Any>)>;
}

/// The system's default output device through cpal.
pub struct CpalBackend;

impl Backend for CpalBackend {
    fn open(&mut self, rate: u32, make: &mut dyn FnMut(usize) -> Renderer) -> Result<(u32, Box<dyn std::any::Any>)> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("no sound output device"))?;
        let (config, format) = pick_config(&device, rate)?;
        let renderer = make(config.channels as usize);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, renderer)?,
            SampleFormat::I16 => build::<i16>(&device, config, renderer)?,
            SampleFormat::I32 => build::<i32>(&device, config, renderer)?,
            SampleFormat::U16 => build::<u16>(&device, config, renderer)?,
            SampleFormat::F64 => build::<f64>(&device, config, renderer)?,
            f => return Err(anyhow!("sound device wants an unsupported sample format ({f})")),
        };
        stream.play().map_err(|e| anyhow!("couldn't start sound output: {e}"))?;
        Ok((config.sample_rate, Box::new(stream)))
    }
}

/// Stereo at the file's rate if the device can (float first); else the
/// device's own default (the engine then resamples).
fn pick_config(device: &cpal::Device, rate: u32) -> Result<(StreamConfig, SampleFormat)> {
    let ranges: Vec<_> = device.supported_output_configs().map(|r| r.collect()).unwrap_or_default();
    let rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I16 => 2,
        _ => 3,
    };
    let best = ranges
        .iter()
        .filter(|r| r.channels() == 2 && r.min_sample_rate() <= rate && rate <= r.max_sample_rate())
        .filter(|r| rank(r.sample_format()) < 3)
        .min_by_key(|r| rank(r.sample_format()));
    if let Some(r) = best {
        let c = r.with_sample_rate(rate);
        return Ok((StreamConfig { channels: 2, sample_rate: rate, buffer_size: cpal::BufferSize::Default }, c.sample_format()));
    }
    let d = device.default_output_config().map_err(|e| anyhow!("sound device has no usable format: {e}"))?;
    Ok((d.config(), d.sample_format()))
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut renderer: Renderer) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut buf: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if buf.len() < data.len() {
                    buf.resize(data.len(), 0.0);
                }
                let b = &mut buf[..data.len()];
                renderer.render(b);
                for (o, s) in data.iter_mut().zip(b.iter()) {
                    *o = T::from_sample(*s);
                }
            },
            |e| eprintln!("sound output: {e}"),
            None,
        )
        .map_err(|e| anyhow!("couldn't open sound output: {e}"))
}

#[cfg(test)]
pub mod fake {
    //! A sound card for tests: pulls from the renderer on its own thread,
    //! as fast as allowed, and records every frame that came from the ring.
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct FakeBackend {
        pub played: Arc<Mutex<Vec<f32>>>,
        /// Rates the engine opened the stream at.
        pub opened: Arc<Mutex<Vec<u32>>>,
        /// Set to make the device refuse every rate but this one.
        pub only_rate: Option<u32>,
    }

    struct Handle(Arc<AtomicBool>);
    impl Drop for Handle {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    impl Backend for FakeBackend {
        fn open(&mut self, rate: u32, make: &mut dyn FnMut(usize) -> Renderer) -> Result<(u32, Box<dyn std::any::Any>)> {
            let rate = self.only_rate.unwrap_or(rate);
            self.opened.lock().unwrap().push(rate);
            let mut r = make(2);
            let stop = Arc::new(AtomicBool::new(false));
            let (s, played) = (stop.clone(), self.played.clone());
            std::thread::spawn(move || {
                let mut out = vec![0.0f32; 512];
                while !s.load(Ordering::Relaxed) {
                    let n = r.render(&mut out);
                    played.lock().unwrap().extend_from_slice(&out[..n * 2]);
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
            });
            Ok((rate, Box::new(Handle(stop))))
        }
    }
}
