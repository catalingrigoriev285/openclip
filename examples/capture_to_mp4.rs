//! Headless smoke test: records the primary monitor (with system audio if
//! available) for a few seconds and writes an MP4 (or AVI).
//!
//! Usage: cargo run --release --example capture_to_mp4 [-- SECONDS OUT.mp4 [flags]]
//! Flags: --half --mic --no-audio --fx --region X,Y,W,H --window TITLE --pause-at S --resume-at S
//!        --codec openh264|h264-hw|h264-sw|hevc|hevc-sw|<label substring>  --audio mp3|aac|pcm  --avi  --fps N  --quality Q

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use openclip::capture::monitors::{list_monitors, list_windows};
use openclip::capture::{Rect, Source};
use openclip::pipeline::{RecordConfig, Recorder};
use openclip::settings::{pick_encoder, AudioCodec, Container, FormatSettings, RateControl, SizeMode, VideoCodec};
use openclip::video::available_encoders;
use openclip::video::mouse_fx::MouseFx;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let audio = !args.iter().any(|a| a == "--no-audio");
    let half = args.iter().any(|a| a == "--half");
    let mic = args.iter().any(|a| a == "--mic");
    let mut fx = MouseFx::default();
    if args.iter().any(|a| a == "--fx") {
        fx.cursor_size = 150;
    }
    let flag = |name: &str| -> Option<&String> { args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)) };

    let mut format = FormatSettings::default();
    if half {
        format.size = SizeMode::Half;
    }
    if args.iter().any(|a| a == "--avi") {
        format.container = Container::Avi;
    }
    let encoders = available_encoders();
    format.video_codec = match flag("--codec") {
        Some(name) => pick_encoder(name, &encoders)
            .ok_or_else(|| anyhow::anyhow!("no encoder matches '{name}' (see `cargo run --example list_encoders`)"))?,
        None => VideoCodec::OpenH264,
    };
    format.audio_codec = match flag("--audio").map(|s| s.as_str()) {
        Some("aac") => AudioCodec::Aac,
        Some("pcm") => AudioCodec::Pcm,
        _ => AudioCodec::Mp3,
    };
    if let Some(f) = flag("--fps").and_then(|s| s.parse().ok()) {
        format.fps = f;
    }
    if let Some(q) = flag("--quality").and_then(|s| s.parse().ok()) {
        format.rate_control = RateControl::Quality(q);
    }
    for n in format.normalize(&encoders) {
        println!("note: {n}");
    }
    let out = PathBuf::from(
        args.get(2).cloned().unwrap_or_else(|| format!("capture.{}", format.container.extension())),
    );

    let monitors = list_monitors()?;
    let primary = monitors.iter().find(|m| m.is_primary).or(monitors.first()).expect("no monitors");
    // --region X,Y,W,H (monitor-local physical pixels) or --window <title substring>
    let mut source = Source::Monitor { id: primary.id };
    if let Some(r) = flag("--region") {
        let v: Vec<u32> = r.split(',').filter_map(|s| s.parse().ok()).collect();
        source = Source::Region {
            monitor_id: primary.id,
            rect: Rect { x: v[0], y: v[1], width: v[2], height: v[3] },
        };
    } else if let Some(needle) = flag("--window") {
        let needle = needle.to_lowercase();
        let w = list_windows()?
            .into_iter()
            .find(|w| w.title.to_lowercase().contains(&needle))
            .ok_or_else(|| anyhow::anyhow!("no window matching '{needle}'"))?;
        println!("window: {}", w.label());
        source = Source::Window { id: w.id };
    }
    println!(
        "recording {:?} on {} for {seconds}s → {} ({}, {} / {})",
        source,
        primary.label(),
        out.display(),
        format.container.label(),
        format.video_codec.label(&encoders),
        format.audio_codec.label()
    );

    let config = RecordConfig {
        source,
        format,
        mouse_fx: fx,
        system_audio: audio,
        microphone: mic.then_some(None),
        output: out.clone(),
    };
    // --pause-at S --resume-at S: exercise pause/resume (wall-clock seconds).
    let flag_secs = |name: &str| -> Option<f64> { flag(name).and_then(|s| s.parse().ok()) };
    let pause_at = flag_secs("--pause-at");
    let resume_at = flag_secs("--resume-at");

    let mut recorder = Recorder::start(config, None)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(500));
        let t = start.elapsed().as_secs_f64();
        if let Some(p) = pause_at
            && t >= p
            && !recorder.is_paused()
            && resume_at.map(|r| t < r).unwrap_or(true)
        {
            recorder.pause();
            println!("  paused");
        }
        if let Some(r) = resume_at
            && t >= r
            && recorder.is_paused()
        {
            recorder.resume();
            println!("  resumed");
        }
        let s = recorder.stats();
        println!(
            "  {:>4.1}s captured {} encoded {} dropped {} skipped {} repeated {} superseded {} | enc {:.1}ms slot {:.1}ms mux {:.2}ms | audio {} bytes {}",
            start.elapsed().as_secs_f64(),
            s.frames_captured.load(Ordering::Relaxed),
            s.frames_encoded.load(Ordering::Relaxed),
            s.frames_dropped.load(Ordering::Relaxed),
            s.frames_skipped.load(Ordering::Relaxed),
            s.frames_repeated.load(Ordering::Relaxed),
            s.frames_superseded.load(Ordering::Relaxed),
            s.encode_us.load(Ordering::Relaxed) as f64 / 1000.0,
            s.slot_us.load(Ordering::Relaxed) as f64 / 1000.0,
            s.mux_us.load(Ordering::Relaxed) as f64 / 1000.0,
            s.audio_frames.load(Ordering::Relaxed),
            s.bytes_written.load(Ordering::Relaxed),
        );
        if let Some(e) = s.error() {
            println!("error: {e}");
            break;
        }
        for n in [s.note(), s.audio_note.lock().unwrap().clone()].into_iter().flatten() {
            println!("  note: {n}");
        }
    }
    let recorded = recorder.elapsed();
    let path = recorder.stop()?;
    let size = std::fs::metadata(&path)?.len();
    println!("done: {} ({} KiB), recorded time {:.2}s", path.display(), size / 1024, recorded.as_secs_f64());
    Ok(())
}
