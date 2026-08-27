//! Video pipeline: raw frame types, pixel conversion, scaling, encoders, preview.

pub mod convert;
pub mod encoder;
#[cfg(windows)]
pub mod mf;
pub mod mouse_fx;
pub mod openh264;
pub mod preview;
pub mod scale;

pub use convert::{Converter, PixelFormat, RawFrame};
pub use encoder::{
    available_encoders, create_video_encoder, refresh_encoders, CodecParams, EncodedFrame, EncoderInfo,
    EncoderRequest, FrameInput, InputLayout, Vendor, VideoEncoder,
};
pub use openh264::H264Encoder;
pub use scale::Scaler;
