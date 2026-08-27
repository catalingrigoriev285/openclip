//! Screen capture backends behind a common interface.
//!
//! * Windows: Windows.Graphics.Capture via `windows-capture` (BGRA, native fps
//!   throttling, GPU-side crop).
//! * macOS / Linux (X11): `xcap`'s video recorder for monitors, screenshot
//!   polling for single windows.
//!
//! Frames are delivered to a [`FrameSink`] callback on a backend-owned thread
//! as [`RawFrame`]s with timestamps relative to the recording epoch.

pub mod monitors;

#[cfg(windows)]
pub mod windows;
#[cfg(not(windows))]
pub mod xcap_backend;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::video::RawFrame;

/// A rectangle in physical pixels, relative to a monitor's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A whole monitor, identified by [`monitors::MonitorInfo::id`].
    Monitor { id: u32 },
    /// A single window, identified by [`monitors::WindowInfo::id`].
    Window { id: u32 },
    /// A sub-rectangle of a monitor.
    Region { monitor_id: u32, rect: Rect },
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub source: Source,
    /// Target frame rate; backends throttle to at most this rate.
    pub fps: u32,
    pub show_cursor: bool,
}

/// Receives frames; return `false` to stop the capture.
pub type FrameSink = Box<dyn FnMut(RawFrame) -> bool + Send>;

/// Handle to a running capture session.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    stopper: Option<Box<dyn FnOnce() -> Result<()> + Send>>,
}

impl CaptureHandle {
    pub(crate) fn new(stop: Arc<AtomicBool>, stopper: Box<dyn FnOnce() -> Result<()> + Send>) -> Self {
        Self { stop, stopper: Some(stopper) }
    }

    /// Signals the backend to stop and waits for its thread to finish.
    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        match self.stopper.take() {
            Some(f) => f(),
            None => Ok(()),
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(f) = self.stopper.take() {
            let _ = f();
        }
    }
}

/// Starts capturing `config.source`, delivering frames to `sink` with
/// timestamps relative to `epoch`.
pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    #[cfg(windows)]
    {
        windows::start(config, epoch, sink)
    }
    #[cfg(not(windows))]
    {
        xcap_backend::start(config, epoch, sink)
    }
}

/// Simple wall-clock frame-rate limiter shared by the backends.
pub(crate) struct FpsLimiter {
    min_interval: std::time::Duration,
    last: Option<Instant>,
}

impl FpsLimiter {
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1) as f64;
        // Allow a little slack so a source running at exactly `fps` is not halved.
        Self { min_interval: std::time::Duration::from_secs_f64(0.85 / fps), last: None }
    }

    /// Returns `true` if a frame arriving now should be kept.
    pub fn accept(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.duration_since(last) < self.min_interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}
