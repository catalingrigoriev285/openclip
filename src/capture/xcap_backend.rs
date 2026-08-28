//! Portable backend (macOS, Linux/X11) built on `xcap`.
//!
//! Monitors and regions use `Monitor::video_recorder()`; single windows fall
//! back to polling `Window::capture_image()` at the requested frame rate since
//! xcap does not record windows yet.
//!
//! xcap's `Monitor`, `Window` and `VideoRecorder` are not `Send` on macOS
//! (they wrap Objective-C objects), so every xcap object is created and used
//! on the capture thread itself; only plain ids cross the thread boundary.
//! Setup errors are reported back through a channel so `start` still fails
//! synchronously.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use xcap::{Monitor, Window};

use super::{CaptureConfig, CaptureHandle, FpsLimiter, FrameSink, Rect, Source};
use crate::video::{PixelFormat, RawFrame};

pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = match &config.source {
        Source::Monitor { id } => spawn_monitor(*id, None, config.fps, epoch, stop.clone(), sink)?,
        Source::Region { monitor_id, rect } => {
            spawn_monitor(*monitor_id, Some(*rect), config.fps, epoch, stop.clone(), sink)?
        }
        Source::Window { id } => spawn_window(*id, config.fps, epoch, stop.clone(), sink)?,
    };
    let thread = Arc::new(Mutex::new(Some(thread)));
    let stopper = {
        let stop = stop.clone();
        Box::new(move || -> Result<()> {
            stop.store(true, Ordering::SeqCst);
            let handle = thread.lock().unwrap().take();
            match handle {
                Some(h) => h.join().map_err(|_| anyhow!("capture thread panicked"))?,
                None => Ok(()),
            }
        })
    };
    Ok(CaptureHandle::new(stop, stopper))
}

fn find_monitor(id: u32) -> Result<Monitor> {
    Monitor::all()?
        .into_iter()
        .find(|m| m.id().map(|i| i == id).unwrap_or(false))
        .ok_or_else(|| anyhow!("monitor {id} not found"))
}

fn spawn_monitor(
    id: u32,
    crop: Option<Rect>,
    fps: u32,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    mut sink: FrameSink,
) -> Result<JoinHandle<Result<()>>> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let thread = std::thread::Builder::new().name("openclip-capture".into()).spawn(move || {
        let setup: Result<_> = (|| {
            let monitor = find_monitor(id)?;
            let (recorder, rx) = monitor.video_recorder().context("creating xcap video recorder")?;
            recorder.start().context("starting xcap video recorder")?;
            Ok((recorder, rx))
        })();
        let (recorder, rx) = match setup {
            Ok(v) => {
                let _ = ready_tx.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return Ok(());
            }
        };
        let mut limiter = FpsLimiter::new(fps);
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(frame) => {
                    let now = Instant::now();
                    if !limiter.accept(now) {
                        continue;
                    }
                    if frame.raw.len() < (frame.width * frame.height * 4) as usize {
                        log::warn!("xcap frame has unexpected size, skipping");
                        continue;
                    }
                    let mut raw = RawFrame {
                        data: frame.raw,
                        width: frame.width,
                        height: frame.height,
                        stride: frame.width * 4,
                        format: PixelFormat::Rgba,
                        pts: now.duration_since(epoch),
                        mouse: None,
                    };
                    if let Some(r) = crop {
                        raw = raw.crop(r.x, r.y, r.width, r.height);
                    }
                    if !sink(raw) {
                        break Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break Err(anyhow!("xcap recorder stopped unexpectedly")),
            }
        };
        let _ = recorder.stop();
        result
    })?;
    wait_ready(ready_rx, thread)
}

/// Waits for the capture thread to report the outcome of its setup, joining
/// it (and returning its error) if setup failed.
fn wait_ready(
    ready_rx: mpsc::Receiver<Result<()>>,
    thread: JoinHandle<Result<()>>,
) -> Result<JoinHandle<Result<()>>> {
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("capture thread exited during setup"))
        }
    }
}

fn spawn_window(
    id: u32,
    fps: u32,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    mut sink: FrameSink,
) -> Result<JoinHandle<Result<()>>> {
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let thread = std::thread::Builder::new().name("openclip-capture".into()).spawn(move || {
        let setup: Result<_> = (|| {
            Window::all()?
                .into_iter()
                .find(|w| w.id().map(|i| i == id).unwrap_or(false))
                .ok_or_else(|| anyhow!("window {id} not found"))
        })();
        let window = match setup {
            Ok(w) => {
                let _ = ready_tx.send(Ok(()));
                w
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return Ok(());
            }
        };
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            next += interval;
            if next < Instant::now() {
                next = Instant::now();
            }
            let img = match window.capture_image() {
                Ok(img) => img,
                Err(e) => {
                    log::warn!("window capture failed: {e}");
                    continue;
                }
            };
            let (w, h) = (img.width(), img.height());
            let raw = RawFrame {
                data: img.into_raw(),
                width: w,
                height: h,
                stride: w * 4,
                format: PixelFormat::Rgba,
                pts: Instant::now().duration_since(epoch),
                mouse: None,
            };
            if !sink(raw) {
                break;
            }
        }
        Ok(())
    })?;
    wait_ready(ready_rx, thread)
}
