//! Decodes a recording with Windows' own Media Foundation source reader (the
//! demuxers and decoders Windows Media Player / Movies & TV use) and reports
//! the streams, duration and how many frames / audio samples decoded — an
//! independent check that the MP4 and AVI writers produce playable files.
//!
//! Usage: cargo run --example probe_media -- FILE [FILE …]

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: probe_media FILE…");
        std::process::exit(2);
    }
    let mut failed = false;
    for f in files {
        match probe(&f) {
            Ok(report) => println!("{f}: {report}"),
            Err(e) => {
                println!("{f}: ERROR {e:#}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn probe(_path: &str) -> anyhow::Result<String> {
    anyhow::bail!("probe_media needs Windows Media Foundation")
}

#[cfg(windows)]
fn probe(path: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    use openclip::video::mf::{startup, ComGuard};
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::{
        IMFSourceReader, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
        MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READER_ALL_STREAMS,
        MFVideoFormat_H264, MFVideoFormat_HEVC, MFAudioFormat_AAC, MFAudioFormat_MP3, MFAudioFormat_PCM,
        MF_SOURCE_READERF_ENDOFSTREAM,
    };

    let _com = ComGuard::new();
    startup()?;
    let abs = std::fs::canonicalize(path).with_context(|| format!("{path} not found"))?;
    let url = HSTRING::from(abs.to_string_lossy().trim_start_matches(r"\\?\"));
    let reader: IMFSourceReader = unsafe { MFCreateSourceReaderFromURL(&url, None) }.context("opening with Media Foundation")?;
    unsafe { reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, true) }?;

    let duration = unsafe { reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION) }
        .ok()
        .map(|v| unsafe { v.Anonymous.Anonymous.Anonymous.uhVal } as f64 / 10_000_000.0);

    let mut streams = Vec::new();
    for i in 0.. {
        let Ok(t) = (unsafe { reader.GetNativeMediaType(i, 0) }) else { break };
        let major = unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }?;
        let sub = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }?;
        let codec = if sub == MFVideoFormat_H264 {
            "H264".to_string()
        } else if sub == MFVideoFormat_HEVC {
            "HEVC".to_string()
        } else if sub == MFAudioFormat_AAC {
            "AAC".to_string()
        } else if sub == MFAudioFormat_MP3 {
            "MP3".to_string()
        } else if sub == MFAudioFormat_PCM {
            "PCM".to_string()
        } else {
            format!("{sub:?}")
        };
        if major == MFMediaType_Video {
            let size = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
            streams.push(format!("video {codec} {}×{}", size >> 32, size & 0xFFFF_FFFF));
        } else if major == MFMediaType_Audio {
            let rate = unsafe { t.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.unwrap_or(0);
            let ch = unsafe { t.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.unwrap_or(0);
            streams.push(format!("audio {codec} {rate} Hz {ch}ch"));
        } else {
            streams.push(format!("other {codec}"));
        }
    }

    // Decode everything (the reader inserts decoders automatically).
    let mut video_frames = 0u64;
    let mut audio_samples = 0u64;
    let mut last_video_ts = 0i64;
    let mut ended = 0;
    let total = streams.len();
    while ended < total {
        let mut index = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample = None;
        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_ALL_STREAMS.0 as u32,
                0,
                Some(&mut index),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )
        }
        .context("ReadSample")?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            ended += 1;
            continue;
        }
        if let Some(s) = sample {
            let is_video = streams.get(index as usize).map(|s| s.starts_with("video")).unwrap_or(false);
            if is_video {
                video_frames += 1;
                last_video_ts = ts;
            } else {
                let t = unsafe { reader.GetCurrentMediaType(index) }?;
                let ch = unsafe { t.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.unwrap_or(2).max(1);
                let bytes = unsafe { s.GetTotalLength() }.unwrap_or(0) as u64;
                audio_samples += bytes / (ch as u64 * 2); // PCM 16-bit after decoding
            }
        }
    }
    Ok(format!(
        "{} | duration {} | decoded {video_frames} video frames (last pts {:.2}s), {audio_samples} audio sample frames",
        streams.join(", "),
        duration.map(|d| format!("{d:.2}s")).unwrap_or_else(|| "?".into()),
        last_video_ts as f64 / 10_000_000.0
    ))
}
