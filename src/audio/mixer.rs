//! Places captured audio on the recording timeline and mixes sources into a
//! single stereo stream at the master sample rate.
//!
//! Each source's chunks are mapped to timeline positions using their arrival
//! time; gaps (e.g. WASAPI loopback delivering nothing while the system is
//! silent) are filled with silence so audio never drifts against video.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::capture::SharedQueue;
use super::resample::LinearResampler;

/// If a source's sample count lags its wall-clock position by more than this,
/// the gap is filled with silence.
const GAP_THRESHOLD: Duration = Duration::from_millis(60);

struct SourceState {
    queue: SharedQueue,
    channels: usize,
    resampler: LinearResampler,
    /// Stereo interleaved samples at the master rate, starting at `buffer_start`.
    buffer: VecDeque<f32>,
    /// Timeline frame index of `buffer[0]`.
    buffer_start: i64,
    /// Timeline frame index where the next incoming sample lands.
    next_pos: Option<i64>,
    scratch: Vec<f32>,
    stereo: Vec<f32>,
    gain: f32,
}

pub struct Mixer {
    rate: u32,
    epoch: Instant,
    /// Timeline origin (first video frame) relative to `epoch`.
    origin: Duration,
    sources: Vec<SourceState>,
    /// Next output frame index.
    out_pos: i64,
    pub silence_inserted: u64,
}

impl Mixer {
    pub fn new(rate: u32, epoch: Instant, origin: Duration) -> Self {
        Self { rate, epoch, origin, sources: Vec::new(), out_pos: 0, silence_inserted: 0 }
    }

    pub fn add_source(&mut self, queue: SharedQueue, sample_rate: u32, channels: u16, gain: f32) {
        self.sources.push(SourceState {
            queue,
            channels: channels.max(1) as usize,
            resampler: LinearResampler::new(sample_rate, self.rate, 2),
            buffer: VecDeque::new(),
            buffer_start: 0,
            next_pos: None,
            scratch: Vec::new(),
            stereo: Vec::new(),
            gain,
        });
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Output frames produced so far.
    pub fn position(&self) -> i64 {
        self.out_pos
    }

    fn timeline_frames(&self, at: Instant) -> i64 {
        let t = at.duration_since(self.epoch).as_secs_f64() - self.origin.as_secs_f64();
        (t * self.rate as f64).round() as i64
    }

    /// Pulls queued chunks from every source onto the timeline.
    fn ingest(&mut self) {
        let rate = self.rate as f64;
        let gap_frames = (GAP_THRESHOLD.as_secs_f64() * rate) as i64;
        for src in &mut self.sources {
            let chunks: Vec<_> = {
                let mut q = src.queue.lock().unwrap();
                q.chunks.drain(..).collect()
            };
            for chunk in chunks {
                let frames_in = chunk.data.len() / src.channels;
                if frames_in == 0 {
                    continue;
                }
                // The chunk was captured *before* it arrived: place its end at the arrival time.
                let arrival = {
                    let t = chunk.at.duration_since(self.epoch).as_secs_f64() - self.origin.as_secs_f64();
                    (t * rate).round() as i64
                };
                let expected_start = arrival - (frames_in as f64 * rate / src.resampler_in_rate()).round() as i64;
                let next = match src.next_pos {
                    None => {
                        src.buffer_start = expected_start;
                        expected_start
                    }
                    Some(n) if expected_start - n > gap_frames => {
                        // Source stalled (e.g. loopback silence): pad with silence.
                        let pad = (expected_start - n) as usize;
                        src.buffer.extend(std::iter::repeat_n(0.0, pad * 2));
                        self.silence_inserted += pad as u64;
                        expected_start
                    }
                    Some(n) => n,
                };
                // Downmix / upmix to stereo.
                src.stereo.clear();
                src.stereo.reserve(frames_in * 2);
                for f in 0..frames_in {
                    let base = f * src.channels;
                    let l = chunk.data[base] * src.gain;
                    let r = if src.channels > 1 { chunk.data[base + 1] * src.gain } else { l };
                    src.stereo.push(l);
                    src.stereo.push(r);
                }
                src.scratch.clear();
                src.resampler.process(&src.stereo, &mut src.scratch);
                let frames_out = src.scratch.len() / 2;
                src.buffer.extend(src.scratch.iter().copied());
                src.next_pos = Some(next + frames_out as i64);
            }
        }
    }

    /// Mixes all sources up to timeline frame `target`, appending interleaved
    /// stereo samples to `out`.
    pub fn mix_until(&mut self, target_time: Instant, out: &mut Vec<f32>) {
        self.ingest();
        let target = self.timeline_frames(target_time);
        if target <= self.out_pos {
            return;
        }
        let n = (target - self.out_pos) as usize;
        let start = out.len();
        out.resize(start + n * 2, 0.0);
        let dst = &mut out[start..];
        for src in &mut self.sources {
            // Discard anything older than the output position.
            while src.buffer_start < self.out_pos && !src.buffer.is_empty() {
                src.buffer.pop_front();
                src.buffer.pop_front();
                src.buffer_start += 1;
            }
            if src.buffer.is_empty() {
                src.buffer_start = src.buffer_start.max(self.out_pos);
            }
            let offset = (src.buffer_start - self.out_pos).max(0) as usize; // frames of silence first
            let avail = src.buffer.len() / 2;
            let take = avail.min(n.saturating_sub(offset));
            for i in 0..take {
                let d = (offset + i) * 2;
                dst[d] += src.buffer[i * 2];
                dst[d + 1] += src.buffer[i * 2 + 1];
            }
            src.buffer.drain(..take * 2);
            src.buffer_start += take as i64;
        }
        for s in dst.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        self.out_pos = target;
    }
}

impl SourceState {
    fn resampler_in_rate(&self) -> f64 {
        self.resampler.input_rate() as f64
    }
}
