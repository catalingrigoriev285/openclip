//! Audio pipeline: capture (cpal), mixing, resampling and MP3 encoding.

pub mod capture;
pub mod mixer;
pub mod mp3;
pub mod resample;

pub use mp3::{Mp3Encoder, Mp3Frame};
