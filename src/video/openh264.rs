//! Bundled OpenH264 encoder (CPU, all platforms) behind [`VideoEncoder`].

use std::time::Duration;

use anyhow::{bail, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};

use super::encoder::{CodecParams, EncodedFrame, EncoderRequest, FrameInput, InputLayout, VideoEncoder};
use crate::settings::{H264Profile, RateControl};

pub struct H264Encoder {
    inner: Encoder,
    params: Option<CodecParams>,
    frames: u64,
    description: String,
}

impl H264Encoder {
    /// Creates an encoder tuned for real-time screen content.
    pub fn new(req: &EncoderRequest) -> Result<Self> {
        // More threads make OpenH264 *slower* on hybrid (P/E core) CPUs and
        // starve the GUI and capture threads; four is the sweet spot measured
        // on a 16-thread laptop. `OPENCLIP_OPENH264_THREADS` overrides for testing.
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        let threads = std::env::var("OPENCLIP_OPENH264_THREADS")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or((cores / 4).clamp(2, 4) as u16);
        let fps = req.fps.max(1) as f32;
        let rc = match req.rate_control {
            RateControl::Quality(_) => RateControlMode::Quality,
            RateControl::ConstantBitrate { .. } => RateControlMode::Bitrate,
        };
        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(req.target_bitrate_bps.max(200_000)))
            .max_frame_rate(FrameRate::from_hz(fps))
            .rate_control_mode(rc)
            .usage_type(UsageType::ScreenContentRealTime)
            .intra_frame_period(IntraFramePeriod::from_num_frames(req.keyframe_interval_frames.max(1)))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // Required for rate control to actually cap the bitrate; skipped
            // frames simply extend the previous sample (durations are real).
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .complexity(Complexity::Low)
            .num_threads(threads);
        let profile = match req.profiles.h264 {
            H264Profile::Auto => None,
            H264Profile::Baseline => Some(Profile::Baseline),
            H264Profile::Main => Some(Profile::Main),
            H264Profile::High => Some(Profile::High),
        };
        if let Some(p) = profile {
            config = config.profile(p);
        }
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("failed to create OpenH264 encoder")?;
        let description = format!(
            "H264 (OpenH264, CPU, {threads} threads) {}×{} @ {} fps, {} kbps {}, {} profile",
            req.width,
            req.height,
            req.fps,
            req.target_bitrate_bps / 1000,
            match req.rate_control {
                RateControl::Quality(q) => format!("(quality {q})"),
                RateControl::ConstantBitrate { .. } => "CBR".into(),
            },
            req.profiles.h264.label()
        );
        Ok(Self { inner, params: None, frames: 0, description })
    }

    pub fn frames_encoded(&self) -> u64 {
        self.frames
    }
}

impl VideoEncoder for H264Encoder {
    fn input_layout(&self) -> InputLayout {
        InputLayout::I420
    }

    fn is_hevc(&self) -> bool {
        false
    }

    fn encode(&mut self, frame: FrameInput<'_>, pts: Duration) -> Result<Vec<EncodedFrame>> {
        let FrameInput::I420 { y, u, v, strides, dims } = frame else {
            bail!("OpenH264 needs I420 input");
        };
        let yuv = YUVSlices::new((y, u, v), (dims.0 as usize, dims.1 as usize), strides);
        let ts = Timestamp::from_millis(pts.as_millis() as u64);
        let bs = self.inner.encode_at(&yuv, ts).context("OpenH264 encode failed")?;
        let frame_type = bs.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(Vec::new());
        }
        let mut data = Vec::new();
        for l in 0..bs.num_layers() {
            let layer = bs.layer(l).unwrap();
            for n in 0..layer.nal_count() {
                let raw = layer.nal_unit(n).unwrap();
                // OpenH264 hands out Annex-B NALs (with start codes); a slot may
                // hold several concatenated NALs, which is fine for a byte stream.
                if !(raw.starts_with(&[0, 0, 0, 1]) || raw.starts_with(&[0, 0, 1])) {
                    data.extend_from_slice(&[0, 0, 0, 1]);
                }
                data.extend_from_slice(raw);
            }
        }
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        if self.params.is_none() {
            self.params = CodecParams::from_annexb(&data, false);
        }
        self.frames += 1;
        Ok(vec![EncodedFrame { data, keyframe, pts }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }

    fn codec_params(&self) -> Option<&CodecParams> {
        self.params.as_ref()
    }

    fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}
