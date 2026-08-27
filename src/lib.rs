//! openclip — a cross-platform screen recorder with no external runtime
//! dependencies. Video is H.264 (bundled OpenH264), audio is MP3 (bundled
//! LAME), muxed into MP4 by an in-house writer.

pub mod audio;
pub mod capture;
pub mod mux;
pub mod video;
