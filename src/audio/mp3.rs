//! MP3 encoding via bundled LAME, with a frame splitter so that each MP4
//! sample holds exactly one MP3 frame.

use anyhow::{anyhow, Context, Result};
use mp3lame_encoder::{Bitrate, Builder, Encoder, FlushNoGap, InterleavedPcm, Mode, Quality};

use super::encoder::{AudioCodecConfig, AudioEncoder, AudioFrame};

/// Samples per frame for MPEG-1 Layer III (32/44.1/48 kHz).
pub const MPEG1_SAMPLES_PER_FRAME: u32 = 1152;
/// LAME encoder delay (576) + standard decoder delay (529): the first samples
/// the decoder emits do not correspond to input. We drop this many input
/// samples so the decoded stream lines up with the video timeline.
pub const CODEC_DELAY_SAMPLES: usize = 1105;

/// One MP3 frame (kept as an alias of the generic audio frame).
pub type Mp3Frame = AudioFrame;

pub struct Mp3Encoder {
    inner: Encoder,
    sample_rate: u32,
    channels: u8,
    bitrate_kbps: u32,
    pending: Vec<u8>,
    out: Vec<u8>,
    skip_samples: usize,
    frames_emitted: u64,
}

impl Mp3Encoder {
    /// `channels` must be 1 or 2; `sample_rate` should be 32000/44100/48000.
    pub fn new(sample_rate: u32, channels: u8, bitrate_kbps: u32) -> Result<Self> {
        let bitrate = match bitrate_kbps {
            0..=64 => Bitrate::Kbps64,
            65..=96 => Bitrate::Kbps96,
            97..=128 => Bitrate::Kbps128,
            129..=160 => Bitrate::Kbps160,
            161..=192 => Bitrate::Kbps192,
            193..=256 => Bitrate::Kbps256,
            _ => Bitrate::Kbps320,
        };
        let mut b = Builder::new().context("LAME init failed")?;
        b.set_sample_rate(sample_rate).map_err(|e| anyhow!("LAME sample rate: {e:?}"))?;
        b.set_num_channels(channels).map_err(|e| anyhow!("LAME channels: {e:?}"))?;
        b.set_brate(bitrate).map_err(|e| anyhow!("LAME bitrate: {e:?}"))?;
        b.set_mode(if channels == 1 { Mode::Mono } else { Mode::JointStereo })
            .map_err(|e| anyhow!("LAME mode: {e:?}"))?;
        b.set_quality(Quality::Good).map_err(|e| anyhow!("LAME quality: {e:?}"))?;
        b.set_to_write_vbr_tag(false).map_err(|e| anyhow!("LAME vbr tag: {e:?}"))?;
        let inner = b.build().map_err(|e| anyhow!("LAME build: {e:?}"))?;
        Ok(Self {
            inner,
            sample_rate,
            channels,
            bitrate_kbps: bitrate as u32,
            pending: Vec::new(),
            out: Vec::new(),
            skip_samples: CODEC_DELAY_SAMPLES,
            frames_emitted: 0,
        })
    }

    /// Encodes interleaved f32 PCM (in -1..1). Returns complete frames.
    pub fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<Mp3Frame>> {
        let ch = self.channels as usize;
        let mut input = interleaved;
        if self.skip_samples > 0 {
            let skip = (self.skip_samples * ch).min(input.len());
            input = &input[skip..];
            self.skip_samples -= skip / ch;
        }
        if input.is_empty() {
            return Ok(Vec::new());
        }
        // LAME writes into the Vec's spare capacity and treats a zero-sized
        // buffer as "unchecked", so reserve its documented worst case first.
        self.out.clear();
        self.out.reserve(input.len() / ch * 5 / 4 + 7200);
        self.inner
            .encode_to_vec(InterleavedPcm(input), &mut self.out)
            .map_err(|e| anyhow!("LAME encode: {e:?}"))?;
        self.pending.extend_from_slice(&self.out);
        Ok(self.drain_frames(false))
    }

    /// Flushes the encoder and returns the final frames.
    pub fn flush(&mut self) -> Result<Vec<Mp3Frame>> {
        self.out.clear();
        self.out.reserve(16 * 1024);
        self.inner
            .flush_to_vec::<FlushNoGap>(&mut self.out)
            .map_err(|e| anyhow!("LAME flush: {e:?}"))?;
        self.pending.extend_from_slice(&self.out);
        Ok(self.drain_frames(true))
    }

    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    fn drain_frames(&mut self, final_flush: bool) -> Vec<Mp3Frame> {
        let mut frames = Vec::new();
        let mut pos = 0;
        while pos + 4 <= self.pending.len() {
            match parse_frame_header(&self.pending[pos..]) {
                Some(hdr) => {
                    if pos + hdr.frame_len > self.pending.len() {
                        break;
                    }
                    frames.push(Mp3Frame {
                        data: self.pending[pos..pos + hdr.frame_len].to_vec(),
                        samples: hdr.samples,
                    });
                    self.frames_emitted += 1;
                    pos += hdr.frame_len;
                }
                None => {
                    // Not a sync word (e.g. stray bytes); resync.
                    log::warn!("MP3 splitter: resyncing at byte {pos}");
                    pos += 1;
                }
            }
        }
        if final_flush {
            self.pending.clear();
        } else {
            self.pending.drain(..pos);
        }
        frames
    }
}

impl AudioEncoder for Mp3Encoder {
    fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<AudioFrame>> {
        Mp3Encoder::encode(self, interleaved)
    }

    fn flush(&mut self) -> Result<Vec<AudioFrame>> {
        Mp3Encoder::flush(self)
    }

    fn samples_per_frame(&self) -> u32 {
        MPEG1_SAMPLES_PER_FRAME
    }

    fn bitrate_bps(&self) -> u32 {
        self.bitrate_kbps * 1000
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels as u16
    }

    fn codec_config(&self) -> AudioCodecConfig {
        AudioCodecConfig::Mp3
    }

    fn describe(&self) -> String {
        format!("MP3 (LAME) {} Hz {}ch {} kbps", self.sample_rate, self.channels, self.bitrate_kbps)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_len: usize,
    pub samples: u32,
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}

/// Parses an MPEG audio Layer III frame header at the start of `b`.
pub fn parse_frame_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < 4 || b[0] != 0xFF || (b[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 0x3; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (b[1] >> 1) & 0x3; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_idx = (b[2] >> 4) as usize;
    let rate_idx = ((b[2] >> 2) & 0x3) as usize;
    let padding = ((b[2] >> 1) & 0x1) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 || rate_idx == 3 {
        return None;
    }
    const BR_V1: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    const BR_V2: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0];
    let (bitrate_kbps, sample_rate, samples) = match version {
        3 => (BR_V1[bitrate_idx], [44100, 48000, 32000][rate_idx], 1152),
        2 => (BR_V2[bitrate_idx], [22050, 24000, 16000][rate_idx], 576),
        _ => (BR_V2[bitrate_idx], [11025, 12000, 8000][rate_idx], 576),
    };
    let frame_len =
        samples as usize / 8 * bitrate_kbps as usize * 1000 / sample_rate as usize + padding;
    Some(FrameHeader { frame_len, samples, sample_rate, bitrate_kbps })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mpeg1_header() {
        // MPEG1 Layer III, 160 kbps, 48 kHz, no padding, stereo.
        let h = parse_frame_header(&[0xFF, 0xFB, 0xA4, 0x00]).unwrap();
        assert_eq!(h.sample_rate, 48000);
        assert_eq!(h.bitrate_kbps, 160);
        assert_eq!(h.samples, 1152);
        assert_eq!(h.frame_len, 480);
    }

    #[test]
    fn encodes_tone_into_whole_frames() {
        let rate = 48000;
        let mut enc = Mp3Encoder::new(rate, 2, 160).unwrap();
        let mut pcm = Vec::new();
        for i in 0..rate {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
            pcm.push(v);
            pcm.push(v);
        }
        let mut frames = Vec::new();
        for chunk in pcm.chunks(4096) {
            frames.extend(enc.encode(chunk).unwrap());
        }
        frames.extend(enc.flush().unwrap());
        assert!(frames.len() >= 40, "got {} frames", frames.len());
        for f in &frames {
            let h = parse_frame_header(&f.data).expect("frame starts with header");
            assert_eq!(h.frame_len, f.data.len());
            assert_eq!(f.samples, 1152);
        }
    }
}
