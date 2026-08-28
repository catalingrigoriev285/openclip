//! AAC-LC through the Windows Media Foundation AAC encoder (ships with
//! Windows 7+). Input is 16-bit PCM; output is raw AAC access units (no ADTS),
//! one 1024-sample frame per sample.

use anyhow::{anyhow, bail, Context, Result};
use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    IMFTransform, MFCreateMediaType, MFAudioFormat_AAC, MFAudioFormat_PCM, MFMediaType_Audio,
    MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_MT_USER_DATA,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use super::encoder::{AudioCodecConfig, AudioEncoder, AudioFrame};
use super::pcm::f32_to_i16;
use crate::settings::AAC_BITRATES;
use crate::video::mf::transform::{make_sample, sample_bytes, MftSession};
use crate::video::mf::{enumerate_activates, startup, ComGuard};

/// `CLSID_AACMFTEncoder` (not exported by the `windows` crate).
const CLSID_AAC_MFT_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
/// Samples per AAC-LC frame.
pub const AAC_SAMPLES_PER_FRAME: u32 = 1024;
/// Encoder priming: the first output frames decode to silence that precedes
/// the input. Dropping this many input samples keeps audio aligned with the
/// video timeline (same approach as the MP3 path).
pub const AAC_ENCODER_DELAY_SAMPLES: usize = 2112;

pub struct AacEncoder {
    session: MftSession,
    asc: Vec<u8>,
    rate: u32,
    channels: u16,
    bytes_per_sec: u32,
    pending: Vec<i16>,
    skip_samples: usize,
    pts_samples: u64,
    frames_out: u64,
    _com: ComGuard,
}

impl AacEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate_kbps: u32) -> Result<Self> {
        let com = ComGuard::new();
        startup()?;
        if !matches!(sample_rate, 44_100 | 48_000) {
            bail!("AAC encoder supports 44100 / 48000 Hz only");
        }
        let channels = channels.clamp(1, 2);
        let kbps = *AAC_BITRATES.iter().min_by_key(|b| b.abs_diff(bitrate_kbps)).unwrap();
        let bytes_per_sec = kbps * 1000 / 8;

        let mft = create_transform()?;
        let mut session = MftSession::from_transform(mft)?;

        let in_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            in_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            in_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels as u32 * 2)?;
            in_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * channels as u32 * 2)?;
            in_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        }
        let out_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            out_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
            out_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        }
        // The AAC encoder accepts either order; try input first, then output.
        let mut ok = session.set_input_type(&in_type).and_then(|_| session.set_output_type(&out_type));
        if ok.is_err() {
            ok = session.set_output_type(&out_type).and_then(|_| session.set_input_type(&in_type));
        }
        ok.context("AAC encoder rejected the audio format")?;

        // AudioSpecificConfig follows the 12-byte HEAACWAVEINFO tail in MF_MT_USER_DATA.
        let current = session.output_type()?;
        let size = unsafe { current.GetBlobSize(&MF_MT_USER_DATA) }.unwrap_or(0) as usize;
        let mut user = vec![0u8; size];
        if size > 0 {
            unsafe { current.GetBlob(&MF_MT_USER_DATA, &mut user, None) }?;
        }
        let asc = if user.len() > 12 {
            user[12..].to_vec()
        } else {
            // Derive AAC-LC config: profile 2, sampling index, channel config.
            let idx: u8 = if sample_rate == 48_000 { 3 } else { 4 };
            vec![(2 << 3) | (idx >> 1), ((idx & 1) << 7) | ((channels as u8) << 3)]
        };
        session.start(64 * 1024)?;
        log::info!("AAC: Media Foundation encoder, {sample_rate} Hz, {channels} ch, {kbps} kbps, asc {asc:02x?}");
        Ok(Self {
            session,
            asc,
            rate: sample_rate,
            channels,
            bytes_per_sec,
            pending: Vec::new(),
            skip_samples: AAC_ENCODER_DELAY_SAMPLES,
            pts_samples: 0,
            frames_out: 0,
            _com: com,
        })
    }

    fn feed(&mut self, final_flush: bool) -> Result<Vec<AudioFrame>> {
        let ch = self.channels as usize;
        let block = AAC_SAMPLES_PER_FRAME as usize * ch;
        let mut out = Vec::new();
        while self.pending.len() >= block || (final_flush && !self.pending.is_empty()) {
            let n = self.pending.len().min(block);
            let rest = self.pending.split_off(n);
            let chunk = std::mem::replace(&mut self.pending, rest);
            let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
            let frames = (chunk.len() / ch) as u64;
            let time = (self.pts_samples * 10_000_000 / self.rate as u64) as i64;
            let dur = (frames * 10_000_000 / self.rate as u64) as i64;
            self.pts_samples += frames;
            let sample = make_sample(&bytes, time, dur)?;
            for s in self.session.process(&sample)? {
                let data = sample_bytes(&s)?;
                if !data.is_empty() {
                    self.frames_out += 1;
                    out.push(AudioFrame { data, samples: AAC_SAMPLES_PER_FRAME });
                }
            }
        }
        Ok(out)
    }
}

fn create_transform() -> Result<IMFTransform> {
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    if let Ok(acts) = enumerate_activates(MFT_CATEGORY_AUDIO_ENCODER, flags, MFMediaType_Audio, MFAudioFormat_AAC) {
        for a in acts {
            if let Ok(t) = unsafe { a.ActivateObject::<IMFTransform>() } {
                return Ok(t);
            }
        }
    }
    unsafe { CoCreateInstance(&CLSID_AAC_MFT_ENCODER, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| anyhow!("no AAC encoder available: {e}"))
}

impl AudioEncoder for AacEncoder {
    fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<AudioFrame>> {
        let ch = self.channels as usize;
        let mut input = interleaved;
        if self.skip_samples > 0 {
            let skip = (self.skip_samples * ch).min(input.len());
            input = &input[skip..];
            self.skip_samples -= skip / ch;
        }
        self.pending.extend(input.iter().map(|&v| f32_to_i16(v)));
        self.feed(false)
    }

    fn flush(&mut self) -> Result<Vec<AudioFrame>> {
        let mut out = self.feed(true)?;
        for s in self.session.drain()? {
            let data = sample_bytes(&s)?;
            if !data.is_empty() {
                self.frames_out += 1;
                out.push(AudioFrame { data, samples: AAC_SAMPLES_PER_FRAME });
            }
        }
        log::info!("AAC: {} frames emitted", self.frames_out);
        Ok(out)
    }

    fn samples_per_frame(&self) -> u32 {
        AAC_SAMPLES_PER_FRAME
    }

    fn bitrate_bps(&self) -> u32 {
        self.bytes_per_sec * 8
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn codec_config(&self) -> AudioCodecConfig {
        AudioCodecConfig::Aac { asc: self.asc.clone() }
    }

    fn describe(&self) -> String {
        format!("AAC (Media Foundation) {} Hz {}ch {} kbps", self.rate, self.channels, self.bytes_per_sec * 8 / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a tone and checks the encoder produces whole AAC frames with a
    /// sensible AudioSpecificConfig. Skipped when the transform is unavailable.
    #[test]
    fn encodes_tone() {
        let Ok(mut enc) = AacEncoder::new(48_000, 2, 160) else {
            eprintln!("AAC encoder unavailable; skipping");
            return;
        };
        assert_eq!(enc.codec_config(), AudioCodecConfig::Aac { asc: vec![0x11, 0x90] });
        let mut pcm = Vec::new();
        for i in 0..48_000 {
            let v = ((i as f32 / 48_000.0) * 440.0 * std::f32::consts::TAU).sin() * 0.3;
            pcm.push(v);
            pcm.push(v);
        }
        let mut frames = enc.encode(&pcm).unwrap();
        frames.extend(enc.flush().unwrap());
        assert!(frames.len() >= 40, "{} frames", frames.len());
        assert!(frames.iter().all(|f| f.samples == 1024 && !f.data.is_empty()));
    }
}
