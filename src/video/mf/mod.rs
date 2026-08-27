//! Windows Media Foundation encoders (hardware H.264 / HEVC, Microsoft's
//! software H.264, and AAC). Placeholder until the transforms are wired up.

use anyhow::{bail, Result};

use super::encoder::{EncoderInfo, EncoderRequest, VideoEncoder};

pub fn available_encoders() -> Vec<EncoderInfo> {
    Vec::new()
}

pub fn refresh_encoders() -> Vec<EncoderInfo> {
    Vec::new()
}

pub fn create_encoder(req: &EncoderRequest) -> Result<Box<dyn VideoEncoder>> {
    bail!("{} is not available yet", req.codec.generic_label())
}
