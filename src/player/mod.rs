//! Playback of finished media files for the library viewer.
//!
//! [`Player`] is a handle over a background worker that decodes one file and,
//! when the file has an audio track, plays it through the default output
//! device. Decoding goes through Media Foundation, so real playback is
//! Windows-only; everywhere else a player reports
//! [`PlaybackState::Unsupported`] and the GUI offers the system player instead.
//!
//! The handle itself holds no COM object and no `cpal::Stream` — both are
//! thread-affine and stay on the worker — so `Player` is `Send + Sync` and the
//! GUI thread can own it freely.
//!
//! Still pictures do not need any of this; [`load_image`] decodes them with the
//! `image` crate on every platform.

#[cfg(windows)]
mod decode;
pub mod output;
#[cfg(windows)]
mod worker;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::capture::FramePool;
use crate::video::preview::PreviewImage;
use crate::video::{PixelFormat, RawFrame};

/// Longest side the decoder is allowed to hand back. A 4K frame is 33 MB of
/// RGBA, and the viewer never shows more than the window holds.
pub const MAX_DECODE_SIDE: u32 = 3840;

/// Longest side asked for during playback.
///
/// The viewer's media area is a few hundred points across, so a 4K file was
/// costing 33 MB a frame — copied, swizzled, turned into a `ColorImage` and
/// uploaded — to be drawn into a fraction of that. Media Foundation's advanced
/// video processor does the resize on the way out of the decoder, where it is
/// nearly free, and every pass afterwards shrinks with it. Snapshots do not go
/// through here: [`frame_at`] re-reads the one frame at [`MAX_DECODE_SIDE`] so
/// what gets saved is still full resolution.
pub const MAX_PLAYBACK_SIDE: u32 = 1920;

/// Frame rate assumed when the file does not declare one.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_nanos(33_366_667);

/// Frames the queue between the worker and the GUI holds.
const FRAME_QUEUE: usize = 2;

/// Most frames the worker may hold back when that queue is full.
pub(crate) const MAX_HELD: usize = 8;

/// Buffers kept for reuse. Everything that can be alive at once: the queue, the
/// frame on screen, the one held back until it is due, and the worker's
/// backlog. A pool smaller than the working set hands back empty `Vec`s exactly
/// when playback is busiest, and every miss is a multi-megabyte allocation.
const POOL_SIZE: usize = FRAME_QUEUE + 2 + MAX_HELD;

/// What the player is doing. Everything the GUI draws hangs off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    /// The worker is opening the file; no frame yet.
    Opening,
    Playing,
    Paused,
    /// Every stream reached its end and the audio ring drained.
    Ended,
    /// The file could not be opened or decoded; see [`Player::error`].
    Failed,
    /// This platform has no in-app decoder.
    Unsupported,
}

impl PlaybackState {
    fn from_u8(v: u8) -> PlaybackState {
        match v {
            1 => PlaybackState::Playing,
            2 => PlaybackState::Paused,
            3 => PlaybackState::Ended,
            4 => PlaybackState::Failed,
            5 => PlaybackState::Unsupported,
            _ => PlaybackState::Opening,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            PlaybackState::Opening => 0,
            PlaybackState::Playing => 1,
            PlaybackState::Paused => 2,
            PlaybackState::Ended => 3,
            PlaybackState::Failed => 4,
            PlaybackState::Unsupported => 5,
        }
    }

    /// True once the file is known to be unplayable, so the GUI can offer the
    /// system player instead of a transport bar.
    pub fn is_dead(self) -> bool {
        matches!(self, PlaybackState::Failed | PlaybackState::Unsupported)
    }
}

/// One decoded video frame, already RGBA with alpha forced opaque.
pub struct Frame {
    pub image: PreviewImage,
    pub pts: Duration,
    /// Which seek generation produced it. Frames decoded before the latest
    /// seek are still in flight on the channel when it happens, and showing one
    /// would jump the picture somewhere the user did not ask for.
    pub(crate) seq: u64,
}

/// Commands the GUI sends to the worker. Fire and forget.
#[derive(Debug, Clone, Copy)]
enum Command {
    Play,
    Pause,
    Seek(Duration),
    /// Step `delta` frames from the frame on screen; negative steps back.
    /// The anchor travels with the command because the decoder is buffered
    /// well ahead of what the GUI is showing. Implies pause.
    Step { from: Duration, delta: i32 },
    Close,
}

/// Everything the GUI reads every frame: atomics plus two rarely-touched
/// mutexes, the same contract [`crate::pipeline::Stats`] uses.
pub(crate) struct Shared {
    state: AtomicU8,
    /// 0 when the container did not say.
    duration_us: AtomicU64,
    /// `(width << 32) | height` of the decoded video, 0 until the first frame.
    dims: AtomicU64,
    frame_interval_us: AtomicU64,
    /// Bumped on every seek; see [`Frame::seq`].
    pub(crate) seq: AtomicU64,
    has_video: AtomicBool,
    has_audio: AtomicBool,
    /// Playback gain as `f32` bits; kept across mute so unmuting restores it.
    volume: AtomicU32,
    muted: AtomicBool,
    error: Mutex<Option<String>>,
    note: Mutex<Option<String>>,
    pub(crate) clock: Clock,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            state: AtomicU8::new(PlaybackState::Opening.as_u8()),
            duration_us: AtomicU64::new(0),
            dims: AtomicU64::new(0),
            frame_interval_us: AtomicU64::new(DEFAULT_FRAME_INTERVAL.as_micros() as u64),
            seq: AtomicU64::new(0),
            has_video: AtomicBool::new(false),
            has_audio: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            error: Mutex::new(None),
            note: Mutex::new(None),
            clock: Clock::new(),
        }
    }

    pub(crate) fn set_state(&self, state: PlaybackState) {
        self.state.store(state.as_u8(), Ordering::Relaxed);
    }

    pub(crate) fn state(&self) -> PlaybackState {
        PlaybackState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub(crate) fn fail(&self, msg: impl std::fmt::Display) {
        *self.error.lock().unwrap() = Some(msg.to_string());
        self.set_state(PlaybackState::Failed);
    }

    pub(crate) fn set_note(&self, msg: impl std::fmt::Display) {
        *self.note.lock().unwrap() = Some(msg.to_string());
    }

    pub(crate) fn set_duration(&self, d: Duration) {
        self.duration_us.store(d.as_micros() as u64, Ordering::Relaxed);
    }

    pub(crate) fn set_dims(&self, w: u32, h: u32) {
        self.dims.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    }

    pub(crate) fn set_frame_interval(&self, d: Duration) {
        self.frame_interval_us.store(d.as_micros().max(1) as u64, Ordering::Relaxed);
    }

    pub(crate) fn set_tracks(&self, video: bool, audio: bool) {
        self.has_video.store(video, Ordering::Relaxed);
        self.has_audio.store(audio, Ordering::Relaxed);
    }

    /// Gain the audio callback should apply right now.
    pub(crate) fn gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }
    }
}

/// The playback clock.
///
/// With an audio track the output device is the master: its callback reports
/// how many frames it has actually *played* (silence written during an
/// underrun does not count, so the clock stalls instead of running ahead of the
/// decoder). Because that number only moves once per callback — roughly every
/// 10 ms — the position is interpolated from the instant the callback reported
/// it, clamped to one buffer so it can never overshoot the next callback.
///
/// Without audio it is a plain wall clock that stops while paused, the model
/// [`crate::pipeline`] already uses for recording pauses.
pub struct Clock {
    base_pts_us: AtomicU64,
    base_frames: AtomicU64,
    /// Device sample rate; 0 means "no audio, use the wall clock".
    rate: AtomicU32,
    latency_frames: AtomicU32,
    max_interp_us: AtomicU64,
    running: AtomicBool,
    /// Frames played and when the callback said so.
    tick: Mutex<Option<(u64, Instant)>>,
    wall_base: Mutex<Option<Instant>>,
}

impl Clock {
    fn new() -> Clock {
        Clock {
            base_pts_us: AtomicU64::new(0),
            base_frames: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            latency_frames: AtomicU32::new(0),
            max_interp_us: AtomicU64::new(50_000),
            running: AtomicBool::new(false),
            tick: Mutex::new(None),
            wall_base: Mutex::new(None),
        }
    }

    /// Switches to audio-master mode. `buffer_frames` bounds the interpolation
    /// so the clock cannot run past the next callback.
    pub fn set_audio(&self, rate: u32, buffer_frames: u32) {
        self.rate.store(rate, Ordering::Relaxed);
        let buf_us = if rate > 0 { buffer_frames as u64 * 1_000_000 / rate as u64 } else { 0 };
        self.max_interp_us.store(buf_us.clamp(5_000, 100_000), Ordering::Relaxed);
    }

    /// Anchors the clock at `pts`, with `frames_played` as the new zero.
    pub fn rebase(&self, pts: Duration, frames_played: u64) {
        self.base_pts_us.store(pts.as_micros() as u64, Ordering::Relaxed);
        self.base_frames.store(frames_played, Ordering::Relaxed);
        *self.tick.lock().unwrap() = None;
        *self.wall_base.lock().unwrap() = Some(Instant::now());
    }

    /// Reported by the audio callback. Uses `try_lock` because it runs on the
    /// device thread; missing one update costs a single callback of precision.
    pub fn tick(&self, frames_played: u64, latency_frames: u32) {
        self.latency_frames.store(latency_frames, Ordering::Relaxed);
        if let Ok(mut t) = self.tick.try_lock() {
            *t = Some((frames_played, Instant::now()));
        }
    }

    pub fn resume(&self, frames_played: u64) {
        self.base_pts_us.store(self.position().as_micros() as u64, Ordering::Relaxed);
        self.base_frames.store(frames_played, Ordering::Relaxed);
        *self.tick.lock().unwrap() = None;
        *self.wall_base.lock().unwrap() = Some(Instant::now());
        self.running.store(true, Ordering::Relaxed);
    }

    /// Stops the clock where it stands.
    pub fn freeze(&self) {
        let now = self.position();
        self.running.store(false, Ordering::Relaxed);
        self.base_pts_us.store(now.as_micros() as u64, Ordering::Relaxed);
        *self.tick.lock().unwrap() = None;
        *self.wall_base.lock().unwrap() = None;
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> Duration {
        let base = Duration::from_micros(self.base_pts_us.load(Ordering::Relaxed));
        if !self.running.load(Ordering::Relaxed) {
            return base;
        }
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            let started = *self.wall_base.lock().unwrap();
            return base + started.map(|t| t.elapsed()).unwrap_or_default();
        }
        let Some((frames, at)) = *self.tick.lock().unwrap() else {
            return base;
        };
        let played = frames
            .saturating_sub(self.base_frames.load(Ordering::Relaxed))
            .saturating_sub(self.latency_frames.load(Ordering::Relaxed) as u64);
        let heard = Duration::from_secs_f64(played as f64 / rate as f64);
        let interp = at.elapsed().min(Duration::from_micros(self.max_interp_us.load(Ordering::Relaxed)));
        base + heard + interp
    }
}

/// Handle to one playing file. Dropping it stops playback.
pub struct Player {
    shared: Arc<Shared>,
    pool: Arc<FramePool>,
    cmds: Option<Sender<Command>>,
    frames: Option<Receiver<Frame>>,
    worker: Option<JoinHandle<()>>,
    /// The frame on screen. Kept here so the snapshot button has a source and
    /// [`Player::poll`] can tell whether anything changed.
    current: Option<Frame>,
    /// A frame that arrived before its time; shown on a later poll.
    pending: Option<Frame>,
}

impl Player {
    /// Starts decoding `path`. Never fails: a file that cannot be opened simply
    /// reports [`PlaybackState::Failed`]. `repaint` is called whenever a frame
    /// or a state change lands, the same contract the capture preview uses.
    pub fn open(path: &Path, repaint: Arc<dyn Fn() + Send + Sync>) -> Player {
        let shared = Arc::new(Shared::new());
        // Works on every platform and covers the containers openclip writes, so
        // the scrubber has a length even before the decoder answers.
        if let Some(d) = crate::video::thumbnail::container_duration(path) {
            shared.set_duration(d);
        }
        let pool = FramePool::new(POOL_SIZE);

        #[cfg(windows)]
        {
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
            let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(FRAME_QUEUE);
            let worker = {
                let shared = shared.clone();
                let pool = pool.clone();
                let path = path.to_path_buf();
                std::thread::Builder::new()
                    .name("player".into())
                    .spawn(move || worker::run(&path, shared, pool, cmd_rx, frame_tx, repaint))
                    .ok()
            };
            if worker.is_none() {
                shared.fail("could not start the playback thread");
            }
            Player { shared, pool, cmds: Some(cmd_tx), frames: Some(frame_rx), worker, current: None, pending: None }
        }

        #[cfg(not(windows))]
        {
            let _ = path;
            shared.set_state(PlaybackState::Unsupported);
            repaint();
            Player { shared, pool, cmds: None, frames: None, worker: None, current: None, pending: None }
        }
    }

    pub fn state(&self) -> PlaybackState {
        self.shared.state()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// A non-fatal remark, such as playback continuing without sound.
    pub fn note(&self) -> Option<String> {
        self.shared.note.lock().unwrap().clone()
    }

    pub fn duration(&self) -> Option<Duration> {
        match self.shared.duration_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }

    pub fn position(&self) -> Duration {
        let pos = self.shared.clock.position();
        match self.duration() {
            Some(d) => pos.min(d),
            None => pos,
        }
    }

    /// Decoded frame size, `(0, 0)` until the first frame arrives.
    pub fn dimensions(&self) -> (u32, u32) {
        let packed = self.shared.dims.load(Ordering::Relaxed);
        ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
    }

    pub fn has_video(&self) -> bool {
        self.shared.has_video.load(Ordering::Relaxed)
    }

    pub fn has_audio(&self) -> bool {
        self.shared.has_audio.load(Ordering::Relaxed)
    }

    pub fn frame_interval(&self) -> Duration {
        Duration::from_micros(self.shared.frame_interval_us.load(Ordering::Relaxed).max(1))
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.shared.muted.store(m, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.state() == PlaybackState::Playing
    }

    fn send(&self, cmd: Command) {
        if let Some(tx) = &self.cmds {
            let _ = tx.send(cmd);
        }
    }

    pub fn play(&self) {
        if self.state().is_dead() {
            return;
        }
        // Replaying from the end starts over rather than doing nothing.
        if self.state() == PlaybackState::Ended {
            self.send(Command::Seek(Duration::ZERO));
        }
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn seek(&self, to: Duration) {
        if self.state().is_dead() {
            return;
        }
        let to = match self.duration() {
            Some(d) => to.min(d),
            None => to,
        };
        self.send(Command::Seek(to));
    }

    /// Steps `delta` frames from the frame on screen; negative goes back.
    /// Pauses first.
    pub fn step(&self, delta: i32) {
        if self.state().is_dead() || delta == 0 {
            return;
        }
        let from = self.current.as_ref().map(|f| f.pts).unwrap_or_else(|| self.position());
        self.send(Command::Step { from, delta });
    }

    /// Collects frames that are due against the clock, keeping only the newest,
    /// and recycles the buffers of the ones it passes over. Returns `true` when
    /// the frame on screen changed.
    pub fn poll(&mut self) -> bool {
        if self.frames.is_none() {
            return false;
        }
        // While paused (a frame step, say) the worker pushes exactly the frame
        // that should be on screen, so the clock must not gate it.
        let gate = self.shared.clock.is_running().then(|| self.shared.clock.position());
        let mut changed = false;
        loop {
            let next = match self.pending.take() {
                Some(f) => Some(f),
                None => self.frames.as_ref().and_then(|rx| rx.try_recv().ok()),
            };
            let Some(frame) = next else { break };
            // Anything decoded before the last seek is no longer wanted.
            if frame.seq != self.shared.seq.load(Ordering::Relaxed) {
                self.pool.recycle(frame.image.rgba);
                continue;
            }
            // Hold back a frame that is not due yet — unless the screen is
            // still empty, where showing something beats showing nothing.
            if let Some(now) = gate
                && frame.pts > now
                && self.current.is_some()
            {
                self.pending = Some(frame);
                break;
            }
            if let Some(old) = self.current.replace(frame) {
                self.pool.recycle(old.image.rgba);
            }
            changed = true;
        }
        changed
    }

    /// The frame currently on screen.
    pub fn frame(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    /// How long until the frame [`Player::poll`] held back becomes due.
    ///
    /// The GUI schedules its next repaint from this. Without it the only
    /// wake-up is a fixed grid, which has no phase relationship to the clock:
    /// a held-back frame then waits for whichever tick happens to come after
    /// it is due, and the picture beats against the audio.
    pub fn next_due(&self) -> Option<Duration> {
        let pending = self.pending.as_ref()?;
        if !self.shared.clock.is_running() {
            return None;
        }
        Some(pending.pts.saturating_sub(self.shared.clock.position()))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.send(Command::Close);
        // Let the channel close so a worker blocked on a full frame queue wakes.
        self.frames = None;
        self.cmds = None;
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// How many packets to spend looking for the wanted frame in [`frame_at`].
///
/// Media Foundation lands on the keyframe at or before the target, so this has
/// to cover a whole group of pictures plus the audio samples interleaved with
/// it. Generous, because giving up early would silently save the wrong frame.
#[cfg(windows)]
const SNAPSHOT_READS: u32 = 600;

/// Decodes the frame at `at`, at the source's own resolution.
///
/// Playback runs at [`MAX_PLAYBACK_SIDE`], which is what makes it smooth, but
/// the frame the viewer keeps on screen is then too small to save. This opens a
/// reader of its own for the one frame the user asked to keep, so a snapshot is
/// still full resolution.
///
/// Media Foundation is thread-affine, so this must run on a thread of its own
/// (it creates and drops its own `ComGuard`) and never on the GUI thread.
#[cfg(windows)]
pub fn frame_at(path: &Path, at: Duration) -> Result<PreviewImage> {
    // Declared first so COM outlives the reader.
    let _com = crate::video::mf::ComGuard::new();
    let mut reader = decode::Reader::open(path, MAX_DECODE_SIDE)?;
    if !reader.has_video() {
        return Err(anyhow!("the file has no video track"));
    }
    let interval = reader.video_info.map(|v| v.interval).unwrap_or(DEFAULT_FRAME_INTERVAL);
    reader.seek(at)?;
    // Everything up to half a frame before the target is pre-roll the reader
    // can skip converting — the same trick that makes seeking feel immediate.
    let from = at.saturating_sub(interval / 2);
    let pool = FramePool::new(1);
    for _ in 0..SNAPSHOT_READS {
        match reader.read(Some(from), &pool, false)? {
            decode::Packet::Video { image, .. } => return Ok(image),
            decode::Packet::End => break,
            _ => {}
        }
    }
    Err(anyhow!("no frame at that position"))
}

#[cfg(not(windows))]
pub fn frame_at(_path: &Path, _at: Duration) -> Result<PreviewImage> {
    Err(anyhow!("this platform has no in-app decoder"))
}

/// Decodes a still picture at full size. Cross-platform: the `image` crate
/// covers every format the Images tab lists.
pub fn load_image(path: &Path) -> Result<PreviewImage> {
    let img = image::ImageReader::open(path)?.with_guessed_format()?.decode()?.to_rgba8();
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return Err(anyhow!("empty image"));
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
    // `max_side` at or above the longer side makes `Previewer` a 1:1 copy, so
    // this only normalizes the alpha channel.
    Ok(crate::video::preview::make_preview(&frame, width.max(height)))
}

/// `MF_MT_FRAME_RATE` is `numerator << 32 | denominator`. Falls back to 1/30 s
/// when the file does not declare a usable rate.
pub fn frame_interval_from_rate(packed: u64) -> Duration {
    let (num, den) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    if num == 0 || den == 0 {
        return DEFAULT_FRAME_INTERVAL;
    }
    Duration::from_secs_f64(den as f64 / num as f64)
}

/// Where a backward frame step should land: `back` intervals before `current`,
/// never below zero.
pub fn step_target(current: Duration, interval: Duration, back: u32) -> Duration {
    current.saturating_sub(interval * back.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rate_unpacks_or_falls_back() {
        assert_eq!(frame_interval_from_rate((30u64 << 32) | 1), Duration::from_secs_f64(1.0 / 30.0));
        let ntsc = frame_interval_from_rate((30_000u64 << 32) | 1001);
        assert!((ntsc.as_secs_f64() - 1001.0 / 30_000.0).abs() < 1e-9);
        // Nothing usable declared: the 1/30 s default.
        for bad in [0, 1, 30u64 << 32] {
            assert_eq!(frame_interval_from_rate(bad), DEFAULT_FRAME_INTERVAL);
        }
    }

    #[test]
    fn step_target_walks_back_and_saturates() {
        let i = Duration::from_millis(40);
        assert_eq!(step_target(Duration::from_millis(200), i, 1), Duration::from_millis(160));
        assert_eq!(step_target(Duration::from_millis(200), i, 3), Duration::from_millis(80));
        // Never before the start of the file.
        assert_eq!(step_target(Duration::from_millis(20), i, 1), Duration::ZERO);
    }

    #[test]
    fn wall_clock_stops_when_frozen() {
        let c = Clock::new();
        c.rebase(Duration::from_secs(5), 0);
        c.resume(0);
        assert!(c.is_running());
        std::thread::sleep(Duration::from_millis(20));
        let running = c.position();
        assert!(running >= Duration::from_secs(5), "{running:?}");
        c.freeze();
        let frozen = c.position();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.position(), frozen, "a frozen clock must not advance");
    }

    #[test]
    fn audio_clock_counts_played_frames() {
        let c = Clock::new();
        c.set_audio(48_000, 480);
        c.rebase(Duration::from_secs(2), 0);
        c.resume(0);
        // Half a second of audio has reached the speakers.
        c.tick(24_000, 0);
        let pos = c.position();
        assert!(pos >= Duration::from_millis(2500), "{pos:?}");
        // Interpolation is bounded by one buffer (10 ms at 480/48 kHz).
        assert!(pos < Duration::from_millis(2520), "{pos:?}");
    }

    #[test]
    fn audio_clock_stalls_while_the_device_starves() {
        let c = Clock::new();
        c.set_audio(48_000, 480);
        c.rebase(Duration::ZERO, 1_000);
        c.resume(1_000);
        c.tick(1_000, 0);
        let first = c.position();
        // No further callbacks: the clock may only creep by the interpolation
        // clamp, never by the full sleep.
        std::thread::sleep(Duration::from_millis(60));
        let later = c.position();
        assert!(later - first <= Duration::from_millis(11), "{first:?} -> {later:?}");
    }

    #[test]
    fn reported_latency_is_subtracted() {
        let c = Clock::new();
        c.set_audio(48_000, 480);
        c.rebase(Duration::ZERO, 0);
        c.resume(0);
        c.tick(48_000, 4_800); // one second queued, 100 ms still in the device
        let pos = c.position();
        assert!(pos >= Duration::from_millis(900) && pos < Duration::from_millis(920), "{pos:?}");
    }
}
