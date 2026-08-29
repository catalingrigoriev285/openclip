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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// A region rect the UI can change while the capture is running (dragging the
/// on-screen border). The four fields are packed into one atomic so a frame
/// can never observe a half-applied drag.
#[derive(Debug, Clone)]
pub struct LiveRect(Arc<AtomicU64>);

impl LiveRect {
    pub fn new(rect: Rect) -> Self {
        Self(Arc::new(AtomicU64::new(pack(rect))))
    }

    pub fn get(&self) -> Rect {
        unpack(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, rect: Rect) {
        self.0.store(pack(rect), Ordering::Relaxed);
    }
}

/// Monitor-local physical pixels never exceed `u16::MAX`, so a rect fits in one
/// `u64`. Larger values saturate rather than wrap; the backends clamp anyway.
fn pack(r: Rect) -> u64 {
    let f = |v: u32| v.min(u16::MAX as u32) as u64;
    (f(r.x) << 48) | (f(r.y) << 32) | (f(r.width) << 16) | f(r.height)
}

fn unpack(v: u64) -> Rect {
    Rect {
        x: (v >> 48) as u32 & 0xffff,
        y: (v >> 32) as u32 & 0xffff,
        width: (v >> 16) as u32 & 0xffff,
        height: v as u32 & 0xffff,
    }
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
    /// For [`Source::Region`]: a rect the caller can move while capture runs.
    /// `None` pins the crop to `source`'s rect for the whole session.
    pub live_region: Option<LiveRect>,
}

impl CaptureConfig {
    /// The rect the backend should crop to, as a handle it can re-read every
    /// frame. `None` for whole-monitor and window sources.
    pub(crate) fn crop(&self) -> Option<LiveRect> {
        match (&self.source, &self.live_region) {
            (Source::Region { .. }, Some(live)) => Some(live.clone()),
            (Source::Region { rect, .. }, None) => Some(LiveRect::new(*rect)),
            _ => None,
        }
    }
}

impl std::fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("source", &self.source)
            .field("fps", &self.fps)
            .field("show_cursor", &self.show_cursor)
            .field("pool", &self.pool.is_some())
            .field("live_region", &self.live_region.as_ref().map(LiveRect::get))
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

/// Floor for Windows.Graphics.Capture's minimum update interval. The value is a
/// *minimum*, so asking for exactly `1/fps` leaves no margin: at 30 fps it
/// truncates to 33.33330 ms while two vsyncs on a 60 Hz panel are 33.33333 ms,
/// which makes every second-vsync update ineligible and snaps delivery to three
/// vsyncs — 20 fps instead of 30. Same slack [`FpsLimiter`] uses, so a source
/// running at exactly `fps` is not halved.
pub(crate) fn min_update_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(0.85 / fps.max(1) as f64)
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
    fn live_rect_round_trips() {
        let r = Rect { x: 1920, y: 12, width: 1280, height: 722 };
        let live = LiveRect::new(r);
        assert_eq!(live.get(), r);
        // A clone shares the value with the backend that holds it.
        let backend = live.clone();
        let moved = Rect { x: 0, y: 0, width: 1280, height: 722 };
        live.set(moved);
        assert_eq!(backend.get(), moved);
        // Every field keeps its own 16 bits, up to the ceiling.
        let big = Rect { x: 65535, y: 65535, width: 65535, height: 65535 };
        live.set(big);
        assert_eq!(live.get(), big);
        // Out-of-range values saturate instead of wrapping into a neighbour.
        live.set(Rect { x: 70000, y: 1, width: 2, height: 3 });
        assert_eq!(live.get(), Rect { x: 65535, y: 1, width: 2, height: 3 });
    }

    #[test]
    fn min_update_interval_leaves_room_on_the_vsync_grid() {
        let vsync = Duration::from_secs_f64(1.0 / 60.0);
        // 30 fps must stay eligible on every second vsync, but not on every one.
        assert!(min_update_interval(30) < vsync * 2);
        assert!(min_update_interval(30) > vsync);
        // 60 fps on a 60 Hz panel must not be halved to 30.
        assert!(min_update_interval(60) < vsync);
        // Never divides by zero.
        assert!(min_update_interval(0) > Duration::ZERO);
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
