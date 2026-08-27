//! Round-trips a small A+V recording through the in-house muxer and validates
//! the result with the independent `mp4-atom` parser.

use std::cell::RefCell;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::time::Duration;

use mp4_atom::{Any, Codec, FourCC, ReadFrom, StszSamples};
use openclip::audio::Mp3Encoder;
use openclip::mux::{AudioTrackConfig, Mp4Writer, VideoTrackConfig, VIDEO_TIMESCALE};
use openclip::video::{Converter, H264Encoder, PixelFormat, RawFrame};

const W: u32 = 320;
const H: u32 = 180;
const FPS: u32 = 30;
const FRAMES: u32 = 75;
const RATE: u32 = 48_000;

/// In-memory writer we can still read after the muxer consumed it.
#[derive(Clone)]
struct Shared(Rc<RefCell<Cursor<Vec<u8>>>>);

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

struct Built {
    bytes: Vec<u8>,
    video_samples: usize,
    keyframes: Vec<bool>,
    audio_samples: usize,
}

fn build(with_audio: bool) -> Built {
    let shared = Shared(Rc::new(RefCell::new(Cursor::new(Vec::new()))));
    let mut converter = Converter::new(W, H).unwrap();
    let mut video = H264Encoder::new(FPS as f32, 800_000).unwrap();
    let mut audio = Mp3Encoder::new(RATE, 2, 128).unwrap();
    let audio_cfg = with_audio.then(|| AudioTrackConfig {
        sample_rate: RATE,
        channels: 2,
        bitrate_bps: audio.bitrate_bps(),
        samples_per_frame: audio.samples_per_frame(),
    });
    let mut mux = Mp4Writer::new(
        shared.clone(),
        Some(VideoTrackConfig { width: W, height: H, fps: FPS as f64 }),
        audio_cfg,
    )
    .unwrap();

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
    let mut video_samples = 0;
    let mut audio_samples = 0;
    let mut pcm = Vec::new();
    for i in 0..FRAMES {
        for (n, px) in frame.data.chunks_exact_mut(4).enumerate() {
            let x = (n as u32 % W + i * 3) % W;
            px[0] = (x * 255 / W) as u8;
            px[1] = ((n as u32 / W) * 255 / H) as u8;
            px[2] = if (x / 32).is_multiple_of(2) { 200 } else { 30 };
            px[3] = 255;
        }
        frame.pts = Duration::from_secs_f64(i as f64 / FPS as f64);
        converter.convert(&frame).unwrap();
        let enc = video.encode(&converter.yuv(), frame.pts).unwrap();
        if let (Some(sps), Some(pps)) = (video.sps(), video.pps()) {
            mux.set_parameter_sets(sps, pps);
        }
        if let Some(f) = enc {
            assert!(f.data.len() > 4);
            mux.push_video(&f.data, f.pts, f.keyframe).unwrap();
            keyframes.push(f.keyframe);
            video_samples += 1;
        }
        if with_audio {
            pcm.clear();
            for s in 0..(RATE / FPS) {
                let t = (i * (RATE / FPS) + s) as f32 / RATE as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                pcm.push(v);
                pcm.push(v);
            }
            for f in audio.encode(&pcm).unwrap() {
                mux.push_audio(&f.data).unwrap();
                audio_samples += 1;
            }
        }
    }
    if with_audio {
        for f in audio.flush().unwrap() {
            mux.push_audio(&f.data).unwrap();
            audio_samples += 1;
        }
    }
    mux.finalize().unwrap();
    let bytes = shared.0.borrow().get_ref().clone();
    Built { bytes, video_samples, keyframes, audio_samples }
}

fn parse(bytes: &[u8]) -> Vec<Any> {
    let mut cursor = Cursor::new(bytes);
    let mut atoms = Vec::new();
    while let Some(atom) = <Option<Any> as ReadFrom>::read_from(&mut cursor).unwrap() {
        atoms.push(atom);
    }
    atoms
}

fn sample_sizes(s: &StszSamples) -> Vec<u32> {
    match s {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    }
}

/// Number of samples in each chunk according to stsc.
fn chunk_sample_counts(stbl: &mp4_atom::Stbl, chunk_count: usize) -> Vec<u32> {
    (0..chunk_count)
        .map(|ci| {
            let chunk_no = ci as u32 + 1;
            stbl.stsc
                .entries
                .iter()
                .rev()
                .find(|e| e.first_chunk <= chunk_no)
                .map(|e| e.samples_per_chunk)
                .expect("stsc covers chunk")
        })
        .collect()
}

#[test]
fn video_and_audio_roundtrip() {
    let b = build(true);
    let bytes = &b.bytes;
    assert!(b.video_samples >= FRAMES as usize - 2, "video samples {}", b.video_samples);
    assert!(b.audio_samples > 50, "audio samples {}", b.audio_samples);
    assert!(b.keyframes[0], "first frame must be a keyframe");

    let atoms = parse(bytes);
    assert_eq!(atoms.len(), 3, "top-level boxes: {atoms:?}");
    assert!(matches!(atoms[0], Any::Ftyp(_)));
    let mdat_len = match &atoms[1] {
        Any::Mdat(m) => m.data.len(),
        other => panic!("expected mdat, got {other:?}"),
    };
    assert!(mdat_len > 10_000);
    let moov = match &atoms[2] {
        Any::Moov(m) => m,
        other => panic!("expected moov, got {other:?}"),
    };
    assert_eq!(moov.trak.len(), 2);
    assert_eq!(moov.mvhd.timescale, 1000);
    let expected_ms = (FRAMES as u64 * 1000) / FPS as u64;
    assert!(
        (moov.mvhd.duration as i64 - expected_ms as i64).abs() <= 40,
        "movie duration {}",
        moov.mvhd.duration
    );

    // --- video track ---
    let vtrak = &moov.trak[0];
    assert_eq!(vtrak.tkhd.track_id, 1);
    assert_eq!(vtrak.mdia.hdlr.handler, FourCC::new(b"vide"));
    assert_eq!(vtrak.mdia.mdhd.timescale, VIDEO_TIMESCALE);
    let stbl = &vtrak.mdia.minf.stbl;
    let avc1 = match &stbl.stsd.codecs[0] {
        Codec::Avc1(a) => a,
        other => panic!("expected avc1, got {other:?}"),
    };
    assert_eq!((avc1.visual.width as u32, avc1.visual.height as u32), (W, H));
    assert_eq!(avc1.avcc.length_size, 4);
    assert_eq!(avc1.avcc.sequence_parameter_sets.len(), 1);
    assert_eq!(avc1.avcc.picture_parameter_sets.len(), 1);
    assert_eq!(avc1.avcc.sequence_parameter_sets[0][0] & 0x1F, 7);
    assert_eq!(avc1.avcc.picture_parameter_sets[0][0] & 0x1F, 8);

    let sizes = sample_sizes(&stbl.stsz.samples);
    assert_eq!(sizes.len(), b.video_samples);
    let total_stts: u32 = stbl.stts.entries.iter().map(|e| e.sample_count).sum();
    assert_eq!(total_stts as usize, b.video_samples);
    for e in &stbl.stts.entries {
        assert!((e.sample_delta as i64 - 3000).abs() <= 1, "delta {}", e.sample_delta);
    }
    let stss = stbl.stss.as_ref().expect("stss present");
    let expected_keys: Vec<u32> =
        b.keyframes.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i as u32 + 1).collect();
    assert_eq!(stss.entries, expected_keys);

    // Walk chunks and verify every sample is a sequence of length-prefixed NALs.
    let stco = stbl.stco.as_ref().expect("stco present");
    let counts = chunk_sample_counts(stbl, stco.entries.len());
    let mut sample_idx = 0usize;
    for (ci, off) in stco.entries.iter().enumerate() {
        let mut pos = *off as usize;
        for _ in 0..counts[ci] {
            let size = sizes[sample_idx] as usize;
            let mut p = pos;
            while p < pos + size {
                let len = u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
                assert!(
                    len > 0 && p + 4 + len <= pos + size,
                    "bad NAL length {len} in sample {sample_idx}"
                );
                let nal_type = bytes[p + 4] & 0x1F;
                assert!(matches!(nal_type, 1 | 5 | 6 | 9), "unexpected NAL type {nal_type}");
                p += 4 + len;
            }
            pos += size;
            sample_idx += 1;
        }
    }
    assert_eq!(sample_idx, b.video_samples);

    // --- audio track ---
    let atrak = &moov.trak[1];
    assert_eq!(atrak.tkhd.track_id, 2);
    assert_eq!(atrak.mdia.hdlr.handler, FourCC::new(b"soun"));
    assert_eq!(atrak.mdia.mdhd.timescale, RATE);
    let astbl = &atrak.mdia.minf.stbl;
    let mp4a = match &astbl.stsd.codecs[0] {
        Codec::Mp4a(a) => a,
        other => panic!("expected mp4a, got {other:?}"),
    };
    assert_eq!(mp4a.audio.channel_count, 2);
    assert_eq!(mp4a.esds.es_desc.dec_config.object_type_indication, 0x6B);
    assert_eq!(mp4a.esds.es_desc.dec_config.stream_type, 0x05);
    assert_eq!(astbl.stts.entries.len(), 1);
    assert_eq!(astbl.stts.entries[0].sample_delta, 1152);
    assert_eq!(astbl.stts.entries[0].sample_count as usize, b.audio_samples);

    // Every audio sample must start with an MP3 sync word.
    let asizes = sample_sizes(&astbl.stsz.samples);
    let astco = astbl.stco.as_ref().unwrap();
    let acounts = chunk_sample_counts(astbl, astco.entries.len());
    let mut idx = 0;
    for (ci, off) in astco.entries.iter().enumerate() {
        let mut pos = *off as usize;
        for _ in 0..acounts[ci] {
            assert_eq!(bytes[pos], 0xFF, "audio sample {idx} lacks sync");
            assert_eq!(bytes[pos + 1] & 0xE0, 0xE0);
            pos += asizes[idx] as usize;
            idx += 1;
        }
    }
    assert_eq!(idx, b.audio_samples);
}

#[test]
fn video_only_roundtrip() {
    let b = build(false);
    let atoms = parse(&b.bytes);
    let moov = atoms.iter().find_map(|a| if let Any::Moov(m) = a { Some(m) } else { None }).unwrap();
    assert_eq!(moov.trak.len(), 1);
    let stbl = &moov.trak[0].mdia.minf.stbl;
    let n: u32 = stbl.stts.entries.iter().map(|e| e.sample_count).sum();
    assert_eq!(n as usize, b.video_samples);
}
