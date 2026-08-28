//! Recording pipeline: capture thread → encode thread (video encoder + muxer),
//! plus an audio thread (cpal → mixer → audio encoder) feeding the muxer.
//!
//! Video runs on a **fixed-cadence frame clock**: slot `k` has presentation
//! time `pts0 + k / fps`. Captured frames are collected until a slot's
//! deadline, the newest one is encoded with the slot's timestamp (or the last
//! frame is repeated when nothing new arrived), so the file always has a
//! perfectly regular frame rate whatever the capture timing does. Frame
//! buffers are pooled, mouse effects are painted in place and restored, and
//! previews are only produced while the GUI shows them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::audio::capture::{open_microphone, open_system_loopback, AudioSource};
use crate::audio::mixer::Mixer;
use crate::audio::{create_audio_encoder, AudioFrame};
use crate::capture::monitors::source_origin;
use crate::capture::{self, CaptureConfig, CaptureHandle, FramePool, Source};
use crate::mux::{AudioTrackConfig, Muxer, VideoCodecConfig, VideoTrackConfig};
use crate::settings::{FormatSettings, SizeMode};
use crate::video::convert::even_dims;
use crate::video::mouse_fx::{FrameClick, MouseFx, MouseSampler, Patch};
use crate::video::preview::{PreviewImage, Previewer};
use crate::video::{create_video_encoder, Converter, EncodedFrame, EncoderRequest, PixelFormat, RawFrame, Scaler, VideoEncoder};

/// Longest side of preview images handed to the GUI.
const PREVIEW_MAX_SIDE: u32 = 640;
/// Minimum time between two preview images.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(200);
/// Audio is mixed this far behind wall-clock so device latency never causes gaps.
const AUDIO_LAG: Duration = Duration::from_millis(150);
/// Frame buffers kept in circulation (channel + in flight + last frame).
const POOL_SIZE: usize = 6;

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub source: Source,
    pub format: FormatSettings,
    pub mouse_fx: MouseFx,
    pub system_audio: bool,
    /// `Some(None)` = default microphone, `Some(Some(name))` = named device.
    pub microphone: Option<Option<String>>,
    pub output: PathBuf,
}

impl RecordConfig {
    pub fn wants_audio(&self) -> bool {
        self.system_audio || self.microphone.is_some()
    }

    pub fn fps(&self) -> u32 {
        self.format.fps.max(1)
    }
}

/// Live counters, readable from the GUI.
#[derive(Debug, Default)]
pub struct Stats {
    pub frames_captured: AtomicU64,
    pub frames_encoded: AtomicU64,
    /// Frames lost: capture channel full, or slots skipped because encoding fell behind.
    pub frames_dropped: AtomicU64,
    /// Frames the encoder chose not to output (rate control) or the container could not place.
    pub frames_skipped: AtomicU64,
    /// Slots filled by repeating the previous frame (static screen or late capture).
    pub frames_repeated: AtomicU64,
    /// Captured frames replaced by a newer one before their slot came (source faster than fps).
    pub frames_superseded: AtomicU64,
    /// Rolling average encode time per frame, in microseconds.
    pub encode_us: AtomicU64,
    /// Rolling average of the whole per-slot work (scale + effects + convert + encode + mux).
    pub slot_us: AtomicU64,
    /// Rolling average time spent writing to the container per frame.
    pub mux_us: AtomicU64,
    pub audio_frames: AtomicU64,
    pub bytes_written: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub audio_note: Mutex<Option<String>>,
    /// Encoder / container notes (e.g. a hardware encoder fell back to OpenH264).
    pub note: Mutex<Option<String>>,
}

impl Stats {
    fn set_error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        log::error!("{msg}");
        let mut e = self.error.lock().unwrap();
        if e.is_none() {
            *e = Some(msg);
        }
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn add_note(&self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!("{msg}");
        let mut n = self.note.lock().unwrap();
        match n.as_mut() {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&msg);
            }
            None => *n = Some(msg),
        }
    }

    pub fn note(&self) -> Option<String> {
        self.note.lock().unwrap().clone()
    }

    fn rolling(cell: &AtomicU64, sample_us: u64) {
        let prev = cell.load(Ordering::Relaxed);
        let next = if prev == 0 { sample_us } else { (prev * 9 + sample_us) / 10 };
        cell.store(next, Ordering::Relaxed);
    }
}

/// Shared slot for the latest preview image.
#[derive(Default)]
pub struct PreviewSlot {
    pub image: Mutex<Option<PreviewImage>>,
}

impl PreviewSlot {
    pub fn take(&self) -> Option<PreviewImage> {
        self.image.lock().unwrap().take()
    }
}

/// Called whenever a new preview image is available (e.g. to request a repaint).
pub type PreviewCallback = Arc<dyn Fn() + Send + Sync>;

/// Raises the system timer resolution to 1 ms while recording so the frame
/// clock and the encoder polling wake up on time (Windows defaults to 15.6 ms).
struct TimerResolution;

impl TimerResolution {
    fn acquire() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Media::timeBeginPeriod;
            use windows::Win32::System::Threading::{
                GetCurrentProcess, SetProcessInformation, ProcessPowerThrottling, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            };
            // Windows 11 ignores the request for occluded / minimized windows unless told otherwise.
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0,
            };
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// Total paused wall time in microseconds (grows only at resume).
    paused_total_us: Arc<AtomicU64>,
    pause_started: Option<Instant>,
    capture: Option<CaptureHandle>,
    encode_thread: Option<JoinHandle<Result<()>>>,
    audio_thread: Option<JoinHandle<Result<()>>>,
    stats: Arc<Stats>,
    preview: Arc<PreviewSlot>,
    preview_visible: Arc<AtomicBool>,
    started: Instant,
    output: PathBuf,
    _timer: TimerResolution,
}

impl Recorder {
    pub fn start(config: RecordConfig, on_preview: Option<PreviewCallback>) -> Result<Recorder> {
        let timer = TimerResolution::acquire();
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let paused_total_us = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(Stats::default());
        let preview = Arc::new(PreviewSlot::default());
        let preview_visible = Arc::new(AtomicBool::new(false));
        // Timeline origin (first video pts) shared with the audio thread, in µs; u64::MAX = unknown.
        let audio_origin = Arc::new(AtomicU64::new(u64::MAX));
        let pool = FramePool::new(POOL_SIZE);

        let (video_tx, video_rx) = mpsc::sync_channel::<RawFrame>(4);
        let (audio_tx, audio_rx) = mpsc::sync_channel::<AudioFrame>(256);

        let (audio_thread, audio_cfg) = if config.wants_audio() {
            let (handle, cfg) = spawn_audio_thread(AudioArgs {
                config: config.clone(),
                epoch,
                stop: stop.clone(),
                paused: paused.clone(),
                paused_total: paused_total_us.clone(),
                origin: audio_origin.clone(),
                tx: audio_tx,
                stats: stats.clone(),
            })?;
            (Some(handle), Some(cfg))
        } else {
            drop(audio_tx);
            (None, None)
        };

        let sampler = config.mouse_fx.any_overlay().then(|| Arc::new(Mutex::new(MouseSampler::new())));
        let encode_thread = spawn_encode_thread(EncodeArgs {
            config: config.clone(),
            epoch,
            video_rx,
            audio_rx,
            audio_cfg,
            audio_origin,
            stop: stop.clone(),
            paused: paused.clone(),
            paused_total: paused_total_us.clone(),
            stats: stats.clone(),
            preview: preview.clone(),
            preview_visible: preview_visible.clone(),
            on_preview,
            sampler: sampler.clone(),
            pool: pool.clone(),
        })?;

        let capture = start_capture(&config, epoch, video_tx, stats.clone(), sampler, pool)?;

        Ok(Recorder {
            stop,
            paused,
            paused_total_us,
            pause_started: None,
            capture: Some(capture),
            encode_thread: Some(encode_thread),
            audio_thread,
            stats,
            preview,
            preview_visible,
            started: epoch,
            output: config.output,
            _timer: timer,
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn preview(&self) -> &Arc<PreviewSlot> {
        &self.preview
    }

    /// Tells the encode thread whether anyone is looking at previews.
    pub fn set_preview_visible(&self, visible: bool) {
        self.preview_visible.store(visible, Ordering::Relaxed);
    }

    /// Recorded time: wall time minus everything spent paused.
    pub fn elapsed(&self) -> Duration {
        let paused = Duration::from_micros(self.paused_total_us.load(Ordering::Relaxed))
            + self.pause_started.map(|t| t.elapsed()).unwrap_or_default();
        self.started.elapsed().saturating_sub(paused)
    }

    pub fn is_paused(&self) -> bool {
        self.pause_started.is_some()
    }

    /// Pauses recording: frames and audio captured until [`Self::resume`] are discarded.
    pub fn pause(&mut self) {
        if self.pause_started.is_none() {
            self.pause_started = Some(Instant::now());
            self.paused.store(true, Ordering::SeqCst);
        }
    }

    /// Resumes recording; the paused wall time is removed from the timeline.
    pub fn resume(&mut self) {
        if let Some(t) = self.pause_started.take() {
            self.paused_total_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::SeqCst);
            self.paused.store(false, Ordering::SeqCst);
        }
    }

    pub fn output(&self) -> &PathBuf {
        &self.output
    }

    /// True if a worker thread has terminated (with or without error).
    pub fn is_finished(&self) -> bool {
        self.encode_thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    /// Stops recording and finalizes the file.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.resume();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(capture) = self.capture.take()
            && let Err(e) = capture.stop()
        {
            log::warn!("capture stop: {e:#}");
        }
        if let Some(t) = self.audio_thread.take() {
            match t.join() {
                Ok(Err(e)) => self.stats.set_error(format!("audio: {e:#}")),
                Err(_) => self.stats.set_error("audio thread panicked"),
                _ => {}
            }
        }
        if let Some(t) = self.encode_thread.take() {
            match t.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow!("encode thread panicked")),
            }
        }
        if let Some(e) = self.stats.error() {
            return Err(anyhow!(e));
        }
        Ok(self.output.clone())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.capture.take() {
            let _ = c.stop();
        }
        if let Some(t) = self.audio_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
        }
    }
}

fn start_capture(
    config: &RecordConfig,
    epoch: Instant,
    tx: SyncSender<RawFrame>,
    stats: Arc<Stats>,
    sampler: Option<Arc<Mutex<MouseSampler>>>,
    pool: Arc<FramePool>,
) -> Result<CaptureHandle> {
    let sink_pool = pool.clone();
    let sink: capture::FrameSink = Box::new(move |mut frame| {
        stats.frames_captured.fetch_add(1, Ordering::Relaxed);
        // Sample the pointer right when the frame arrives so effects line up
        // with the captured image even if encoding lags behind.
        if let Some(s) = &sampler {
            frame.mouse = Some(s.lock().unwrap().snapshot());
        }
        match tx.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(f)) => {
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                sink_pool.recycle(f.data);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    });
    capture::start(
        CaptureConfig {
            source: config.source.clone(),
            fps: config.fps(),
            show_cursor: config.mouse_fx.native_cursor(),
            pool: Some(pool),
        },
        epoch,
        sink,
    )
    .context("starting screen capture")
}

/// Fixed-cadence timeline: slot `k` is presented at `pts0 + k / fps`
/// (exact rational arithmetic, so long recordings never drift).
#[derive(Debug, Clone)]
pub struct FrameClock {
    fps: u64,
    pts0_ns: u64,
    /// Next slot to encode.
    pub next: u64,
}

const NS: u64 = 1_000_000_000;

impl FrameClock {
    /// Encoding may lag this many slots before slots are skipped.
    pub const MAX_BEHIND: u64 = 2;

    pub fn new(fps: u32, pts0: Duration) -> Self {
        Self { fps: fps.max(1) as u64, pts0_ns: pts0.as_nanos() as u64, next: 0 }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_nanos(NS / self.fps)
    }

    pub fn slot_pts(&self, k: u64) -> Duration {
        Duration::from_nanos(self.pts0_ns + k * NS / self.fps)
    }

    /// When slot `k` must be encoded: half an interval after its pts, so a
    /// frame captured anywhere around the slot time can still be used.
    pub fn deadline(&self, k: u64) -> Duration {
        Duration::from_nanos(self.pts0_ns + (2 * k + 1) * NS / (2 * self.fps))
    }

    /// Slots (from `next`) whose deadline has passed at `now`.
    pub fn due(&self, now: Duration) -> u64 {
        let now = now.as_nanos() as u64;
        if now < self.pts0_ns {
            return 0;
        }
        // Largest k with deadline(k) <= now: (2k+1)/(2 fps) <= t  ⇔  k <= (2 t fps − 1)/2.
        let twice = (now - self.pts0_ns) * 2 * self.fps / NS;
        if twice == 0 {
            return 0;
        }
        let k_max = (twice - 1) / 2;
        (k_max + 1).saturating_sub(self.next)
    }

    /// Skips slots so that at most one is due; returns how many were skipped.
    pub fn catch_up(&mut self, now: Duration) -> u64 {
        let due = self.due(now);
        if due > Self::MAX_BEHIND {
            let skipped = due - 1;
            self.next += skipped;
            skipped
        } else {
            0
        }
    }

    /// After a pause: continue with the slot nearest to `now` (never backwards).
    pub fn resync(&mut self, now: Duration) {
        let now = now.as_nanos() as u64;
        if now > self.pts0_ns {
            let k = ((now - self.pts0_ns) * self.fps + NS / 2) / NS;
            self.next = self.next.max(k);
        }
    }
}

struct EncodeArgs {
    config: RecordConfig,
    epoch: Instant,
    video_rx: Receiver<RawFrame>,
    audio_rx: Receiver<AudioFrame>,
    audio_cfg: Option<AudioTrackConfig>,
    audio_origin: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    paused_total: Arc<AtomicU64>,
    stats: Arc<Stats>,
    preview: Arc<PreviewSlot>,
    preview_visible: Arc<AtomicBool>,
    on_preview: Option<PreviewCallback>,
    /// Shared with the capture sink; used here only for repeated frames.
    sampler: Option<Arc<Mutex<MouseSampler>>>,
    pool: Arc<FramePool>,
}

fn spawn_encode_thread(args: EncodeArgs) -> Result<JoinHandle<Result<()>>> {
    let stats = args.stats.clone();
    Ok(std::thread::Builder::new().name("openclip-encode".into()).spawn(move || {
        let r = encode_loop(args);
        if let Err(e) = &r {
            stats.set_error(format!("encoding: {e:#}"));
        }
        r
    })?)
}

fn encode_loop(args: EncodeArgs) -> Result<()> {
    let EncodeArgs {
        config,
        epoch,
        video_rx,
        audio_rx,
        mut audio_cfg,
        audio_origin,
        stop,
        paused,
        paused_total,
        stats,
        preview,
        preview_visible,
        on_preview,
        sampler,
        pool,
    } = args;
    // Recording-time clock: wall time since epoch minus paused time.
    let paused_dur = || Duration::from_micros(paused_total.load(Ordering::Relaxed));
    let rec_now = || epoch.elapsed().saturating_sub(paused_dur());
    let fps = config.fps();

    let mut state: Option<EncodeState> = None;
    let mut clock: Option<FrameClock> = None;
    // Newest captured frame waiting for its slot.
    let mut newest: Option<RawFrame> = None;
    let mut origin = source_origin(&config.source).unwrap_or((0, 0));
    let mut frames_since_origin = 0u32;
    let mut was_paused = false;
    let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
    let mut last_report = Instant::now();
    let mut disconnected = false;

    let push_audio = |mux: &mut Muxer, stats: &Stats| -> Result<()> {
        if mux.has_audio() {
            while let Ok(f) = audio_rx.try_recv() {
                mux.push_audio(&f)?;
                stats.audio_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    };
    let take_newest = |newest: &mut Option<RawFrame>, f: RawFrame, pool: &FramePool, stats: &Stats| {
        if let Some(old) = newest.replace(f) {
            pool.recycle(old.data);
            stats.frames_superseded.fetch_add(1, Ordering::Relaxed);
        }
    };

    loop {
        let stopping = stop.load(Ordering::Relaxed);
        // ----- pause: discard what arrives, keep the timeline frozen -----
        if paused.load(Ordering::Relaxed) && !stopping {
            was_paused = true;
            match video_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(f) => pool.recycle(f.data),
                Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if let Some(st) = state.as_mut() {
                push_audio(&mut st.mux, &stats)?;
            }
            if disconnected {
                break;
            }
            continue;
        }
        if was_paused {
            was_paused = false;
            // Frames queued before the pause ended belong to paused time.
            while let Ok(f) = video_rx.try_recv() {
                pool.recycle(f.data);
            }
            if let Some(f) = newest.take() {
                pool.recycle(f.data);
            }
            if let Some(c) = clock.as_mut() {
                c.resync(rec_now());
            }
        }

        // ----- collect frames until the next slot deadline -----
        let Some(c) = clock.as_ref() else {
            // Waiting for the very first frame, which defines the timeline origin.
            match video_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(mut f) => {
                    f.pts = f.pts.saturating_sub(paused_dur());
                    let st = EncodeState::new(&config, &f, audio_cfg.take(), &stats)?;
                    stats.width.store(st.dims.0 as u64, Ordering::Relaxed);
                    stats.height.store(st.dims.1 as u64, Ordering::Relaxed);
                    state = Some(st);
                    clock = Some(FrameClock::new(fps, f.pts));
                    audio_origin.store(f.pts.as_micros() as u64, Ordering::SeqCst);
                    newest = Some(f);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if stopping {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            continue;
        };
        let deadline = c.deadline(c.next);
        loop {
            let now = rec_now();
            if now >= deadline || stopping {
                break;
            }
            match video_rx.recv_timeout(deadline - now) {
                Ok(mut f) => {
                    f.pts = f.pts.saturating_sub(paused_dur());
                    take_newest(&mut newest, f, &pool, &stats);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if stopping || disconnected {
            // Take whatever is still queued so the last captured frame is not lost.
            while let Ok(mut f) = video_rx.try_recv() {
                f.pts = f.pts.saturating_sub(paused_dur());
                take_newest(&mut newest, f, &pool, &stats);
            }
        }

        // ----- encode one slot -----
        let st = state.as_mut().unwrap();
        let c = clock.as_mut().unwrap();
        let now = rec_now();
        let skipped = c.catch_up(now);
        if skipped > 0 {
            stats.frames_dropped.fetch_add(skipped, Ordering::Relaxed);
        }
        let have_new = newest.is_some();
        if stopping && !have_new {
            break;
        }
        if !have_new && c.due(now) == 0 {
            // Woken early (stop during pause etc.): nothing to do yet.
            continue;
        }
        let t0 = Instant::now();
        let slot = c.next;
        let pts = c.slot_pts(slot);
        if let Some(f) = newest.take() {
            st.accept(f, &pool);
        } else {
            stats.frames_repeated.fetch_add(1, Ordering::Relaxed);
            // The screen is static but the pointer may move: sample now.
            if let (Some(s), Some(fx)) = (&sampler, st.frame.as_mut()) {
                fx.mouse = Some(s.lock().unwrap().snapshot());
            }
        }
        let snap = st.frame.as_mut().expect("frame present after accept").mouse.take();

        // Mouse effects: paint in place, convert, restore — no frame copy.
        let mut painted = false;
        if let Some(snap) = snap.filter(|_| sampler.is_some()) {
            if matches!(config.source, Source::Window { .. }) {
                frames_since_origin += 1;
                if frames_since_origin >= 30 {
                    frames_since_origin = 0;
                    if let Ok(o) = source_origin(&config.source) {
                        origin = o;
                    }
                }
            }
            let (cursor, clicks) = snap.mapped(origin, st.fx_scale);
            let changed = st.fx_changed(cursor, &clicks);
            if changed || st.converted_is_new {
                let scale = st.fx_scale.0.min(st.fx_scale.1);
                let frame = st.frame.as_mut().unwrap();
                config.mouse_fx.paint(frame, cursor, &clicks, scale, &mut st.patches);
                painted = true;
            }
            st.last_fx = Some((cursor, clicks));
        }
        if st.converted_is_new || painted {
            st.converter.convert(st.frame.as_ref().unwrap())?;
            st.converted_is_new = false;
        }
        if painted {
            MouseFx::restore(st.frame.as_mut().unwrap(), &mut st.patches);
        }
        let t1 = Instant::now();
        let encoded = st.encoder.encode(st.converter.frame(), pts)?;
        let t2 = Instant::now();
        if encoded.is_empty() {
            stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
        }
        for enc in encoded {
            st.push(&enc, &stats)?;
        }
        let t3 = Instant::now();
        c.next += 1;

        if preview_visible.load(Ordering::Relaxed) && t0.duration_since(last_preview) >= PREVIEW_INTERVAL {
            last_preview = t0;
            let img = st.previewer.make(st.frame.as_ref().unwrap());
            *preview.image.lock().unwrap() = Some(img);
            if let Some(cb) = &on_preview {
                cb();
            }
        }

        Stats::rolling(&stats.encode_us, (t2 - t1).as_micros() as u64);
        Stats::rolling(&stats.mux_us, (t3 - t2).as_micros() as u64);
        Stats::rolling(&stats.slot_us, (t3 - t0).as_micros() as u64);
        if t3 - t0 > c.interval() * 2 {
            log::warn!(
                "slow slot {slot}: prepare {:.1} ms, encode {:.1} ms, mux {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                (t3 - t2).as_secs_f64() * 1e3
            );
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            log::info!(
                "slot {slot}: captured {} encoded {} dropped {} repeated {} superseded {} | encode {:.1} ms, slot {:.1} ms",
                stats.frames_captured.load(Ordering::Relaxed),
                stats.frames_encoded.load(Ordering::Relaxed),
                stats.frames_dropped.load(Ordering::Relaxed),
                stats.frames_repeated.load(Ordering::Relaxed),
                stats.frames_superseded.load(Ordering::Relaxed),
                stats.encode_us.load(Ordering::Relaxed) as f64 / 1e3,
                stats.slot_us.load(Ordering::Relaxed) as f64 / 1e3
            );
        }
        push_audio(&mut st.mux, &stats)?;
        if disconnected && newest.is_none() {
            break;
        }
    }

    let Some(mut st) = state else {
        return Err(anyhow!("no frames were captured"));
    };
    if let Some(f) = newest.take() {
        pool.recycle(f.data);
    }
    for enc in st.encoder.flush()? {
        st.push(&enc, &stats)?;
    }
    let EncodeState { mut mux, .. } = st;
    // Audio thread flushes and closes its channel after the stop flag; drain everything.
    if mux.has_audio() {
        for f in audio_rx.iter() {
            mux.push_audio(&f)?;
            stats.audio_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    stats.bytes_written.store(mux.bytes_written(), Ordering::Relaxed);
    mux.finalize()?;
    Ok(())
}

struct EncodeState {
    /// Half size uses the exact 2×2 box filter; everything else the scaler.
    half: bool,
    scaler: Option<Scaler>,
    /// The frame being encoded (already scaled). Kept for repeats.
    frame: Option<RawFrame>,
    /// True when `frame` changed since the converter last saw it.
    converted_is_new: bool,
    converter: Converter,
    encoder: Box<dyn VideoEncoder>,
    mux: Muxer,
    previewer: Previewer,
    dims: (u32, u32),
    /// Encoded-frame / source ratio per axis, for mouse-effect placement.
    fx_scale: (f32, f32),
    params_sent: bool,
    patches: Vec<Patch>,
    last_fx: Option<((i32, i32), Vec<FrameClick>)>,
}

impl EncodeState {
    fn new(
        config: &RecordConfig,
        first: &RawFrame,
        audio: Option<AudioTrackConfig>,
        stats: &Stats,
    ) -> Result<Self> {
        let fmt = &config.format;
        let src = (first.width, first.height);
        let (half, scaled) = match fmt.size {
            SizeMode::Full => (false, src),
            SizeMode::Half if src.0 >= 64 && src.1 >= 64 => (true, ((src.0 / 2).max(1), (src.1 / 2).max(1))),
            SizeMode::Half => (false, src),
            other => (false, other.resolve(src.0, src.1)),
        };
        let dims = even_dims(scaled.0, scaled.1);
        if dims.0 == 0 || dims.1 == 0 {
            return Err(anyhow!("frame too small to encode: {}x{}", src.0, src.1));
        }
        let req = EncoderRequest {
            codec: fmt.video_codec.clone(),
            width: dims.0,
            height: dims.1,
            fps: config.fps(),
            rate_control: fmt.rate_control,
            target_bitrate_bps: fmt.target_bitrate_kbps(dims.0, dims.1).saturating_mul(1000),
            keyframe_interval_frames: fmt.keyframe_interval_frames(),
            profiles: fmt.profiles,
        };
        let (encoder, note) = create_video_encoder(&req)?;
        if let Some(n) = note {
            stats.add_note(n);
        }
        let converter = Converter::new(scaled.0, scaled.1, encoder.input_layout())?;
        let scaler = (!half && scaled != src).then(|| Scaler::new(src, scaled));
        let video = VideoTrackConfig {
            width: dims.0,
            height: dims.1,
            fps: config.fps() as f64,
            codec: if encoder.is_hevc() { VideoCodecConfig::Hevc } else { VideoCodecConfig::H264 },
        };
        let mux = Muxer::create(fmt.container, &config.output, video, audio)?;
        let fx_scale = (dims.0 as f32 / src.0.max(1) as f32, dims.1 as f32 / src.1.max(1) as f32);
        log::info!(
            "encoding {}×{} → {}×{} @ {} fps into {} with {}",
            src.0,
            src.1,
            dims.0,
            dims.1,
            config.fps(),
            fmt.container.label(),
            encoder.describe()
        );
        Ok(Self {
            half,
            scaler,
            frame: None,
            converted_is_new: false,
            converter,
            encoder,
            mux,
            previewer: Previewer::new(scaled.0, scaled.1, PREVIEW_MAX_SIDE),
            dims,
            fx_scale,
            params_sent: false,
            patches: Vec::new(),
            last_fx: None,
        })
    }

    /// Makes `captured` the current frame (scaling into the reusable buffer),
    /// returning consumed buffers to the pool.
    fn accept(&mut self, captured: RawFrame, pool: &FramePool) {
        if self.half || self.scaler.is_some() {
            let mut dst = self.frame.take().unwrap_or_else(|| RawFrame::empty(PixelFormat::Bgra));
            if self.half {
                captured.downscale_half_into(&mut dst);
            } else if let Some(s) = &mut self.scaler {
                s.scale_into(&captured, &mut dst);
            }
            pool.recycle(captured.data);
            self.frame = Some(dst);
        } else {
            if let Some(old) = self.frame.replace(captured) {
                pool.recycle(old.data);
            }
        }
        self.converted_is_new = true;
    }

    /// Whether the effects would look different from the last painted frame.
    fn fx_changed(&self, cursor: (i32, i32), clicks: &[FrameClick]) -> bool {
        match &self.last_fx {
            // Ripples animate: any live click means repaint.
            Some((c, prev)) => *c != cursor || !clicks.is_empty() || !prev.is_empty(),
            None => true,
        }
    }

    fn push(&mut self, enc: &EncodedFrame, stats: &Stats) -> Result<()> {
        if !self.params_sent
            && let Some(p) = self.encoder.codec_params()
        {
            self.mux.set_codec_params(p);
            self.params_sent = true;
        }
        if self.mux.push_video(&enc.data, enc.pts, enc.keyframe)? {
            stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
            stats.bytes_written.store(self.mux.bytes_written(), Ordering::Relaxed);
        } else {
            stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

struct AudioArgs {
    config: RecordConfig,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    paused_total: Arc<AtomicU64>,
    origin: Arc<AtomicU64>,
    tx: SyncSender<AudioFrame>,
    stats: Arc<Stats>,
}

fn spawn_audio_thread(args: AudioArgs) -> Result<(JoinHandle<Result<()>>, AudioTrackConfig)> {
    let AudioArgs { config, epoch, stop, paused, paused_total, origin, tx, stats } = args;
    // Open devices and create the encoder on the worker thread (Media
    // Foundation objects are thread-affine), reporting back before mixing.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<AudioTrackConfig>>();
    let handle = std::thread::Builder::new().name("openclip-audio".into()).spawn(move || {
        let mut sources: Vec<(AudioSource, f32)> = Vec::new();
        let mut notes = Vec::new();
        if config.system_audio {
            match open_system_loopback() {
                Ok(s) => sources.push((s, 1.0)),
                Err(e) => notes.push(format!("system audio unavailable: {e:#}")),
            }
        }
        if let Some(mic) = &config.microphone {
            match open_microphone(mic.as_deref()) {
                Ok(s) => sources.push((s, 1.0)),
                Err(e) => notes.push(format!("microphone unavailable: {e:#}")),
            }
        }
        if !notes.is_empty() {
            *stats.audio_note.lock().unwrap() = Some(notes.join("; "));
        }
        if sources.is_empty() {
            let _ = ready_tx.send(Err(anyhow!("no audio source could be opened")));
            return Ok(());
        }
        let fmt = &config.format;
        let (mut encoder, note) =
            match create_audio_encoder(fmt.audio_codec, fmt.audio_sample_rate, fmt.audio_channels, fmt.audio_bitrate_kbps) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return Ok(());
                }
            };
        if let Some(n) = note {
            stats.add_note(n);
        }
        log::info!("audio: {}", encoder.describe());
        let channels = encoder.channels().clamp(1, 2) as usize;
        let rate = encoder.sample_rate();
        let track = AudioTrackConfig {
            sample_rate: rate,
            channels: channels as u16,
            bitrate_bps: encoder.bitrate_bps(),
            samples_per_frame: encoder.samples_per_frame(),
            codec: encoder.codec_config(),
        };
        let _ = ready_tx.send(Ok(track));

        // Wait for the first video frame to define the timeline origin.
        let origin_us = loop {
            let v = origin.load(Ordering::SeqCst);
            if v != u64::MAX {
                break v;
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let origin = Duration::from_micros(origin_us);
        let mut mixer = Mixer::new(rate, epoch, origin);
        for (s, gain) in &sources {
            mixer.add_source(s.queue.clone(), s.sample_rate, s.channels, *gain);
        }
        let mut pcm = Vec::new();
        let mut mono = Vec::new();
        let send = |frames: Vec<AudioFrame>| -> Result<()> {
            for f in frames {
                if tx.send(f).is_err() {
                    return Err(anyhow!("encoder went away"));
                }
            }
            Ok(())
        };
        let mut seen_paused_us = 0u64;
        loop {
            let stopping = stop.load(Ordering::Relaxed);
            let total_paused_us = paused_total.load(Ordering::SeqCst);
            if total_paused_us != seen_paused_us {
                mixer.shift_origin(Duration::from_micros(total_paused_us - seen_paused_us));
                seen_paused_us = total_paused_us;
            }
            if paused.load(Ordering::Relaxed) && !stopping {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let until = if stopping { Instant::now() } else { Instant::now() - AUDIO_LAG };
            pcm.clear();
            mixer.mix_until(until, &mut pcm);
            if !pcm.is_empty() {
                let input: &[f32] = if channels == 1 {
                    // The mixer always produces stereo; fold it down.
                    mono.clear();
                    mono.extend(pcm.as_chunks::<2>().0.iter().map(|[l, r]| (l + r) * 0.5));
                    &mono
                } else {
                    &pcm
                };
                send(encoder.encode(input)?)?;
            }
            if stopping {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        send(encoder.flush()?)?;
        log::info!("audio: done, {} silence samples inserted", mixer.silence_inserted);
        drop(sources);
        Ok(())
    })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(cfg)) => Ok((handle, cfg)),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err(anyhow!("audio thread did not start")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn clock_slots_are_regular() {
        let c = FrameClock::new(30, ms(1000));
        assert_eq!(c.slot_pts(0), ms(1000));
        assert_eq!(c.slot_pts(30), ms(2000));
        assert_eq!(c.deadline(0).as_micros(), 1_016_666);
        assert_eq!(c.due(ms(1010)), 0);
        assert_eq!(c.due(ms(1017)), 1);
        assert_eq!(c.due(ms(1050)), 2);
    }

    #[test]
    fn clock_catches_up_after_a_stall() {
        let mut c = FrameClock::new(30, ms(0));
        // Two slots late: encode them one by one, no skip.
        assert_eq!(c.catch_up(ms(60)), 0);
        assert_eq!(c.due(ms(60)), 2);
        // A 5-slot stall: keep only the latest due slot.
        let skipped = c.catch_up(ms(170));
        assert_eq!(skipped, 4);
        assert_eq!(c.next, 4);
        assert_eq!(c.due(ms(170)), 1);
    }

    #[test]
    fn clock_resyncs_after_pause() {
        let mut c = FrameClock::new(30, ms(0));
        c.next = 10;
        c.resync(ms(1000));
        assert_eq!(c.next, 30);
        assert_eq!(c.due(ms(1000)), 0);
        // Never moves backwards.
        c.resync(ms(100));
        assert_eq!(c.next, 30);
    }
}
