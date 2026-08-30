//! Audio playback for the media viewer: a ring of interleaved stereo samples
//! drained by a cpal output stream.
//!
//! This is the mirror image of [`crate::audio::capture`], which only ever opens
//! *input* streams. The important rule here is that the ring counts the frames
//! it actually **played**: silence written during an underrun does not count, so
//! [`crate::player::Clock`] stalls rather than running ahead of the decoder.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};

use super::Shared;

/// How much decoded audio to keep ahead of the device.
pub const HIGH_WATER_MS: u64 = 300;

/// Interleaved stereo samples waiting for the device.
pub struct Ring {
    inner: Mutex<VecDeque<f32>>,
    /// Frames handed to the device for real, never silence.
    played: AtomicU64,
    cap_frames: usize,
}

impl Ring {
    pub fn new(cap_frames: usize) -> Arc<Ring> {
        Arc::new(Ring {
            inner: Mutex::new(VecDeque::with_capacity(cap_frames * 2)),
            played: AtomicU64::new(0),
            cap_frames: cap_frames.max(1),
        })
    }

    /// Appends interleaved stereo samples, dropping the oldest if the decoder
    /// somehow outruns the cap (it should not: the worker watches
    /// [`Ring::queued_frames`]).
    pub fn push(&self, stereo: &[f32]) {
        let mut q = self.inner.lock().unwrap();
        q.extend(stereo.iter().copied());
        let cap = self.cap_frames * 2;
        while q.len() > cap {
            q.pop_front();
        }
    }

    pub fn queued_frames(&self) -> usize {
        self.inner.lock().unwrap().len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    pub fn frames_played(&self) -> u64 {
        self.played.load(Ordering::Relaxed)
    }

    /// Fills one device buffer, expanding stereo to `channels` and applying
    /// `gain`. Returns how many frames came from the ring; the rest of `out` is
    /// silence and is deliberately **not** counted.
    pub fn fill(&self, out: &mut [f32], channels: usize, gain: f32) -> usize {
        out.fill(0.0);
        if channels == 0 {
            return 0;
        }
        let mut q = self.inner.lock().unwrap();
        let mut taken = 0;
        for frame in out.chunks_mut(channels) {
            if q.len() < 2 {
                break;
            }
            let l = q.pop_front().unwrap_or(0.0) * gain;
            let r = q.pop_front().unwrap_or(0.0) * gain;
            frame[0] = l;
            if channels > 1 {
                frame[1] = r;
            }
            taken += 1;
        }
        drop(q);
        self.played.fetch_add(taken as u64, Ordering::Relaxed);
        taken
    }
}

/// Anything a device buffer can hold, written from our `f32` mix.
trait FromF32: cpal::SizedSample {
    fn from_f32(v: f32) -> Self;
}

impl FromF32 for f32 {
    fn from_f32(v: f32) -> f32 {
        v
    }
}

impl FromF32 for i16 {
    fn from_f32(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0) as i16
    }
}

impl FromF32 for u16 {
    fn from_f32(v: f32) -> u16 {
        ((v.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16
    }
}

impl FromF32 for i32 {
    fn from_f32(v: f32) -> i32 {
        (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32
    }
}

/// An open output stream. Thread-affine (`cpal::Stream` is `!Send`), so it must
/// live and die on the worker thread that built it.
pub struct Output {
    stream: Stream,
    pub ring: Arc<Ring>,
    pub rate: u32,
    pub channels: u16,
}

impl Output {
    /// Opens the default output device. The callback pulls from the returned
    /// [`Ring`] and reports progress to `shared`'s clock.
    pub(crate) fn open(shared: Arc<Shared>) -> Result<Output> {
        let host = cpal::default_host();
        let device = host.default_output_device().context("no default output device")?;
        let supported = device.default_output_config().context("output device default config")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels;
        // A second of slack: plenty for the 300 ms high-water mark, small
        // enough that a seek throws away almost nothing.
        let ring = Ring::new(rate as usize);

        let stream = match sample_format {
            SampleFormat::F32 => build::<f32>(&device, &config, &ring, &shared, rate)?,
            SampleFormat::I16 => build::<i16>(&device, &config, &ring, &shared, rate)?,
            SampleFormat::U16 => build::<u16>(&device, &config, &ring, &shared, rate)?,
            SampleFormat::I32 => build::<i32>(&device, &config, &ring, &shared, rate)?,
            other => return Err(anyhow!("unsupported output sample format {other:?}")),
        };
        log::info!("playback: {rate} Hz, {channels} ch, {sample_format:?}");
        Ok(Output { stream, ring, rate, channels })
    }

    pub fn play(&self) {
        if let Err(e) = self.stream.play() {
            log::warn!("playback stream play: {e}");
        }
    }

    pub fn pause(&self) {
        if let Err(e) = self.stream.pause() {
            log::warn!("playback stream pause: {e}");
        }
    }

    /// Frames to buffer before the device runs dry.
    pub fn high_water_frames(&self) -> usize {
        (self.rate as u64 * HIGH_WATER_MS / 1000) as usize
    }
}

fn build<T: FromF32>(
    device: &Device,
    config: &StreamConfig,
    ring: &Arc<Ring>,
    shared: &Arc<Shared>,
    rate: u32,
) -> Result<Stream> {
    let channels = config.channels as usize;
    let ring = ring.clone();
    let shared = shared.clone();
    // Reused across callbacks so the audio thread never allocates.
    let mut scratch: Vec<f32> = Vec::new();
    let stream = device.build_output_stream(
        *config,
        move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
            if scratch.len() != out.len() {
                scratch.resize(out.len(), 0.0);
            }
            ring.fill(&mut scratch, channels, shared.gain());
            for (dst, src) in out.iter_mut().zip(scratch.iter()) {
                *dst = T::from_f32(*src);
            }
            // How much of what we just wrote is still ahead of the speakers.
            let ts = info.timestamp();
            let ahead = ts.playback.duration_since(ts.callback);
            let latency = (ahead.as_secs_f64() * rate as f64) as u32;
            shared.clock.tick(ring.frames_played(), latency);
        },
        |e| log::warn!("playback stream: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Folds an interleaved buffer of `channels` to stereo, exactly as
/// [`crate::audio::mixer`] does for capture: mono is duplicated, anything wider
/// keeps the first two channels.
pub fn fold_to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    let ch = channels.max(1);
    out.clear();
    let frames = input.len() / ch;
    out.reserve(frames * 2);
    for f in 0..frames {
        let base = f * ch;
        let l = input[base];
        let r = if ch > 1 { input[base + 1] } else { l };
        out.push(l);
        out.push(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_duplicates_mono_and_trims_surround() {
        let mut out = Vec::new();
        fold_to_stereo(&[0.5, -0.5], 1, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
        // 5.1: only front left / right survive.
        fold_to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn ring_counts_only_frames_it_really_played() {
        let ring = Ring::new(1024);
        ring.push(&[1.0, -1.0, 0.5, -0.5]); // two stereo frames
        assert_eq!(ring.queued_frames(), 2);

        // The device asks for four frames but only two are there.
        let mut buf = vec![9.0; 8];
        let taken = ring.fill(&mut buf, 2, 1.0);
        assert_eq!(taken, 2);
        assert_eq!(ring.frames_played(), 2);
        assert_eq!(&buf[..4], &[1.0, -1.0, 0.5, -0.5]);
        // The starved half is silence, not stale data.
        assert_eq!(&buf[4..], &[0.0; 4]);

        // A wholly starved callback must not advance the clock at all.
        let taken = ring.fill(&mut buf, 2, 1.0);
        assert_eq!(taken, 0);
        assert_eq!(ring.frames_played(), 2, "silence must never count as played");
    }

    #[test]
    fn gain_scales_and_mute_silences() {
        let ring = Ring::new(64);
        ring.push(&[1.0, 1.0]);
        let mut buf = vec![0.0; 2];
        ring.fill(&mut buf, 2, 0.25);
        assert_eq!(buf, vec![0.25, 0.25]);

        ring.push(&[1.0, 1.0]);
        ring.fill(&mut buf, 2, 0.0);
        assert_eq!(buf, vec![0.0, 0.0]);
    }

    #[test]
    fn stereo_expands_into_a_surround_buffer() {
        let ring = Ring::new(64);
        ring.push(&[0.3, 0.7]);
        let mut buf = vec![9.0; 6]; // one 6-channel frame
        assert_eq!(ring.fill(&mut buf, 6, 1.0), 1);
        assert_eq!(buf, vec![0.3, 0.7, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn clear_drops_pending_audio_but_keeps_the_played_count() {
        let ring = Ring::new(64);
        ring.push(&[1.0, 1.0]);
        let mut buf = vec![0.0; 2];
        ring.fill(&mut buf, 2, 1.0);
        ring.push(&[1.0, 1.0, 1.0, 1.0]);
        ring.clear();
        assert!(ring.is_empty());
        // Seeking rebases against this counter, so it must not rewind.
        assert_eq!(ring.frames_played(), 1);
    }
}
