//! Video encoder abstraction: OpenH264 (bundled, CPU) and, on Windows, Media
//! Foundation transforms (hardware H.264 / HEVC and software encoders).
//!
//! Encoders produce Annex-B access units (start codes, parameter-set NALs kept
//! in-band on keyframes). Container writers convert as they need: the MP4 muxer
//! length-prefixes and strips parameter sets, the AVI muxer stores them verbatim.

use std::time::Duration;

use anyhow::Result;

use crate::mux::{avc, hevc};
use crate::settings::{Profiles, RateControl, VideoCodec};

/// One encoded access unit in Annex-B form.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: Duration,
}

/// Start-code-free parameter sets needed for the MP4 sample entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecParams {
    H264 { sps: Vec<u8>, pps: Vec<u8> },
    Hevc { vps: Vec<u8>, sps: Vec<u8>, pps: Vec<u8> },
}

impl CodecParams {
    /// Harvests parameter sets from an Annex-B access unit, if all are present.
    pub fn from_annexb(data: &[u8], is_hevc: bool) -> Option<CodecParams> {
        let nals = avc::split_annexb(data);
        if is_hevc {
            let find = |t: u8| nals.iter().find(|n| hevc::nal_type(n) == t).map(|n| n.to_vec());
            Some(CodecParams::Hevc { vps: find(hevc::NAL_VPS)?, sps: find(hevc::NAL_SPS)?, pps: find(hevc::NAL_PPS)? })
        } else {
            let find = |t: u8| nals.iter().find(|n| avc::nal_type(n) == t).map(|n| n.to_vec());
            Some(CodecParams::H264 { sps: find(avc::NAL_SPS)?, pps: find(avc::NAL_PPS)? })
        }
    }

    pub fn is_hevc(&self) -> bool {
        matches!(self, CodecParams::Hevc { .. })
    }
}

/// Pixel layout an encoder wants its input in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLayout {
    I420,
    Nv12,
}

/// One converted frame, borrowed from the [`crate::video::Converter`].
#[derive(Debug, Clone, Copy)]
pub enum FrameInput<'a> {
    I420 { y: &'a [u8], u: &'a [u8], v: &'a [u8], strides: (usize, usize, usize), dims: (u32, u32) },
    Nv12 { y: &'a [u8], uv: &'a [u8], strides: (usize, usize), dims: (u32, u32) },
}

impl FrameInput<'_> {
    pub fn dims(&self) -> (u32, u32) {
        match self {
            FrameInput::I420 { dims, .. } | FrameInput::Nv12 { dims, .. } => *dims,
        }
    }
}

/// Encoders are created and used on one thread (the encode thread): Media
/// Foundation objects are not `Send`.
pub trait VideoEncoder {
    fn input_layout(&self) -> InputLayout;
    /// Whether the output bitstream is HEVC (else H.264).
    fn is_hevc(&self) -> bool;
    /// Encodes one frame. Returns nothing when the encoder skipped the frame
    /// (rate control) or is still buffering (asynchronous hardware encoders),
    /// and possibly several frames when a backlog is released.
    fn encode(&mut self, frame: FrameInput<'_>, pts: Duration) -> Result<Vec<EncodedFrame>>;
    /// Drains everything still buffered; called once when recording stops.
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;
    /// Parameter sets, available once the first keyframe has been produced.
    fn codec_params(&self) -> Option<&CodecParams>;
    fn force_keyframe(&mut self);
    /// Human-readable description for logs and the status strip.
    fn describe(&self) -> String;
}

/// Everything an encoder needs to be configured, resolved from the settings
/// and the measured output size.
#[derive(Debug, Clone)]
pub struct EncoderRequest {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate_control: RateControl,
    /// Target bitrate in bits per second (derived from the quality when in quality mode).
    pub target_bitrate_bps: u32,
    pub keyframe_interval_frames: u32,
    pub profiles: Profiles,
}

impl EncoderRequest {
    /// Plain OpenH264 request at a fixed bitrate, for tests and examples.
    pub fn simple(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        Self {
            codec: VideoCodec::OpenH264,
            width,
            height,
            fps,
            rate_control: RateControl::ConstantBitrate { kbps: bitrate_bps / 1000 },
            target_bitrate_bps: bitrate_bps,
            keyframe_interval_frames: fps.max(1) * 2,
            profiles: Profiles::default(),
        }
    }
}

/// GPU / codec vendor of an enumerated encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Microsoft,
    Other,
}

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Microsoft => "Microsoft",
            Vendor::Other => "Other",
        }
    }
}

/// An encoder found on this machine (Media Foundation on Windows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub hevc: bool,
    /// Short user-facing label, e.g. "H264 (NVIDIA® NVENC)".
    pub label: String,
    /// The transform's own name, e.g. "NVIDIA H.264 Encoder MFT".
    pub friendly_name: String,
    pub vendor: Vendor,
    pub hardware: bool,
    /// Stable identity: the transform CLSID as 32 lowercase hex digits, or
    /// `name:<hardware url>` for transforms registered without a CLSID.
    pub clsid: String,
}

impl EncoderInfo {
    pub fn codec(&self) -> VideoCodec {
        VideoCodec::Mf { hevc: self.hevc, clsid: self.clsid.clone() }
    }
}

/// Encoders available on this machine besides the bundled OpenH264. Empty on
/// platforms without Media Foundation. Enumeration can take a few hundred
/// milliseconds the first time; the result is cached.
pub fn available_encoders() -> Vec<EncoderInfo> {
    #[cfg(windows)]
    {
        super::mf::available_encoders()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Re-runs the enumeration (e.g. after a driver change) and updates the cache.
pub fn refresh_encoders() -> Vec<EncoderInfo> {
    #[cfg(windows)]
    {
        super::mf::refresh_encoders()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Creates the requested encoder. When a Media Foundation encoder is missing
/// or refuses the configuration, other encoders of the same family are tried
/// (hardware first), then OpenH264. The note explains any substitution.
pub fn create_video_encoder(req: &EncoderRequest) -> Result<(Box<dyn VideoEncoder>, Option<String>)> {
    let VideoCodec::Mf { hevc, clsid } = &req.codec else {
        return Ok((Box::new(super::openh264::H264Encoder::new(req)?), None));
    };
    let wanted = req.codec.family();
    let mut failures: Vec<String> = Vec::new();
    #[cfg(windows)]
    {
        let list = super::mf::available_encoders();
        let requested = list.iter().find(|e| &e.clsid == clsid);
        let mut candidates: Vec<&EncoderInfo> = requested.into_iter().collect();
        let mut others: Vec<&EncoderInfo> = list.iter().filter(|e| e.hevc == *hevc && &e.clsid != clsid).collect();
        others.sort_by_key(|e| !e.hardware);
        candidates.extend(others);
        if requested.is_none() {
            failures.push(format!("the selected {wanted} encoder was not found"));
        }
        for info in candidates {
            match super::mf::video::MfVideoEncoder::new(info, req) {
                Ok(enc) => {
                    let note = (Some(&info.clsid) != Some(clsid)).then(|| {
                        format!("{}; using {}", failures.join("; "), info.label)
                    });
                    return Ok((Box::new(enc), note));
                }
                Err(e) => {
                    log::warn!("{} failed: {e:#}", info.label);
                    failures.push(format!("{} failed ({e})", info.label));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hevc, clsid);
        failures.push(format!("{wanted} hardware encoding needs Windows"));
    }
    let enc = super::openh264::H264Encoder::new(req)?;
    let note = format!("{}; recorded H.264 with OpenH264", failures.join("; "));
    Ok((Box::new(enc), Some(note)))
}
