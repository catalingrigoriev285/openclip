//! Headless smoke test: records the primary monitor (with system audio if
//! available) for a few seconds and writes an MP4.
//!
//! Usage: cargo run --release --example capture_to_mp4 [-- SECONDS OUT.mp4 [--no-audio]]

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use openclip::capture::monitors::{list_monitors, list_windows};
use openclip::capture::{Rect, Source};
use openclip::pipeline::{RecordConfig, Recorder};
use openclip::video::mouse_fx::MouseFx;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| "capture.mp4".into()));
    let audio = !args.iter().any(|a| a == "--no-audio");
    let half = args.iter().any(|a| a == "--half");
    let mic = args.iter().any(|a| a == "--mic");
    let mut fx = MouseFx::default();
    if args.iter().any(|a| a == "--fx") {
        fx.cursor_size = 150;
    }

    let monitors = list_monitors()?;
    let primary = monitors.iter().find(|m| m.is_primary).or(monitors.first()).expect("no monitors");
    // --region X,Y,W,H (monitor-local physical pixels) or --window <title substring>
    let mut source = Source::Monitor { id: primary.id };
    if let Some(i) = args.iter().position(|a| a == "--region") {
        let v: Vec<u32> = args[i + 1].split(',').filter_map(|s| s.parse().ok()).collect();
        source = Source::Region {
            monitor_id: primary.id,
            rect: Rect { x: v[0], y: v[1], width: v[2], height: v[3] },
        };
    } else if let Some(i) = args.iter().position(|a| a == "--window") {
        let needle = args[i + 1].to_lowercase();
        let w = list_windows()?
            .into_iter()
            .find(|w| w.title.to_lowercase().contains(&needle))
            .ok_or_else(|| anyhow::anyhow!("no window matching '{needle}'"))?;
        println!("window: {}", w.label());
        source = Source::Window { id: w.id };
    }
    println!("recording {:?} on {} for {seconds}s → {}", source, primary.label(), out.display());

    let config = RecordConfig {
        source,
        fps: 30,
        bitrate_kbps: 6000,
        half_resolution: half,
        mouse_fx: fx,
        system_audio: audio,
        microphone: mic.then_some(None),
        output: out.clone(),
    };
    let recorder = Recorder::start(config, None)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(500));
        let s = recorder.stats();
        println!(
            "  {:>4.1}s captured {} encoded {} dropped {} skipped {} repeated {} enc {:.1}ms audio {} bytes {}",
            start.elapsed().as_secs_f64(),
            s.frames_captured.load(Ordering::Relaxed),
            s.frames_encoded.load(Ordering::Relaxed),
            s.frames_dropped.load(Ordering::Relaxed),
            s.frames_skipped.load(Ordering::Relaxed),
            s.frames_repeated.load(Ordering::Relaxed),
            s.encode_us.load(Ordering::Relaxed) as f64 / 1000.0,
            s.audio_frames.load(Ordering::Relaxed),
            s.bytes_written.load(Ordering::Relaxed),
        );
        if let Some(e) = s.error() {
            println!("error: {e}");
            break;
        }
        if let Some(n) = recorder.stats().audio_note.lock().unwrap().as_ref() {
            println!("  note: {n}");
        }
    }
    let path = recorder.stop()?;
    let size = std::fs::metadata(&path)?.len();
    println!("done: {} ({} KiB)", path.display(), size / 1024);
    Ok(())
}
