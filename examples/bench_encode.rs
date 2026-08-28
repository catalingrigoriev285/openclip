//! Encodes a synthetic moving-gradient sequence to measure encoder throughput
//! and produce a test MP4 (video + 440 Hz tone that beeps on white flashes).
//!
//! Usage: cargo run --release --example bench_encode [-- WIDTH HEIGHT SECONDS OUT.mp4 [--codec NAME]]
//! NAME: openh264 | h264-hw | h264-sw | hevc | hevc-sw | a label substring (nvenc, quick, dx12 …)

use std::fs::File;
use std::io::BufWriter;
use std::time::{Duration, Instant};

use openclip::audio::{AudioEncoder, Mp3Encoder};
use openclip::mux::{AudioTrackConfig, Mp4Writer, VideoCodecConfig, VideoTrackConfig};
use openclip::settings::{pick_encoder, VideoCodec};
use openclip::video::{available_encoders, create_video_encoder, Converter, EncoderRequest, PixelFormat, RawFrame};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let width: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1920);
    let height: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1080);
    let seconds: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = args.get(4).cloned().unwrap_or_else(|| "bench.mp4".to_string());
    let codec = match args.iter().position(|a| a == "--codec").and_then(|i| args.get(i + 1)) {
        Some(name) => pick_encoder(name, &available_encoders())
            .ok_or_else(|| anyhow::anyhow!("no encoder matches '{name}' (see `cargo run --example list_encoders`)"))?,
        None => VideoCodec::OpenH264,
    };
    let fps = 30u32;
    let sample_rate = 48_000u32;

    let req = EncoderRequest { codec, ..EncoderRequest::simple(width, height, fps, 6_000_000) };
    let (mut video, note) = create_video_encoder(&req)?;
    if let Some(n) = note {
        println!("note: {n}");
    }
    println!("encoder: {}", video.describe());
    let mut converter = Converter::new(width, height, video.input_layout())?;
    let mut audio = Mp3Encoder::new(sample_rate, 2, 160)?;
    let mut mux = Mp4Writer::new(
        BufWriter::new(File::create(&out)?),
        Some(VideoTrackConfig {
            width,
            height,
            fps: fps as f64,
            codec: if video.is_hevc() { VideoCodecConfig::Hevc } else { VideoCodecConfig::H264 },
        }),
        Some(AudioTrackConfig {
            sample_rate,
            channels: 2,
            bitrate_bps: audio.bitrate_bps(),
            samples_per_frame: audio.samples_per_frame(),
            codec: audio.codec_config(),
        }),
    )?;

    let total = fps * seconds;
    let mut frame = RawFrame {
        data: vec![0u8; (width * height * 4) as usize],
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Bgra,
        pts: Duration::ZERO,
        mouse: None,
    };
    let mut convert_time = Duration::ZERO;
    let mut encode_time = Duration::ZERO;
    let start = Instant::now();
    let mut pcm = Vec::new();
    let mut audio_pos = 0u64; // samples generated so far

    let push = |mux: &mut Mp4Writer<BufWriter<File>>, f: &openclip::video::EncodedFrame| -> anyhow::Result<()> {
        mux.push_video(&f.data, f.pts, f.keyframe)
    };

    for i in 0..total {
        let t = i as f32 / fps as f32;
        let flash = i % fps == 0; // one white flash per second
        fill_gradient(&mut frame, t, flash);
        frame.pts = Duration::from_secs_f64(i as f64 / fps as f64);

        let c0 = Instant::now();
        converter.convert(&frame)?;
        convert_time += c0.elapsed();

        let e0 = Instant::now();
        let encoded = video.encode(converter.frame(), frame.pts)?;
        encode_time += e0.elapsed();

        if let Some(p) = video.codec_params() {
            mux.set_codec_params(p);
        }
        for f in &encoded {
            push(&mut mux, f)?;
        }

        // Audio for this frame interval: 100 ms beep at each flash.
        let target = ((i as u64 + 1) * sample_rate as u64) / fps as u64;
        pcm.clear();
        while audio_pos < target {
            let ts = audio_pos as f32 / sample_rate as f32;
            let in_beep = ts.fract() < 0.1;
            let v = if in_beep { (ts * 440.0 * std::f32::consts::TAU).sin() * 0.5 } else { 0.0 };
            pcm.push(v);
            pcm.push(v);
            audio_pos += 1;
        }
        for mp3 in AudioEncoder::encode(&mut audio, &pcm)? {
            mux.push_audio(&mp3.data)?;
        }
    }
    for f in video.flush()? {
        push(&mut mux, &f)?;
    }
    for mp3 in AudioEncoder::flush(&mut audio)? {
        mux.push_audio(&mp3.data)?;
    }
    let elapsed = start.elapsed();
    let bytes = mux.bytes_written();
    mux.finalize()?;

    println!(
        "{}x{} {} frames in {:.2}s → {:.1} fps (convert {:.1} ms/frame, encode {:.1} ms/frame), {} KiB → {}",
        width,
        height,
        total,
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        convert_time.as_secs_f64() * 1000.0 / total as f64,
        encode_time.as_secs_f64() * 1000.0 / total as f64,
        bytes / 1024,
        out
    );
    Ok(())
}

fn fill_gradient(frame: &mut RawFrame, t: f32, flash: bool) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let shift = (t * 200.0) as usize;
    for y in 0..h {
        let row = &mut frame.data[y * frame.stride as usize..][..w * 4];
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if flash {
                *px = [255, 255, 255, 255];
                continue;
            }
            let xx = (x + shift) % w;
            px[0] = (xx * 255 / w) as u8; // B
            px[1] = (y * 255 / h) as u8; // G
            px[2] = (((x / 64) + (y / 64)) % 2 * 200) as u8; // R checkerboard
            px[3] = 255;
        }
    }
}
