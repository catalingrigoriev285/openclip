//! Media Foundation decoding for the viewer.
//!
//! One [`IMFSourceReader`] with the first video and the first audio stream
//! selected, decoding to RGB32 and 16-bit PCM. It builds on the poster-frame
//! reader in [`crate::video::thumbnail`] and adds what playback needs: sample
//! timestamps, both streams at once, format-change handling and seeking.
//!
//! Everything here is thread-affine — the reader must be created, used and
//! dropped on one thread with a live `ComGuard`.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_RGB32,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED, MF_SOURCE_READERF_NEWSTREAM, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_MEDIASOURCE,
};

use crate::capture::FramePool;
use crate::video::mf::{propvariant_i64, startup};
use crate::video::preview::{make_preview_into, PreviewImage};
use crate::video::RawFrame;

use super::{frame_interval_from_rate, MAX_DECODE_SIDE};

/// `GUID_NULL` — the default (100 ns) time format for a seek. The constant
/// itself lives behind the `Win32_Media_KernelStreaming` feature, which this
/// crate does not enable, so it is spelled out here.
const TIME_FORMAT_NULL: GUID = GUID::from_u128(0);

/// How many stream indices to probe when looking for video and audio.
const MAX_STREAMS: u32 = 16;

#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioInfo {
    pub rate: u32,
    pub channels: u16,
}

/// What one `ReadSample` produced.
pub enum Packet {
    Video { image: PreviewImage, pts: Duration },
    /// A video sample deliberately left unconverted — seek pre-roll. Skipping
    /// the pixel work here is what makes seeking feel immediate.
    VideoSkipped { pts: Duration },
    Audio { samples: Vec<f32>, channels: usize, pts: Duration },
    /// A gap, a format change or a stream that ended while others continue.
    Idle,
    /// Every selected stream has ended.
    End,
}

pub struct Reader {
    reader: IMFSourceReader,
    video: Option<u32>,
    audio: Option<u32>,
    video_eos: bool,
    audio_eos: bool,
    /// Row pitch of the decoded RGB32; negative means bottom-up.
    stride: i32,
    pub duration: Option<Duration>,
    pub video_info: Option<VideoInfo>,
    pub audio_info: Option<AudioInfo>,
}

impl Reader {
    /// Opens `path`. Tries the advanced video processor first (it can scale the
    /// output, which caps what a 4K file costs per frame) and falls back to the
    /// basic converter for sources that refuse it.
    pub fn open(path: &Path) -> Result<Reader> {
        match Reader::open_with(path, true) {
            Ok(r) => Ok(r),
            Err(e) => {
                log::debug!("advanced video processing unavailable ({e:#}); retrying without it");
                Reader::open_with(path, false)
            }
        }
    }

    fn open_with(path: &Path, advanced: bool) -> Result<Reader> {
        startup()?;
        let abs = std::fs::canonicalize(path).context("resolving the file path")?;
        // Media Foundation rejects the extended-length prefix.
        let url = HSTRING::from(abs.to_string_lossy().trim_start_matches(r"\\?\"));
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: every call gets live, valid arguments and each failure is
        // turned into an error without retaining anything.
        unsafe {
            MFCreateAttributes(&mut attrs, 1).context("MFCreateAttributes")?;
            let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))?;
            // Lets the reader insert a converter so RGB32 can be requested
            // whatever the decoder natively produces.
            let key = if advanced {
                MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING
            } else {
                MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING
            };
            attrs.SetUINT32(&key, 1)?;
            let reader: IMFSourceReader =
                MFCreateSourceReaderFromURL(&url, &attrs).context("opening the file")?;

            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .map(|v| Duration::from_nanos(v.Anonymous.Anonymous.Anonymous.uhVal * 100))
                .filter(|d| !d.is_zero());

            // `ReadSample(ALL_STREAMS, ..)` reports the *real* stream index, not
            // the FIRST_VIDEO/FIRST_AUDIO sentinel, so resolve them up front.
            let (mut video, mut audio) = (None, None);
            for i in 0..MAX_STREAMS {
                let Ok(t) = reader.GetNativeMediaType(i, 0) else { break };
                let Ok(major) = t.GetGUID(&MF_MT_MAJOR_TYPE) else { continue };
                if major == MFMediaType_Video && video.is_none() {
                    video = Some(i);
                } else if major == MFMediaType_Audio && audio.is_none() {
                    audio = Some(i);
                }
            }
            if video.is_none() && audio.is_none() {
                bail!("no video or audio stream");
            }

            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            if let Some(i) = video {
                reader.SetStreamSelection(i, true)?;
            }
            if let Some(i) = audio {
                reader.SetStreamSelection(i, true)?;
            }

            let mut me = Reader {
                reader,
                video,
                audio,
                video_eos: video.is_none(),
                audio_eos: audio.is_none(),
                stride: 0,
                duration,
                video_info: None,
                audio_info: None,
            };
            if let Some(i) = video {
                me.setup_video(i, advanced)?;
            }
            // A file without usable sound still plays; only video is fatal.
            if let Some(i) = audio
                && let Err(e) = me.setup_audio(i)
            {
                log::warn!("audio track unusable ({e:#}); playing without sound");
                me.reader.SetStreamSelection(i, false).ok();
                me.audio = None;
                me.audio_eos = true;
            }
            Ok(me)
        }
    }

    /// Requests RGB32, capping the long side so a 4K file does not cost 33 MB
    /// per frame, then reads back the geometry that was actually negotiated.
    fn setup_video(&mut self, stream: u32, advanced: bool) -> Result<()> {
        // SAFETY: MF calls on a live reader; failures propagate as errors.
        unsafe {
            let native = self.reader.GetNativeMediaType(stream, 0).ok();
            let cap = native
                .as_ref()
                .and_then(|t| t.GetUINT64(&MF_MT_FRAME_SIZE).ok())
                .map(|s| ((s >> 32) as u32, (s & 0xFFFF_FFFF) as u32))
                .and_then(|(w, h)| capped_size(w, h));

            let make = |size: Option<(u32, u32)>| -> Result<IMFMediaType> {
                let want = MFCreateMediaType()?;
                want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
                if let Some((w, h)) = size {
                    want.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
                }
                Ok(want)
            };

            // Only the advanced processor can resize; if the sized request is
            // refused, fall back to the source's own dimensions.
            let sized = advanced.then_some(cap).flatten();
            if sized.is_none()
                || self.reader.SetCurrentMediaType(stream, None, &make(sized)?).is_err()
            {
                self.reader
                    .SetCurrentMediaType(stream, None, &make(None)?)
                    .context("negotiating RGB32 video")?;
            }
            self.refresh_video(stream)
        }
    }

    fn refresh_video(&mut self, stream: u32) -> Result<()> {
        // SAFETY: reading attributes off a media type we own.
        unsafe {
            let mt = self.reader.GetCurrentMediaType(stream)?;
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).context("frame size")?;
            let (width, height) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            if width == 0 || height == 0 {
                bail!("the video stream reports an empty frame size");
            }
            // RGB32 is commonly bottom-up, which the negative stride announces.
            self.stride = mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((width * 4) as i32);
            if self.stride == 0 {
                self.stride = (width * 4) as i32;
            }
            let interval = frame_interval_from_rate(mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0));
            self.video_info = Some(VideoInfo { width, height, interval });
            Ok(())
        }
    }

    /// Pins only PCM and 16 bits, then takes whatever rate and channel count MF
    /// settles on — demanding the device's exact layout here is what makes some
    /// decoders refuse outright.
    fn setup_audio(&mut self, stream: u32) -> Result<()> {
        // SAFETY: MF calls on a live reader.
        unsafe {
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            want.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            self.reader.SetCurrentMediaType(stream, None, &want).context("negotiating PCM audio")?;
            self.refresh_audio(stream)
        }
    }

    fn refresh_audio(&mut self, stream: u32) -> Result<()> {
        // SAFETY: reading attributes off a media type we own.
        unsafe {
            let mt = self.reader.GetCurrentMediaType(stream)?;
            let bits = mt.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16);
            if bits != 16 {
                bail!("decoder produced {bits}-bit samples, expected 16");
            }
            let rate = mt.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).context("sample rate")?;
            let channels = mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2).max(1) as u16;
            if rate == 0 {
                bail!("the audio stream reports a zero sample rate");
            }
            self.audio_info = Some(AudioInfo { rate, channels });
            Ok(())
        }
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn finished(&self) -> bool {
        self.video_eos && self.audio_eos
    }

    /// Reads one sample. Video samples whose timestamp is before `pixels_from`
    /// come back as [`Packet::VideoSkipped`] without being converted.
    pub fn read(&mut self, pixels_from: Option<Duration>, pool: &FramePool) -> Result<Packet> {
        let mut index = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        // SAFETY: all out-parameters are live locals for the duration of the call.
        unsafe {
            self.reader.ReadSample(
                MF_SOURCE_READER_ALL_STREAMS.0 as u32,
                0,
                Some(&mut index),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
        }
        let has = |f: i32| flags & f as u32 != 0;

        if has(MF_SOURCE_READERF_ERROR.0) {
            bail!("the decoder reported an error");
        }
        // The topology changed under us: re-apply both requested output types.
        if has(MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0) || has(MF_SOURCE_READERF_NEWSTREAM.0) {
            if let Some(i) = self.video {
                self.setup_video(i, true).ok();
            }
            if let Some(i) = self.audio {
                self.setup_audio(i).ok();
            }
            return Ok(Packet::Idle);
        }
        if has(MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) {
            if Some(index) == self.video {
                self.refresh_video(index).ok();
            } else if Some(index) == self.audio {
                self.refresh_audio(index).ok();
            }
        }
        if has(MF_SOURCE_READERF_ENDOFSTREAM.0) {
            if Some(index) == self.video {
                self.video_eos = true;
            }
            if Some(index) == self.audio {
                self.audio_eos = true;
            }
            return Ok(if self.finished() { Packet::End } else { Packet::Idle });
        }
        // A stream tick, or simply nothing this time round.
        let Some(sample) = sample else { return Ok(Packet::Idle) };
        let pts = Duration::from_nanos(ts.max(0) as u64 * 100);

        if Some(index) == self.video {
            if pixels_from.is_some_and(|t| pts < t) {
                return Ok(Packet::VideoSkipped { pts });
            }
            let Some(image) = self.video_pixels(&sample, pool)? else { return Ok(Packet::Idle) };
            Ok(Packet::Video { image, pts })
        } else if Some(index) == self.audio {
            let Some(info) = self.audio_info else { return Ok(Packet::Idle) };
            let samples = pcm_to_f32(&sample)?;
            Ok(Packet::Audio { samples, channels: info.channels as usize, pts })
        } else {
            Ok(Packet::Idle)
        }
    }

    fn video_pixels(&mut self, sample: &IMFSample, pool: &FramePool) -> Result<Option<PreviewImage>> {
        let Some(info) = self.video_info else { return Ok(None) };
        // SAFETY: the buffer is locked and unlocked around one bounded read.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let src = std::slice::from_raw_parts(ptr, len as usize);
            let frame = RawFrame::from_bgra_rows(src, info.width, info.height, self.stride);
            let _ = buffer.Unlock();
            // `max_side` at or above the longer side makes this a straight
            // BGRA→RGBA copy with opaque alpha and no resampling.
            Ok(frame.map(|f| make_preview_into(&f, info.width.max(info.height), pool.take())))
        }
    }

    /// Jumps to `to`. Media Foundation lands on the keyframe at or before the
    /// target, so the caller still has to decode forward to reach it.
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let pv = propvariant_i64((to.as_nanos() / 100) as i64);
        // SAFETY: `pv` is a VT_I8 variant that outlives the call and owns nothing.
        unsafe {
            self.reader.Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32).ok();
            self.reader.SetCurrentPosition(&TIME_FORMAT_NULL, &pv)?;
        }
        self.video_eos = self.video.is_none();
        self.audio_eos = self.audio.is_none();
        Ok(())
    }
}

/// Reads a 16-bit PCM sample into interleaved `f32`, the same scaling
/// [`crate::audio::capture`] applies in the opposite direction.
fn pcm_to_f32(sample: &IMFSample) -> Result<Vec<f32>> {
    // SAFETY: the buffer is locked and unlocked around one bounded read.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let bytes = std::slice::from_raw_parts(ptr, len as usize);
        let out = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        let _ = buffer.Unlock();
        Ok(out)
    }
}

/// Output size for a source of `w`×`h`, shrunk to [`MAX_DECODE_SIDE`] on its
/// longer side. `None` when the source already fits and needs no scaling.
pub fn capped_size(w: u32, h: u32) -> Option<(u32, u32)> {
    let long = w.max(h);
    if w == 0 || h == 0 || long <= MAX_DECODE_SIDE {
        return None;
    }
    let scale = long as f64 / MAX_DECODE_SIDE as f64;
    // Even dimensions keep every chroma-subsampled decoder happy.
    let cw = (((w as f64 / scale).round() as u32).max(2)) & !1;
    let ch = (((h as f64 / scale).round() as u32).max(2)) & !1;
    Some((cw, ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_size_only_shrinks_oversized_sources() {
        // 1080p and 4K already fit.
        assert_eq!(capped_size(1920, 1080), None);
        assert_eq!(capped_size(3840, 2160), None);
        assert_eq!(capped_size(0, 0), None);

        // 8K halves, staying on even dimensions.
        assert_eq!(capped_size(7680, 4320), Some((3840, 2160)));
        let (w, h) = capped_size(5000, 3000).unwrap();
        assert_eq!(w, MAX_DECODE_SIDE);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        // Aspect ratio survives the round trip.
        assert!(((w as f64 / h as f64) - (5000.0 / 3000.0)).abs() < 0.01);

        // Portrait is capped on its own long side.
        assert_eq!(capped_size(2160, 7680), Some((1080, 3840)));
    }
}
