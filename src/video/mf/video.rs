//! H.264 / HEVC encoding through a Media Foundation transform (hardware or
//! Microsoft's software encoder), fed with NV12 frames from system memory.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MFCreateMediaType, MFMediaType_Video, MFSampleExtension_CleanPoint, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonQuality, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_Base,
    eAVEncH264VProfile_High, eAVEncH264VProfile_Main, eAVEncH265VProfile_Main_420_8,
};

use super::transform::{make_sample_with, sample_bytes, MftSession};
use super::{activate_for, startup, ComGuard};
use crate::mux::{avc, hevc};
use crate::settings::{H264Profile, HevcProfile, RateControl};
use crate::video::encoder::{CodecParams, EncodedFrame, EncoderInfo, EncoderRequest, FrameInput, InputLayout, VideoEncoder};

pub struct MfVideoEncoder {
    session: MftSession,
    params: Option<CodecParams>,
    /// Annex-B parameter sets from `MF_MT_MPEG_SEQUENCE_HEADER`, prepended to
    /// the first keyframe when the encoder does not emit them in-band.
    sequence_header: Option<Vec<u8>>,
    hevc: bool,
    frame_duration: i64,
    dims: (u32, u32),
    frames_in: u64,
    frames_out: u64,
    description: String,
    /// Must be dropped last (declaration order) so COM objects go first.
    _com: ComGuard,
}

fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

impl MfVideoEncoder {
    pub fn new(info: &EncoderInfo, req: &EncoderRequest) -> Result<Self> {
        let com = ComGuard::new();
        startup()?;
        let hevc = info.hevc;
        let activate = activate_for(info)?;
        let mut session = MftSession::from_activate(&activate)?;
        let fps = req.fps.max(1);
        let bitrate = req.target_bitrate_bps.max(200_000);

        let mut used_quality_mode = false;
        let apply_codec_api = |session: &MftSession, note_quality: &mut bool| {
            // Rate control first: some transforms only accept it before the types are set.
            match req.rate_control {
                RateControl::Quality(q) => {
                    if session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_Quality.0 as u32)
                        && session.set_u32(&CODECAPI_AVEncCommonQuality, q.clamp(1, 100) as u32)
                    {
                        *note_quality = true;
                    } else {
                        session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
                    }
                }
                RateControl::ConstantBitrate { .. } => {
                    session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
                }
            }
            session.set_u32(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
            session.set_u32(&CODECAPI_AVEncMPVGOPSize, req.keyframe_interval_frames.max(1));
            // No B-frames: the muxers assume presentation order == decode order.
            session.set_u32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            session.set_bool(&CODECAPI_AVLowLatencyMode, true);
        };
        apply_codec_api(&session, &mut used_quality_mode);

        // Output type (always before the input type for encoders).
        let profile = if hevc {
            match req.profiles.hevc {
                HevcProfile::Auto => None,
                HevcProfile::Main => Some(eAVEncH265VProfile_Main_420_8.0 as u32),
            }
        } else {
            match req.profiles.h264 {
                H264Profile::Auto => None,
                H264Profile::Baseline => Some(eAVEncH264VProfile_Base.0 as u32),
                H264Profile::Main => Some(eAVEncH264VProfile_Main.0 as u32),
                H264Profile::High => Some(eAVEncH264VProfile_High.0 as u32),
            }
        };
        let out_type = output_media_type(hevc, req.width, req.height, fps, bitrate, profile)?;
        session.set_output_type(&out_type).with_context(|| format!("{} rejected the output format", info.label))?;
        apply_codec_api(&session, &mut used_quality_mode);

        // Input type: NV12 at the same size and rate.
        let in_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(req.width, req.height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, req.width)?;
            in_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        }
        if let Err(e) = session.set_input_type(&in_type) {
            // Some transforms are picky: take their own NV12 proposal and adjust it.
            let proposed = session.input_available_types().into_iter().find(|t| {
                unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.map(|g| g == MFVideoFormat_NV12).unwrap_or(false)
            });
            let Some(t) = proposed else {
                return Err(e.context(format!("{} does not accept NV12 input", info.label)));
            };
            unsafe {
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(req.width, req.height))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            }
            session.set_input_type(&t).with_context(|| format!("{} rejected the NV12 input format", info.label))?;
        }
        // Bitrate-related properties again now that both types are known (ignored if fixed).
        session.set_u32(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
        session.set_u32(&CODECAPI_AVEncMPVGOPSize, req.keyframe_interval_frames.max(1));

        // Encoded frames are far smaller than a raw one; use that as the buffer bound.
        let min_out = (req.width * req.height * 3 / 2).max(1 << 20);
        session.start(min_out)?;
        let sequence_header = sequence_header(&session.output_type().ok());

        let rc = match req.rate_control {
            RateControl::Quality(q) if used_quality_mode => format!("quality {q}"),
            RateControl::Quality(q) => format!("quality {q} → {} kbps CBR", bitrate / 1000),
            RateControl::ConstantBitrate { .. } => format!("{} kbps CBR", bitrate / 1000),
        };
        let description = format!(
            "{} via Media Foundation ({}), {}×{} @ {} fps, {rc}, {} profile{}",
            info.label,
            if session.is_async() { "async" } else { "sync" },
            req.width,
            req.height,
            fps,
            if hevc { req.profiles.hevc.label() } else { req.profiles.h264.label() },
            if session.has_codec_api() { "" } else { ", no codec API" }
        );
        log::info!("{description}");
        Ok(Self {
            session,
            params: None,
            sequence_header,
            hevc,
            frame_duration: 10_000_000 / fps as i64,
            dims: (req.width, req.height),
            frames_in: 0,
            frames_out: 0,
            description,
            _com: com,
        })
    }

    fn convert_output(&mut self, sample: &windows::Win32::Media::MediaFoundation::IMFSample) -> Result<Option<EncodedFrame>> {
        let mut data = sample_bytes(sample)?;
        if data.is_empty() {
            return Ok(None);
        }
        let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.map(|v| v == 1).ok();
        let nals = avc::split_annexb(&data);
        let has_params = nals.iter().any(|n| {
            if self.hevc { hevc::is_parameter_set(hevc::nal_type(n)) } else { avc::is_parameter_set(avc::nal_type(n)) }
        });
        let idr = nals.iter().any(|n| {
            if self.hevc { hevc::is_irap(hevc::nal_type(n)) } else { avc::nal_type(n) == avc::NAL_IDR }
        });
        let keyframe = clean.unwrap_or(idr) || idr || has_params;
        if keyframe
            && !has_params
            && let Some(h) = &self.sequence_header
        {
            let mut with = h.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        if self.params.is_none() {
            self.params = CodecParams::from_annexb(&data, self.hevc);
        }
        let time = unsafe { sample.GetSampleTime() }.unwrap_or(0).max(0);
        self.frames_out += 1;
        Ok(Some(EncodedFrame { data, keyframe, pts: Duration::from_nanos(time as u64 * 100) }))
    }
}

/// Builds the compressed output media type for an H.264 / HEVC encoder.
pub(crate) fn output_media_type(
    hevc: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    profile: Option<u32>,
) -> Result<IMFMediaType> {
    let subtype = if hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
    let t = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps.max(1), 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if let Some(p) = profile {
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, p)?;
        }
    }
    Ok(t)
}

/// Annex-B parameter sets advertised by the output type, if any.
fn sequence_header(t: &Option<IMFMediaType>) -> Option<Vec<u8>> {
    let t = t.as_ref()?;
    let size = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) }.ok()?;
    Some(buf)
}

impl VideoEncoder for MfVideoEncoder {
    fn input_layout(&self) -> InputLayout {
        InputLayout::Nv12
    }

    fn is_hevc(&self) -> bool {
        self.hevc
    }

    fn encode(&mut self, frame: FrameInput<'_>, pts: Duration) -> Result<Vec<EncodedFrame>> {
        let FrameInput::Nv12 { y, uv, strides, dims } = frame else {
            bail!("Media Foundation encoder needs NV12 input");
        };
        if dims != self.dims {
            bail!("frame size {}x{} does not match encoder {}x{}", dims.0, dims.1, self.dims.0, self.dims.1);
        }
        let (w, h) = (dims.0 as usize, dims.1 as usize);
        let uv_rows = h.div_ceil(2);
        let time = (pts.as_nanos() / 100) as i64;
        let t0 = std::time::Instant::now();
        // Repack the planes straight into the media buffer (tightly packed NV12).
        let sample = make_sample_with(w * h + w * uv_rows, time, self.frame_duration, |dst| {
            let (y_dst, uv_dst) = dst.split_at_mut(w * h);
            for (row, out) in y_dst.chunks_exact_mut(w).enumerate() {
                out.copy_from_slice(&y[row * strides.0..row * strides.0 + w]);
            }
            for (row, out) in uv_dst.chunks_exact_mut(w).enumerate() {
                out.copy_from_slice(&uv[row * strides.1..row * strides.1 + w]);
            }
        })?;
        let t1 = std::time::Instant::now();
        self.frames_in += 1;
        let outputs = self.session.process(&sample)?;
        let t2 = std::time::Instant::now();
        let mut frames = Vec::with_capacity(outputs.len());
        for s in &outputs {
            if let Some(f) = self.convert_output(s)? {
                frames.push(f);
            }
        }
        let t3 = std::time::Instant::now();
        if t3 - t0 > Duration::from_millis(8) {
            let (wait, input, pump) = self.session.last_timing;
            log::debug!(
                "slow MF encode: sample {:.1} ms, process {:.1} ms (wait {:.1}, input {:.1}, pump {:.1}), output {:.1} ms, {} out",
                (t1 - t0).as_secs_f64() * 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                wait as f64 / 1e3,
                input as f64 / 1e3,
                pump as f64 / 1e3,
                (t3 - t2).as_secs_f64() * 1e3,
                outputs.len()
            );
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        let outputs = self.session.drain()?;
        let mut frames = Vec::with_capacity(outputs.len());
        for s in &outputs {
            if let Some(f) = self.convert_output(s)? {
                frames.push(f);
            }
        }
        log::info!("{}: {} frames in, {} out", self.description, self.frames_in, self.frames_out);
        Ok(frames)
    }

    fn codec_params(&self) -> Option<&CodecParams> {
        self.params.as_ref()
    }

    fn force_keyframe(&mut self) {
        self.session.set_u32(&CODECAPI_AVEncVideoForceKeyFrame, 1);
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}
