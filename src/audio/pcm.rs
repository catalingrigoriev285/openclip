//! Uncompressed 16-bit PCM "encoder" (AVI only).

use anyhow::Result;

use super::encoder::{AudioCodecConfig, AudioEncoder, AudioFrame};

pub struct PcmEncoder {
    rate: u32,
    channels: u16,
    /// Sample frames per emitted chunk (20 ms).
    chunk: usize,
    pending: Vec<i16>,
}

impl PcmEncoder {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self { rate: sample_rate, channels: channels.clamp(1, 2), chunk: (sample_rate / 50).max(1) as usize, pending: Vec::new() }
    }

    fn take_frames(&mut self, all: bool) -> Vec<AudioFrame> {
        let ch = self.channels as usize;
        let per_chunk = self.chunk * ch;
        let mut frames = Vec::new();
        while self.pending.len() >= per_chunk {
            let rest = self.pending.split_off(per_chunk);
            let chunk = std::mem::replace(&mut self.pending, rest);
            frames.push(AudioFrame { data: to_bytes(&chunk), samples: self.chunk as u32 });
        }
        if all && !self.pending.is_empty() {
            let samples = (self.pending.len() / ch) as u32;
            let chunk = std::mem::take(&mut self.pending);
            frames.push(AudioFrame { data: to_bytes(&chunk), samples });
        }
        frames
    }
}

fn to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl AudioEncoder for PcmEncoder {
    fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<AudioFrame>> {
        self.pending.extend(interleaved.iter().map(|&v| f32_to_i16(v)));
        Ok(self.take_frames(false))
    }

    fn flush(&mut self) -> Result<Vec<AudioFrame>> {
        Ok(self.take_frames(true))
    }

    fn samples_per_frame(&self) -> u32 {
        self.chunk as u32
    }

    fn bitrate_bps(&self) -> u32 {
        self.rate * self.channels as u32 * 16
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn codec_config(&self) -> AudioCodecConfig {
        AudioCodecConfig::Pcm { bits: 16 }
    }

    fn describe(&self) -> String {
        format!("PCM 16-bit {} Hz {}ch", self.rate, self.channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_and_flushes() {
        let mut e = PcmEncoder::new(48_000, 2);
        assert_eq!(e.samples_per_frame(), 960);
        let pcm = vec![0.5f32; 960 * 2 + 10];
        let frames = e.encode(&pcm).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples, 960);
        assert_eq!(frames[0].data.len(), 960 * 4);
        assert_eq!(&frames[0].data[..2], &16384i16.to_le_bytes());
        let rest = e.flush().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].samples, 5);
    }
}
