//! OpenH264 wrapper producing AVCC-formatted access units for the muxer.

use std::time::Duration;

use anyhow::{Context, Result};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};

use crate::mux::avc;

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// NAL units with 4-byte length prefixes (SPS/PPS removed).
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: Duration,
}

pub struct H264Encoder {
    inner: Encoder,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    frames: u64,
}

impl H264Encoder {
    /// Creates an encoder tuned for real-time screen content.
    pub fn new(fps: f32, bitrate_bps: u32) -> Result<Self> {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(16) as u16)
            .unwrap_or(1);
        let keyint = (fps.max(1.0) * 2.0).round() as u32;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(fps))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .intra_frame_period(IntraFramePeriod::from_num_frames(keyint))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // Required for bitrate mode to actually cap the bitrate; skipped
            // frames simply extend the previous sample (durations are real).
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .num_threads(threads);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("failed to create OpenH264 encoder")?;
        Ok(Self { inner, sps: None, pps: None, frames: 0 })
    }

    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    pub fn pps(&self) -> Option<&[u8]> {
        self.pps.as_deref()
    }

    pub fn frames_encoded(&self) -> u64 {
        self.frames
    }

    /// Encodes one frame. Returns `None` if the encoder skipped it.
    pub fn encode<S: YUVSource>(&mut self, yuv: &S, pts: Duration) -> Result<Option<EncodedFrame>> {
        let ts = Timestamp::from_millis(pts.as_millis() as u64);
        let bs = self.inner.encode_at(yuv, ts).context("OpenH264 encode failed")?;
        let frame_type = bs.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let mut data = Vec::new();
        for l in 0..bs.num_layers() {
            let layer = bs.layer(l).unwrap();
            for n in 0..layer.nal_count() {
                let raw = layer.nal_unit(n).unwrap();
                debug_assert!(
                    raw.starts_with(&[0, 0, 0, 1]) || raw.starts_with(&[0, 0, 1]),
                    "OpenH264 NAL without Annex-B start code"
                );
                // A NAL slot may hold several concatenated NALs; split defensively.
                for nal in avc::split_annexb(raw) {
                    match avc::nal_type(nal) {
                        avc::NAL_SPS => {
                            if self.sps.is_none() {
                                self.sps = Some(nal.to_vec());
                            }
                        }
                        avc::NAL_PPS => {
                            if self.pps.is_none() {
                                self.pps = Some(nal.to_vec());
                            }
                        }
                        _ => avc::push_avcc(&mut data, nal),
                    }
                }
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        self.frames += 1;
        Ok(Some(EncodedFrame {
            data,
            keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
            pts,
        }))
    }

    /// Requests that the next frame be an IDR frame.
    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }
}
