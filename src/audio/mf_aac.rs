//! AAC-LC through the Windows Media Foundation encoder. Placeholder until the
//! transform is wired up.

use anyhow::{bail, Result};

use super::encoder::{AudioCodecConfig, AudioEncoder, AudioFrame};

pub struct AacEncoder {
    rate: u32,
    channels: u16,
}

impl AacEncoder {
    pub fn new(_sample_rate: u32, _channels: u16, _bitrate_kbps: u32) -> Result<Self> {
        bail!("AAC encoder is not available yet")
    }
}

impl AudioEncoder for AacEncoder {
    fn encode(&mut self, _interleaved: &[f32]) -> Result<Vec<AudioFrame>> {
        Ok(Vec::new())
    }

    fn flush(&mut self) -> Result<Vec<AudioFrame>> {
        Ok(Vec::new())
    }

    fn samples_per_frame(&self) -> u32 {
        1024
    }

    fn bitrate_bps(&self) -> u32 {
        0
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn codec_config(&self) -> AudioCodecConfig {
        AudioCodecConfig::Aac { asc: Vec::new() }
    }

    fn describe(&self) -> String {
        "AAC".into()
    }
}
