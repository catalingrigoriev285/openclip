//! openclip — a cross-platform screen recorder with no external runtime
//! dependencies. Video is H.264 (bundled OpenH264, or hardware / software
//! Media Foundation encoders on Windows, which also provide HEVC), audio is
//! MP3 (bundled LAME), AAC (Media Foundation) or PCM, muxed into MP4 or AVI
//! by in-house writers.

pub mod audio;
pub mod capture;
pub mod mux;
pub mod pipeline;
pub mod settings;
pub mod ui;
pub mod video;
