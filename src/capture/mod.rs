//! Screen capture backends behind a common interface.
//!
//! * Windows: Windows.Graphics.Capture via `windows-capture` (BGRA, native fps
//!   throttling, GPU-side crop, pipelined readback into pooled buffers).
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Recycles frame buffers between the consumer and the capture backend so a
/// 1080p recording does not allocate (and page-fault) 8 MB per frame.
pub struct FramePool {
    free: Mutex<Vec<Vec<u8>>>,
    cap: usize,
}

impl FramePool {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self { free: Mutex::new(Vec::with_capacity(cap)), cap: cap.max(1) })
    }

    /// An empty buffer (capacity retained from earlier use, if any).
    pub fn take(&self) -> Vec<u8> {
        self.free.lock().unwrap().pop().unwrap_or_default()
    }

    /// Returns a buffer; surplus buffers beyond the cap are freed.
    pub fn recycle(&self, mut buf: Vec<u8>) {
        buf.clear();
        let mut free = self.free.lock().unwrap();
        if free.len() < self.cap {
            free.push(buf);
        }
    }

    pub fn available(&self) -> usize {
        self.free.lock().unwrap().len()
    }
}

#[derive(Clone)]
pub struct CaptureConfig {
    pub source: Source,
    /// Target frame rate; backends throttle to at most this rate.
    pub fps: u32,
    pub show_cursor: bool,
    /// Buffer pool shared with the consumer (Windows backend); `None` allocates per frame.
    pub pool: Option<Arc<FramePool>>,
}

impl std::fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("source", &self.source)
            .field("fps", &self.fps)
            .field("show_cursor", &self.show_cursor)
            .field("pool", &self.pool.is_some())
            .finish()
    }
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

/// Simple wall-clock frame-rate limiter (minimum gap between accepted frames).
#[allow(dead_code)]
pub(crate) struct FpsLimiter {
    min_interval: Duration,
    last: Option<Instant>,
}

#[allow(dead_code)]
impl FpsLimiter {
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1) as f64;
        // Allow a little slack so a source running at exactly `fps` is not halved.
        Self { min_interval: Duration::from_secs_f64(0.85 / fps), last: None }
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

/// Phase-locked limiter: accepts the frame closest to each tick of a fixed
/// `fps` grid instead of enforcing a minimum gap (which beats against vsync).
#[allow(dead_code)]
pub(crate) struct PhaseLimiter {
    interval: Duration,
    next_due: Option<Instant>,
}

#[allow(dead_code)]
impl PhaseLimiter {
    pub fn new(fps: u32) -> Self {
        Self { interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64), next_due: None }
    }

    pub fn accept(&mut self, now: Instant) -> bool {
        let early = self.interval / 4;
        match self.next_due {
            Some(due) if now + early < due => false,
            Some(due) => {
                let mut next = due + self.interval;
                if next + early <= now {
                    next = now + self.interval;
                }
                self.next_due = Some(next);
                true
            }
            None => {
                self.next_due = Some(now + self.interval);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_recycles_up_to_cap() {
        let pool = FramePool::new(2);
        let mut a = pool.take();
        a.extend_from_slice(&[1, 2, 3]);
        pool.recycle(a);
        pool.recycle(vec![9; 10]);
        pool.recycle(vec![9; 10]); // beyond cap → dropped
        assert_eq!(pool.available(), 2);
        let b = pool.take();
        assert!(b.is_empty(), "recycled buffers come back cleared");
        assert!(b.capacity() >= 3);
        assert_eq!(pool.available(), 1);
        pool.take();
        assert!(pool.take().is_empty());
    }

    #[test]
    fn phase_limiter_follows_grid() {
        let mut l = PhaseLimiter::new(30);
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        assert!(l.accept(ms(0)));
        assert!(!l.accept(ms(16))); // vsync frame in between
        assert!(l.accept(ms(33)));
        assert!(!l.accept(ms(50)));
        assert!(l.accept(ms(66)));
        // A long stall re-anchors instead of accepting a burst.
        assert!(l.accept(ms(500)));
        assert!(!l.accept(ms(510)));
        assert!(l.accept(ms(534)));
    }
}
