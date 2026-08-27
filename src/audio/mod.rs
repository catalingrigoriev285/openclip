//! Audio pipeline: capture (cpal), mixing, resampling and encoding (MP3 via
//! bundled LAME, PCM, and AAC through Media Foundation on Windows).

pub mod capture;
pub mod encoder;
#[cfg(windows)]
pub mod mf_aac;
pub mod mixer;
pub mod mp3;
pub mod pcm;
pub mod resample;

pub use encoder::{create_audio_encoder, AudioCodecConfig, AudioEncoder, AudioFrame};
pub use mp3::{Mp3Encoder, Mp3Frame};
pub use pcm::PcmEncoder;
