//! Minimal non-fragmented MP4 (ISO BMFF) muxer for H.264 video + MP3 audio.

pub mod avc;
pub mod boxes;
pub mod mp4;

pub use mp4::{AudioTrackConfig, Mp4Writer, VideoTrackConfig, VIDEO_TIMESCALE};
