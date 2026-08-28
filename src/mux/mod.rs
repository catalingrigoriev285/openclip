//! Container writers: a non-fragmented MP4 (ISO BMFF) muxer and an OpenDML
//! AVI muxer, both in-house, plus H.264 / HEVC bitstream helpers.

pub mod avc;
pub mod avi;
pub mod boxes;
pub mod hevc;
pub mod mp4;
pub mod muxer;

pub use avi::AviWriter;
pub use mp4::{AudioTrackConfig, Mp4Writer, VideoCodecConfig, VideoTrackConfig, VIDEO_TIMESCALE};
pub use muxer::Muxer;
