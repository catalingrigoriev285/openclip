//! Round-trips H.264 + MP3 / PCM through the OpenDML AVI writer and checks
//! the RIFF structure, headers, frame slots and indexes with a small
//! hand-written RIFF walker.

use std::cell::RefCell;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::time::Duration;

use openclip::audio::{AudioEncoder, Mp3Encoder, PcmEncoder};
use openclip::mux::{AudioTrackConfig, AviWriter, VideoCodecConfig, VideoTrackConfig};
use openclip::video::{Converter, EncoderRequest, H264Encoder, InputLayout, PixelFormat, RawFrame, VideoEncoder};

const W: u32 = 320;
const H: u32 = 180;
const FPS: u32 = 30;
const FRAMES: u32 = 60;
const RATE: u32 = 48_000;
/// Frame indexes that are "dropped" (never pushed) → empty slots.
const GAP: std::ops::Range<u32> = 20..23;

#[derive(Clone)]
struct Shared(Rc<RefCell<Cursor<Vec<u8>>>>);

impl Shared {
    fn new() -> Self {
        Shared(Rc::new(RefCell::new(Cursor::new(Vec::new()))))
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.borrow().get_ref().clone()
    }
}

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl Seek for Shared {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.borrow_mut().seek(pos)
    }
}

enum Audio {
    None,
    Mp3,
    Pcm,
}

struct Built {
    bytes: Vec<u8>,
    pushed: usize,
    keyframes: Vec<bool>,
    audio_chunks: usize,
    audio_samples: u64,
}

fn build(audio: Audio, small_riff: bool) -> Built {
    let shared = Shared::new();
    let mut converter = Converter::new(W, H, InputLayout::I420).unwrap();
    let mut video = H264Encoder::new(&EncoderRequest::simple(W, H, FPS, 800_000)).unwrap();
    let mut enc: Option<Box<dyn AudioEncoder>> = match audio {
        Audio::None => None,
        Audio::Mp3 => Some(Box::new(Mp3Encoder::new(RATE, 2, 128).unwrap())),
        Audio::Pcm => Some(Box::new(PcmEncoder::new(RATE, 2))),
    };
    let audio_cfg = enc.as_ref().map(|e| AudioTrackConfig {
        sample_rate: e.sample_rate(),
        channels: e.channels(),
        bitrate_bps: e.bitrate_bps(),
        samples_per_frame: e.samples_per_frame(),
        codec: e.codec_config(),
    });
    let mut mux = AviWriter::new(
        shared.clone(),
        VideoTrackConfig { width: W, height: H, fps: FPS as f64, codec: VideoCodecConfig::H264 },
        audio_cfg,
    )
    .unwrap();
    if small_riff {
        mux.set_max_riff_for_tests(64 * 1024);
    }

    let mut frame = RawFrame {
        data: vec![0u8; (W * H * 4) as usize],
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra,
        pts: Duration::ZERO,
        mouse: None,
    };
    let mut keyframes = Vec::new();
    let mut pushed = 0;
    let mut audio_chunks = 0;
    let mut audio_samples = 0u64;
    let mut pcm = Vec::new();
    for i in 0..FRAMES {
        for (n, px) in frame.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (n as u32 % W + i * 5) % W;
            px[0] = (x * 255 / W) as u8;
            px[1] = ((n as u32 / W) * 255 / H) as u8;
            px[2] = if (x / 16).is_multiple_of(2) { 220 } else { 20 };
            px[3] = 255;
        }
        frame.pts = Duration::from_secs_f64(i as f64 / FPS as f64);
        converter.convert(&frame).unwrap();
        for f in video.encode(converter.frame(), frame.pts).unwrap() {
            if GAP.contains(&i) {
                continue; // simulate dropped frames
            }
            assert!(mux.push_video(&f.data, f.pts, f.keyframe).unwrap());
            keyframes.push(f.keyframe);
            pushed += 1;
        }
        if let Some(e) = enc.as_mut() {
            pcm.clear();
            for s in 0..(RATE / FPS) {
                let t = (i * (RATE / FPS) + s) as f32 / RATE as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                pcm.push(v);
                pcm.push(v);
            }
            for f in e.encode(&pcm).unwrap() {
                mux.push_audio(&f.data, f.samples).unwrap();
                audio_chunks += 1;
                audio_samples += f.samples as u64;
            }
        }
    }
    // A frame landing on an already used slot is refused.
    assert!(!mux.push_video(&[0, 0, 0, 1, 0x65, 1], Duration::from_secs_f64(5.0 / FPS as f64), true).unwrap());
    if let Some(e) = enc.as_mut() {
        for f in e.flush().unwrap() {
            mux.push_audio(&f.data, f.samples).unwrap();
            audio_chunks += 1;
            audio_samples += f.samples as u64;
        }
    }
    mux.finalize().unwrap();
    Built { bytes: shared.bytes(), pushed, keyframes, audio_chunks, audio_samples }
}

// ----- minimal RIFF reader -------------------------------------------------------

fn u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// A chunk: (fourcc, list type if LIST/RIFF, payload start, payload size).
struct Chunk {
    id: [u8; 4],
    kind: Option<[u8; 4]>,
    start: usize,
    size: usize,
}

/// Iterates the chunks in `bytes[from..to]`.
fn chunks(bytes: &[u8], from: usize, to: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut p = from;
    while p + 8 <= to {
        let id: [u8; 4] = bytes[p..p + 4].try_into().unwrap();
        let size = u32(bytes, p + 4) as usize;
        let (kind, start) = if &id == b"RIFF" || &id == b"LIST" {
            (Some(bytes[p + 8..p + 12].try_into().unwrap()), p + 12)
        } else {
            (None, p + 8)
        };
        out.push(Chunk { id, kind, start, size: if kind.is_some() { size - 4 } else { size } });
        p += 8 + size + (size % 2);
    }
    out
}

fn find<'a>(list: &'a [Chunk], id: &[u8; 4], kind: Option<&[u8; 4]>) -> Option<&'a Chunk> {
    list.iter().find(|c| &c.id == id && kind.map(|k| c.kind.as_ref() == Some(k)).unwrap_or(true))
}

struct Parsed {
    riffs: Vec<Chunk>,
    hdrl_start: usize,
    hdrl_end: usize,
}

fn parse(bytes: &[u8]) -> Parsed {
    let riffs = chunks(bytes, 0, bytes.len());
    assert!(!riffs.is_empty());
    assert_eq!(riffs[0].kind.as_ref(), Some(b"AVI "));
    for r in &riffs[1..] {
        assert_eq!(r.kind.as_ref(), Some(b"AVIX"), "continuation RIFFs are AVIX");
    }
    let top = chunks(bytes, riffs[0].start, riffs[0].start + riffs[0].size);
    let hdrl = find(&top, b"LIST", Some(b"hdrl")).expect("hdrl");
    Parsed { riffs, hdrl_start: hdrl.start, hdrl_end: hdrl.start + hdrl.size }
}

#[test]
fn h264_mp3_avi_structure() {
    let b = build(Audio::Mp3, false);
    let bytes = &b.bytes;
    let p = parse(bytes);
    assert_eq!(p.riffs.len(), 1, "one RIFF for a small file");
    assert_eq!(u32(bytes, 4) as usize + 8, bytes.len(), "RIFF size covers the file");

    // --- headers ---
    let hdrl = chunks(bytes, p.hdrl_start, p.hdrl_end);
    let avih = find(&hdrl, b"avih", None).unwrap();
    assert_eq!(u32(bytes, avih.start), 1_000_000 / FPS);
    let total_slots = (FRAMES - (GAP.end - GAP.start)) as usize + GAP.len(); // dropped frames keep their slots
    assert_eq!(u32(bytes, avih.start + 16) as usize, total_slots, "dwTotalFrames");
    assert_eq!(u32(bytes, avih.start + 24), 2, "dwStreams");
    assert_eq!((u32(bytes, avih.start + 32), u32(bytes, avih.start + 36)), (W, H));
    let strls: Vec<&Chunk> = hdrl.iter().filter(|c| c.kind.as_ref() == Some(b"strl")).collect();
    assert_eq!(strls.len(), 2);

    let vs = chunks(bytes, strls[0].start, strls[0].start + strls[0].size);
    let strh = find(&vs, b"strh", None).unwrap();
    assert_eq!(&bytes[strh.start..strh.start + 4], b"vids");
    assert_eq!(&bytes[strh.start + 4..strh.start + 8], b"H264");
    assert_eq!((u32(bytes, strh.start + 20), u32(bytes, strh.start + 24)), (1, FPS), "scale/rate");
    assert_eq!(u32(bytes, strh.start + 32) as usize, total_slots, "dwLength");
    let strf = find(&vs, b"strf", None).unwrap();
    assert_eq!(u32(bytes, strf.start), 40);
    assert_eq!(&bytes[strf.start + 16..strf.start + 20], b"H264");
    let indx = find(&vs, b"indx", None).unwrap();
    assert_eq!(u16(bytes, indx.start), 4, "wLongsPerEntry");
    assert_eq!(bytes[indx.start + 3], 0, "AVI_INDEX_OF_INDEXES");
    assert_eq!(u32(bytes, indx.start + 4), 1, "one ix chunk");
    assert_eq!(&bytes[indx.start + 8..indx.start + 12], b"00dc");

    let aus = chunks(bytes, strls[1].start, strls[1].start + strls[1].size);
    let astrh = find(&aus, b"strh", None).unwrap();
    assert_eq!(&bytes[astrh.start..astrh.start + 4], b"auds");
    assert_eq!((u32(bytes, astrh.start + 20), u32(bytes, astrh.start + 24)), (1152, RATE));
    assert_eq!(u32(bytes, astrh.start + 32) as usize, b.audio_chunks, "audio dwLength = MP3 frames");
    let astrf = find(&aus, b"strf", None).unwrap();
    assert_eq!(u16(bytes, astrf.start), 0x0055, "MP3 format tag");
    assert_eq!(u16(bytes, astrf.start + 2), 2);
    assert_eq!(u32(bytes, astrf.start + 4), RATE);
    assert_eq!(u16(bytes, astrf.start + 16), 12, "MPEGLAYER3WAVEFORMAT extension");

    let odml = find(&hdrl, b"LIST", Some(b"odml")).unwrap();
    let odml_children = chunks(bytes, odml.start, odml.start + odml.size);
    let dmlh = find(&odml_children, b"dmlh", None).unwrap();
    assert_eq!(u32(bytes, dmlh.start) as usize, total_slots);

    // --- movi + idx1 ---
    let top = chunks(bytes, p.riffs[0].start, p.riffs[0].start + p.riffs[0].size);
    let movi = find(&top, b"LIST", Some(b"movi")).unwrap();
    let movi_fourcc_pos = movi.start - 4;
    let data = chunks(bytes, movi.start, movi.start + movi.size);
    let video_chunks: Vec<&Chunk> = data.iter().filter(|c| &c.id == b"00dc").collect();
    let audio_chunks: Vec<&Chunk> = data.iter().filter(|c| &c.id == b"01wb").collect();
    assert_eq!(video_chunks.len(), total_slots);
    assert_eq!(video_chunks.iter().filter(|c| c.size == 0).count(), GAP.len(), "empty chunks for the gap");
    assert_eq!(audio_chunks.len(), b.audio_chunks);
    // Keyframe chunks carry Annex-B SPS/PPS in-band.
    let first = video_chunks[0];
    assert!(bytes[first.start..first.start + 4] == [0, 0, 0, 1]);
    let nal_types: Vec<u8> = openclip::mux::avc::split_annexb(&bytes[first.start..first.start + first.size])
        .iter()
        .map(|n| openclip::mux::avc::nal_type(n))
        .collect();
    assert!(nal_types.contains(&7) && nal_types.contains(&8) && nal_types.contains(&5), "{nal_types:?}");
    let ix = data.iter().filter(|c| &c.id == b"ix00").count();
    assert_eq!(ix, 1, "standard index inside movi");

    let idx1 = find(&top, b"idx1", None).expect("idx1");
    let n = idx1.size / 16;
    assert_eq!(n, total_slots + b.audio_chunks);
    let mut vi = 0;
    for e in 0..n {
        let at = idx1.start + e * 16;
        let id = &bytes[at..at + 4];
        let flags = u32(bytes, at + 4);
        let off = u32(bytes, at + 8) as usize;
        let size = u32(bytes, at + 12) as usize;
        let header = movi_fourcc_pos + off;
        assert_eq!(&bytes[header..header + 4], id, "idx1 offset points at its chunk");
        assert_eq!(u32(bytes, header + 4) as usize, size);
        if id == b"00dc" {
            let expected_key = if size == 0 { false } else { b.keyframes[vi] };
            if size > 0 {
                vi += 1;
            }
            assert_eq!(flags & 0x10 != 0, expected_key, "keyframe flag for chunk {e}");
        }
    }
    assert_eq!(vi, b.pushed);

    // --- ix00 entries point at chunk data ---
    let ix00 = data.iter().find(|c| &c.id == b"ix00").unwrap();
    assert_eq!(u16(bytes, ix00.start), 2);
    assert_eq!(bytes[ix00.start + 3], 1, "AVI_INDEX_OF_CHUNKS");
    let entries = u32(bytes, ix00.start + 4) as usize;
    assert_eq!(entries, total_slots);
    let base = u64(bytes, ix00.start + 12) as usize;
    assert_eq!(base, movi_fourcc_pos);
    for i in 0..entries {
        let at = ix00.start + 24 + i * 8;
        let off = u32(bytes, at) as usize;
        let size = u32(bytes, at + 4);
        assert_eq!(&bytes[base + off - 8..base + off - 4], b"00dc");
        assert_eq!(u32(bytes, base + off - 4), size & 0x7FFF_FFFF);
    }
    // Super index entry points at ix00.
    assert_eq!(u64(bytes, indx.start + 24) as usize, ix00.start - 8);
}

#[test]
fn pcm_audio_and_avix_continuation() {
    let b = build(Audio::Pcm, true);
    let bytes = &b.bytes;
    let p = parse(bytes);
    assert!(p.riffs.len() >= 2, "expected AVIX continuation RIFFs, got {}", p.riffs.len());
    let end = p.riffs.last().map(|r| r.start + r.size).unwrap();
    assert_eq!(end, bytes.len());

    let hdrl = chunks(bytes, p.hdrl_start, p.hdrl_end);
    let strls: Vec<&Chunk> = hdrl.iter().filter(|c| c.kind.as_ref() == Some(b"strl")).collect();
    let aus = chunks(bytes, strls[1].start, strls[1].start + strls[1].size);
    let astrf = find(&aus, b"strf", None).unwrap();
    assert_eq!(u16(bytes, astrf.start), 0x0001, "PCM");
    assert_eq!(u16(bytes, astrf.start + 12), 4, "block align");
    assert_eq!(u16(bytes, astrf.start + 14), 16, "bits");
    let astrh = find(&aus, b"strh", None).unwrap();
    assert_eq!(u32(bytes, astrh.start + 44), 4, "dwSampleSize = block align");
    assert_eq!(u32(bytes, astrh.start + 32) as u64, b.audio_samples, "dwLength = sample frames");

    // Every RIFF has its own ix00 and the video super index lists them all.
    let vs = chunks(bytes, strls[0].start, strls[0].start + strls[0].size);
    let indx = find(&vs, b"indx", None).unwrap();
    let n = u32(bytes, indx.start + 4) as usize;
    assert_eq!(n, p.riffs.len());
    let mut slots = 0;
    for i in 0..n {
        let at = indx.start + 24 + i * 16;
        let off = u64(bytes, at) as usize;
        let size = u32(bytes, at + 8) as usize;
        assert_eq!(&bytes[off..off + 4], b"ix00");
        assert_eq!(u32(bytes, off + 4) as usize + 8, size);
        slots += u32(bytes, at + 12) as usize;
        // The RIFF this index belongs to holds exactly that many video chunks and its own ix chunks.
        let r = &p.riffs[i];
        let inner = chunks(bytes, r.start, r.start + r.size);
        let movi = find(&inner, b"LIST", Some(b"movi")).unwrap();
        let cs = chunks(bytes, movi.start, movi.start + movi.size);
        assert_eq!(cs.iter().filter(|c| &c.id == b"00dc").count(), u32(bytes, at + 12) as usize);
        assert_eq!(cs.iter().filter(|c| &c.id == b"ix00").count(), 1);
        assert_eq!(cs.iter().filter(|c| &c.id == b"ix01").count(), 1);
    }
    let total_slots = (FRAMES - (GAP.end - GAP.start)) as usize + GAP.len();
    assert_eq!(slots, total_slots);
    // The legacy idx1 only covers the first RIFF and lives inside it.
    let top = chunks(bytes, p.riffs[0].start, p.riffs[0].start + p.riffs[0].size);
    let idx1 = find(&top, b"idx1", None).expect("idx1 in the first RIFF");
    let movi = find(&top, b"LIST", Some(b"movi")).unwrap();
    let riff0_video = chunks(bytes, movi.start, movi.start + movi.size).iter().filter(|c| &c.id == b"00dc").count();
    let riff0_audio = chunks(bytes, movi.start, movi.start + movi.size).iter().filter(|c| &c.id == b"01wb").count();
    assert_eq!(idx1.size / 16, riff0_video + riff0_audio);
    let avih = find(&hdrl, b"avih", None).unwrap();
    assert_eq!(u32(bytes, avih.start + 16) as usize, riff0_video, "avih counts the first RIFF only");
    let odml = find(&hdrl, b"LIST", Some(b"odml")).unwrap();
    let odml_children = chunks(bytes, odml.start, odml.start + odml.size);
    let dmlh = find(&odml_children, b"dmlh", None).unwrap();
    assert_eq!(u32(bytes, dmlh.start) as usize, total_slots, "dmlh counts everything");
}

#[test]
fn video_only() {
    let b = build(Audio::None, false);
    let p = parse(&b.bytes);
    let hdrl = chunks(&b.bytes, p.hdrl_start, p.hdrl_end);
    assert_eq!(hdrl.iter().filter(|c| c.kind.as_ref() == Some(b"strl")).count(), 1);
    let avih = find(&hdrl, b"avih", None).unwrap();
    assert_eq!(u32(&b.bytes, avih.start + 24), 1);
}
