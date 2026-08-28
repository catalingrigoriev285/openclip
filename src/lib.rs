//! openclip — a cross-platform screen recorder with no external runtime
//! dependencies. Video is H.264 (bundled OpenH264, or hardware / software
//! Media Foundation encoders on Windows, which also provide HEVC), audio is
//! MP3 (bundled LAME), AAC (Media Foundation) or PCM, muxed into MP4 or AVI
//! by in-house writers.

/// Asks hybrid-graphics laptops (NVIDIA Optimus / AMD PowerXpress) to run the
/// process on the discrete GPU. Without this the vendor's hardware encoder
/// transform refuses to activate on the integrated GPU. The symbols are
/// exported from the executables by `build.rs`.
#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
pub static NvOptimusEnablement: u32 = 1;

#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
pub static AmdPowerXpressRequestHighPerformance: u32 = 1;

pub mod audio;
pub mod capture;
pub mod i18n;
pub mod mux;
pub mod pipeline;
pub mod settings;
pub mod ui;
pub mod update;
pub mod video;
