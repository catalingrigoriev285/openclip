//! Video pipeline: raw frame types, pixel conversion, H.264 encoding, preview.

pub mod convert;
pub mod encoder;
pub mod mouse_fx;
pub mod preview;

pub use convert::{Converter, PixelFormat, RawFrame};
pub use encoder::{EncodedFrame, H264Encoder};
