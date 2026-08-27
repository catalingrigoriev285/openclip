//! Audio encoder abstraction: MP3 (bundled LAME), PCM, and on Windows AAC via
//! the Media Foundation encoder.

use anyhow::Result;

use crate::settings::AudioCodec;

/// One encoded audio frame (an MP3 frame, an AAC access unit, or a PCM chunk).
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub data: Vec<u8>,
    /// PCM sample frames (per channel) represented by this frame.
    pub samples: u32,
}

/// Codec-specific information the container needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodecConfig {
    Mp3,
    /// AAC-LC with its AudioSpecificConfig (2 bytes without SBR).
    Aac { asc: Vec<u8> },
    /// Little-endian signed integer PCM.
    Pcm { bits: u16 },
}

pub trait AudioEncoder: Send {
    /// Encodes interleaved f32 PCM in -1..1 at the encoder's rate/channels.
    fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<AudioFrame>>;
    fn flush(&mut self) -> Result<Vec<AudioFrame>>;
    /// PCM frames per encoded frame (1152 MP3, 1024 AAC, chunk size for PCM).
    fn samples_per_frame(&self) -> u32;
    fn bitrate_bps(&self) -> u32;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn codec_config(&self) -> AudioCodecConfig;
    fn describe(&self) -> String;
}

/// Creates the requested encoder; AAC falls back to MP3 when Media Foundation
/// is unavailable, with a note explaining it.
pub fn create_audio_encoder(
    codec: AudioCodec,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
) -> Result<(Box<dyn AudioEncoder>, Option<String>)> {
    let channels = channels.clamp(1, 2);
    match codec {
        AudioCodec::Mp3 => Ok((Box::new(super::mp3::Mp3Encoder::new(sample_rate, channels as u8, bitrate_kbps)?), None)),
        AudioCodec::Pcm => Ok((Box::new(super::pcm::PcmEncoder::new(sample_rate, channels)), None)),
        AudioCodec::Aac => {
            #[cfg(windows)]
            {
                match super::mf_aac::AacEncoder::new(sample_rate, channels, bitrate_kbps) {
                    Ok(enc) => return Ok((Box::new(enc), None)),
                    Err(e) => {
                        log::warn!("AAC encoder unavailable: {e:#}; falling back to MP3");
                        let enc = super::mp3::Mp3Encoder::new(sample_rate, channels as u8, bitrate_kbps)?;
                        return Ok((Box::new(enc), Some(format!("AAC unavailable ({e}); recorded MP3"))));
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let enc = super::mp3::Mp3Encoder::new(sample_rate, channels as u8, bitrate_kbps)?;
                Ok((Box::new(enc), Some("AAC needs Windows; recorded MP3".into())))
            }
        }
    }
}
