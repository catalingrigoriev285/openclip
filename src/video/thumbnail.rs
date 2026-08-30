//! Reads finished files back: the poster frame and running time the library
//! shows next to each recording.
//!
//! Durations come from the container itself (`mvhd` for MP4/MOV, `avih` /
//! `dmlh` for AVI) so they work on every platform; on Windows a Media
//! Foundation source reader fills in whatever the parsers do not know and
//! decodes the first video frame for the thumbnail. Elsewhere videos get no
//! poster — the GUI falls back to an icon.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use super::preview::{make_preview, PreviewImage};
use super::{PixelFormat, RawFrame, Scaler};

/// What the library knows about one file beyond its name, size and date.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    /// Running time, when the container (or Media Foundation) reports one.
    pub duration: Option<Duration>,
    /// Downscaled first frame (videos) or the image itself (pictures).
    pub poster: Option<PreviewImage>,
}

const IMAGE_EXT: [&str; 6] = ["png", "jpg", "jpeg", "bmp", "gif", "webp"];
const AUDIO_EXT: [&str; 6] = ["mp3", "wav", "m4a", "flac", "ogg", "aac"];

/// Poster frame (longest side at most `max_side` pixels) and duration of
/// `path`. Never fails: anything unreadable comes back empty.
pub fn probe(path: &Path, max_side: u32) -> MediaInfo {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if IMAGE_EXT.contains(&ext.as_str()) {
        return MediaInfo { duration: None, poster: image_poster(path, max_side) };
    }
    let audio = AUDIO_EXT.contains(&ext.as_str());
    let mut info = MediaInfo { duration: container_duration(path), poster: None };
    decode_with_os(path, max_side, !audio, &mut info);
    info
}

/// Fills in what the container parsers could not read, using whatever decoders
/// the operating system already ships.
#[cfg(windows)]
fn decode_with_os(path: &Path, max_side: u32, want_frame: bool, info: &mut MediaInfo) {
    if let Some(found) = mf::probe(path, max_side, want_frame) {
        info.duration = info.duration.or(found.duration);
        info.poster = found.poster;
    }
}

#[cfg(not(windows))]
fn decode_with_os(_path: &Path, _max_side: u32, _want_frame: bool, _info: &mut MediaInfo) {}

/// Decodes a picture and shrinks it to thumbnail size.
fn image_poster(path: &Path, max_side: u32) -> Option<PreviewImage> {
    let img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let frame = RawFrame {
        data: img.into_raw(),
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Rgba,
        pts: Duration::ZERO,
        mouse: None,
    };
    Some(thumbnail(&frame, max_side))
}

/// Box-filtered downscale to `max_side`, then RGBA for the GUI. Going through
/// [`Scaler`] first keeps text and thin lines readable; [`make_preview`] alone
/// samples nearest-neighbour, which turns a 1080p frame into noise.
fn thumbnail(frame: &RawFrame, max_side: u32) -> PreviewImage {
    let long = frame.width.max(frame.height);
    if long <= max_side || long == 0 {
        return make_preview(frame, max_side.max(long));
    }
    let scale = long as f32 / max_side as f32;
    let w = ((frame.width as f32 / scale).round() as u32).max(1);
    let h = ((frame.height as f32 / scale).round() as u32).max(1);
    let small = Scaler::new((frame.width, frame.height), (w, h)).scale(frame);
    make_preview(&small, w.max(h))
}

// ----- container parsers -------------------------------------------------------

/// Running time read straight out of the container header. Understands the two
/// formats openclip writes (plus MOV/M4A, which share MP4's box layout).
pub fn container_duration(path: &Path) -> Option<Duration> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 12];
    f.read_exact(&mut magic).ok()?;
    let end = f.seek(SeekFrom::End(0)).ok()?;
    if &magic[0..4] == b"RIFF" && &magic[8..12] == b"AVI " {
        avi_duration(&mut f, end)
    } else if &magic[4..8] == b"ftyp" {
        mp4_duration(&mut f, end)
    } else {
        None
    }
}

fn read_at(f: &mut File, pos: u64, buf: &mut [u8]) -> Option<()> {
    f.seek(SeekFrom::Start(pos)).ok()?;
    f.read_exact(buf).ok()
}

fn be32(b: &[u8]) -> u64 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
}

fn le32(b: &[u8]) -> u64 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
}

/// Size (including the header), four-character type and header length of the
/// MP4 box at `pos`.
fn mp4_box(f: &mut File, pos: u64, end: u64) -> Option<(u64, [u8; 4], u64)> {
    let mut head = [0u8; 8];
    read_at(f, pos, &mut head)?;
    let kind = [head[4], head[5], head[6], head[7]];
    let size = be32(&head[0..4]);
    match size {
        // 0: the box runs to the end of the file. 1: 64-bit size follows.
        0 => Some((end.saturating_sub(pos), kind, 8)),
        1 => {
            let mut large = [0u8; 8];
            read_at(f, pos + 8, &mut large)?;
            Some((u64::from_be_bytes(large).max(16), kind, 16))
        }
        s if s >= 8 => Some((s, kind, 8)),
        _ => None,
    }
}

fn mp4_duration(f: &mut File, end: u64) -> Option<Duration> {
    let mut pos = 0u64;
    while pos + 8 <= end {
        let (size, kind, header) = mp4_box(f, pos, end)?;
        if &kind == b"moov" {
            let stop = (pos + size).min(end);
            let mut child = pos + header;
            while child + 8 <= stop {
                let (csize, ckind, cheader) = mp4_box(f, child, stop)?;
                if &ckind == b"mvhd" {
                    return mvhd_duration(f, child + cheader);
                }
                child += csize;
            }
            return None;
        }
        pos += size;
    }
    None
}

/// `mvhd` body: version/flags, creation and modification times, timescale,
/// duration. Version 1 widens the times and the duration to 64 bits.
fn mvhd_duration(f: &mut File, pos: u64) -> Option<Duration> {
    let mut body = [0u8; 32];
    read_at(f, pos, &mut body)?;
    let (timescale, ticks) = if body[0] == 1 {
        (be32(&body[20..24]), u64::from_be_bytes(body[24..32].try_into().ok()?))
    } else {
        (be32(&body[12..16]), be32(&body[16..20]))
    };
    // An unfinished file (or one written by a muxer that gave up) says 2^32-1.
    if timescale == 0 || ticks == 0 || ticks == u32::MAX as u64 {
        return None;
    }
    Some(Duration::from_secs_f64(ticks as f64 / timescale as f64))
}

/// AVI: frame time from `avih`, frame count from `dmlh` when the file is
/// OpenDML (`avih` only counts the first RIFF, so it is short for long
/// recordings).
fn avi_duration(f: &mut File, end: u64) -> Option<Duration> {
    let mut pos = 12u64;
    let mut head = [0u8; 12];
    // The `hdrl` list is the first thing in the RIFF.
    while pos + 12 <= end {
        read_at(f, pos, &mut head)?;
        let size = le32(&head[4..8]);
        if &head[0..4] == b"LIST" && &head[8..12] == b"hdrl" {
            break;
        }
        pos += 8 + size + (size & 1);
        if size == 0 {
            return None;
        }
    }
    let stop = (pos + 8 + le32(&head[4..8])).min(end);
    let (mut usec_per_frame, mut frames) = (0u64, 0u64);
    let mut child = pos + 12;
    while child + 8 <= stop {
        let mut ch = [0u8; 8];
        read_at(f, child, &mut ch)?;
        let size = le32(&ch[4..8]);
        if &ch[0..4] == b"avih" && size >= 20 {
            let mut body = [0u8; 20];
            read_at(f, child + 8, &mut body)?;
            usec_per_frame = le32(&body[0..4]);
            frames = le32(&body[16..20]);
        } else if &ch[0..4] == b"LIST" {
            // `odml` holds `dmlh`, whose frame count covers every RIFF.
            let mut kind = [0u8; 4];
            read_at(f, child + 8, &mut kind)?;
            if &kind == b"odml" {
                let mut dm = [0u8; 12];
                if read_at(f, child + 12, &mut dm).is_some() && &dm[0..4] == b"dmlh" {
                    let total = le32(&dm[8..12]);
                    frames = frames.max(total);
                }
            }
        }
        child += 8 + size + (size & 1);
        if size == 0 {
            break;
        }
    }
    if usec_per_frame == 0 || frames == 0 {
        return None;
    }
    Some(Duration::from_micros(usec_per_frame * frames))
}

// ----- Media Foundation (Windows) ----------------------------------------------

#[cfg(windows)]
mod mf {
    use std::path::Path;
    use std::time::Duration;

    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::{
        IMFAttributes, IMFSourceReader, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
        MFMediaType_Video, MFVideoFormat_RGB32, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
        MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ALL_STREAMS,
        MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    };

    use super::{thumbnail, MediaInfo, RawFrame};
    use crate::video::mf::{startup, ComGuard};

    /// How many `ReadSample` calls to spend looking for the first frame; the
    /// reader can hand back stream ticks and format changes before any data.
    const MAX_READS: u32 = 32;

    /// Duration (and, when `want_frame`, the first decoded video frame) through
    /// Windows' own demuxers and decoders. `None` if the file cannot be opened.
    pub fn probe(path: &Path, max_side: u32, want_frame: bool) -> Option<MediaInfo> {
        // Declared first so COM outlives every interface created below.
        let _com = ComGuard::new();
        startup().ok()?;
        let abs = std::fs::canonicalize(path).ok()?;
        let url = HSTRING::from(abs.to_string_lossy().trim_start_matches(r"\\?\"));
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: Media Foundation calls with valid, live arguments; every
        // failure is turned into `None` and nothing is retained.
        unsafe {
            MFCreateAttributes(&mut attrs, 1).ok()?;
            let attrs = attrs?;
            // Lets the reader insert a converter, so RGB32 can be requested
            // whatever the decoder natively produces.
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ok()?;
            let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs).ok()?;
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .map(|v| Duration::from_nanos(v.Anonymous.Anonymous.Anonymous.uhVal * 100));
            let poster = if want_frame { first_frame(&reader, max_side) } else { None };
            Some(MediaInfo { duration, poster })
        }
    }

    unsafe fn first_frame(reader: &IMFSourceReader, max_side: u32) -> Option<super::PreviewImage> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).ok()?;
            reader.SetStreamSelection(video, true).ok()?;
            let want = MFCreateMediaType().ok()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
            reader.SetCurrentMediaType(video, None, &want).ok()?;

            let mut sample = None;
            for _ in 0..MAX_READS {
                let mut flags = 0u32;
                let mut got = None;
                reader.ReadSample(video, 0, None, Some(&mut flags), None, Some(&mut got)).ok()?;
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                if got.is_some() {
                    sample = got;
                    break;
                }
            }
            let sample = sample?;

            let mt = reader.GetCurrentMediaType(video).ok()?;
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
            let (width, height) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            // RGB32 is commonly bottom-up, which the negative stride announces.
            let stride = mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((width * 4) as i32);
            if width == 0 || height == 0 || stride == 0 {
                return None;
            }

            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len)).ok()?;
            let src = std::slice::from_raw_parts(ptr, len as usize);
            let frame = copy_rows(src, width, height, stride);
            let _ = buffer.Unlock();
            Some(thumbnail(&frame?, max_side))
        }
    }

    /// Copies the locked buffer into a top-down, tightly packed BGRA frame.
    fn copy_rows(src: &[u8], width: u32, height: u32, stride: i32) -> Option<RawFrame> {
        RawFrame::from_bgra_rows(src, width, height, stride)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp4_and_avi_durations_are_read_back() {
        let dir = std::env::temp_dir().join("openclip-thumbnail-tests");
        std::fs::create_dir_all(&dir).unwrap();

        // `ftyp`, then a `moov` holding a version-0 `mvhd`: 90 000 ticks at a
        // timescale of 30 000 is three seconds.
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&16u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0; 4]);
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0; 4]); // version 0 + flags
        mvhd.extend_from_slice(&[0; 8]); // creation, modification
        mvhd.extend_from_slice(&30_000u32.to_be_bytes());
        mvhd.extend_from_slice(&90_000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0; 12]);
        mp4.extend_from_slice(&(16 + mvhd.len() as u32).to_be_bytes());
        mp4.extend_from_slice(b"moov");
        mp4.extend_from_slice(&(8 + mvhd.len() as u32).to_be_bytes());
        mp4.extend_from_slice(b"mvhd");
        mp4.extend_from_slice(&mvhd);
        let path = dir.join("a.mp4");
        std::fs::write(&path, &mp4).unwrap();
        assert_eq!(container_duration(&path), Some(Duration::from_secs(3)));

        // `avih` says 40 frames of 25 ms; `dmlh` corrects the total to 100.
        let mut hdrl = Vec::new();
        hdrl.extend_from_slice(b"hdrl");
        hdrl.extend_from_slice(b"avih");
        hdrl.extend_from_slice(&56u32.to_le_bytes());
        let mut avih = vec![0u8; 56];
        avih[0..4].copy_from_slice(&25_000u32.to_le_bytes());
        avih[16..20].copy_from_slice(&40u32.to_le_bytes());
        hdrl.extend_from_slice(&avih);
        hdrl.extend_from_slice(b"LIST");
        hdrl.extend_from_slice(&16u32.to_le_bytes());
        hdrl.extend_from_slice(b"odmldmlh");
        hdrl.extend_from_slice(&4u32.to_le_bytes());
        hdrl.extend_from_slice(&100u32.to_le_bytes());
        let mut avi = Vec::new();
        avi.extend_from_slice(b"RIFF");
        avi.extend_from_slice(&(12 + hdrl.len() as u32).to_le_bytes());
        avi.extend_from_slice(b"AVI LIST");
        avi.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
        avi.extend_from_slice(&hdrl);
        let path = dir.join("a.avi");
        std::fs::write(&path, &avi).unwrap();
        assert_eq!(container_duration(&path), Some(Duration::from_millis(2500)));

        // Neither format: no guess.
        let path = dir.join("a.bin");
        std::fs::write(&path, [0u8; 64]).unwrap();
        assert_eq!(container_duration(&path), None);
    }
}
