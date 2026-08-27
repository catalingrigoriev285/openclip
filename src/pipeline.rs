//! Recording pipeline: capture thread → encode thread (video encoder + muxer),
//! plus an audio thread (cpal → mixer → audio encoder) feeding the muxer.

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
use crate::capture::{self, CaptureConfig, CaptureHandle, Source};
use crate::mux::{AudioTrackConfig, Muxer, VideoCodecConfig, VideoTrackConfig};
use crate::settings::{FormatSettings, SizeMode};
use crate::video::convert::even_dims;
use crate::video::mouse_fx::{MouseFx, MouseSampler};
use crate::video::preview::{make_preview, PreviewImage};
use crate::video::{create_video_encoder, Converter, EncoderRequest, RawFrame, Scaler, VideoEncoder};

/// Longest side of preview images handed to the GUI.
const PREVIEW_MAX_SIDE: u32 = 640;
/// Audio is mixed this far behind wall-clock so device latency never causes gaps.
const AUDIO_LAG: Duration = Duration::from_millis(150);

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
    pub frames_dropped: AtomicU64,
    /// Frames the encoder chose not to output (rate control) or the container could not place.
    pub frames_skipped: AtomicU64,
    /// Heartbeat frames (re-encoded last frame while the screen was static).
    pub frames_repeated: AtomicU64,
    /// Rolling average encode time per frame, in microseconds.
    pub encode_us: AtomicU64,
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
    started: Instant,
    output: PathBuf,
}

impl Recorder {
    pub fn start(config: RecordConfig, on_preview: Option<PreviewCallback>) -> Result<Recorder> {
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let paused_total_us = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(Stats::default());
        let preview = Arc::new(PreviewSlot::default());
        // Timeline origin (first video pts) shared with the audio thread, in µs; u64::MAX = unknown.
        let audio_origin = Arc::new(AtomicU64::new(u64::MAX));

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
            on_preview,
            sampler: sampler.clone(),
        })?;

        let capture = start_capture(&config, epoch, video_tx, stats.clone(), sampler)?;

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
            started: epoch,
            output: config.output,
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn preview(&self) -> &Arc<PreviewSlot> {
        &self.preview
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
) -> Result<CaptureHandle> {
    let sink: capture::FrameSink = Box::new(move |mut frame| {
        stats.frames_captured.fetch_add(1, Ordering::Relaxed);
        // Sample the pointer right when the frame arrives so effects line up
        // with the captured image even if encoding lags behind.
        if let Some(s) = &sampler {
            frame.mouse = Some(s.lock().unwrap().snapshot());
        }
        match tx.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    });
    capture::start(
        CaptureConfig { source: config.source.clone(), fps: config.fps(), show_cursor: config.mouse_fx.native_cursor() },
        epoch,
        sink,
    )
    .context("starting screen capture")
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
    on_preview: Option<PreviewCallback>,
    /// Shared with the capture sink; used here only for heartbeat frames.
    sampler: Option<Arc<Mutex<MouseSampler>>>,
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
        on_preview,
        sampler,
    } = args;
    // Recording-time clock: wall time since epoch minus paused time.
    let paused_dur = || Duration::from_micros(paused_total.load(Ordering::Relaxed));
    let rec_now = || epoch.elapsed().saturating_sub(paused_dur());
    let fps = config.fps();
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let min_delta = Duration::from_secs_f64(0.75 / fps as f64);
    // Capture APIs only deliver frames when the screen changes; when nothing
    // arrives for this long we re-encode the last frame so the video keeps a
    // steady cadence and stays as long as the audio.
    let heartbeat = frame_interval.max(Duration::from_millis(20));
    let preview_every = (fps / 10).max(1) as u64;
    let mut avg_encode_us = 0f64;

    let mut state: Option<EncodeState> = None;
    let mut last_pts: Option<Duration> = None;
    let mut last_size = (0u32, 0u32);
    let mut last_frame: Option<RawFrame> = None;
    let mut origin = source_origin(&config.source).unwrap_or((0, 0));
    let mut frames_since_origin = 0u32;
    let mut frames_in = 0u64;

    let push_audio = |mux: &mut Muxer, stats: &Stats| -> Result<()> {
        if mux.has_audio() {
            while let Ok(f) = audio_rx.try_recv() {
                mux.push_audio(&f)?;
                stats.audio_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    };

    loop {
        let (frame, is_heartbeat) = match video_rx.recv_timeout(heartbeat) {
            Ok(mut f) => {
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                f.pts = f.pts.saturating_sub(paused_dur());
                (f, false)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(st) = state.as_mut() {
                    push_audio(&mut st.mux, &stats)?;
                }
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                match &last_frame {
                    Some(f)
                        if rec_now().saturating_sub(last_pts.unwrap_or_default())
                            >= frame_interval.mul_f64(0.9) =>
                    {
                        let mut f = f.clone();
                        f.pts = rec_now();
                        // The screen is static but the pointer may move: sample now.
                        if let Some(s) = &sampler {
                            f.mouse = Some(s.lock().unwrap().snapshot());
                        }
                        (f, true)
                    }
                    _ => continue,
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(last) = last_pts {
            // Pacing: frames arriving faster than the target rate are skipped
            // (not counted as drops; they are by design).
            if frame.pts.saturating_sub(last) < min_delta {
                continue;
            }
        }

        if !is_heartbeat {
            let size = (frame.width, frame.height);
            if state.is_none() {
                let st = EncodeState::new(&config, &frame, audio_cfg.take(), &stats)?;
                stats.width.store(st.dims.0 as u64, Ordering::Relaxed);
                stats.height.store(st.dims.1 as u64, Ordering::Relaxed);
                state = Some(st);
                audio_origin.store(frame.pts.as_micros() as u64, Ordering::SeqCst);
                last_size = size;
            } else if size != last_size {
                // Source resized (window capture). Keep encoding at the original
                // size by cropping/padding is complex; simplest robust option is to
                // skip frames of a different size and note it once.
                if stats.audio_note.lock().unwrap().is_none() {
                    *stats.audio_note.lock().unwrap() =
                        Some(format!("source resized to {}×{}; frames skipped", size.0, size.1));
                }
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        let st = state.as_mut().unwrap();
        // Heartbeats re-use the already scaled last frame.
        let frame = if is_heartbeat { frame } else { st.prepare(frame) };
        last_pts = Some(frame.pts);

        let t0 = Instant::now();
        // Mouse effects are painted on a copy so the clean frame can be reused for heartbeats.
        let mut painted: Option<RawFrame> = None;
        if let Some(snap) = frame.mouse.as_ref().filter(|_| sampler.is_some()) {
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
            let mut f = frame.clone();
            config.mouse_fx.apply(&mut f, cursor, &clicks, st.fx_scale.0.min(st.fx_scale.1));
            painted = Some(f);
        }
        let shown: &RawFrame = painted.as_ref().unwrap_or(&frame);
        // Heartbeats reuse the previous conversion unless effects moved.
        if !is_heartbeat || painted.is_some() {
            st.converter.convert(shown)?;
        }
        let t1 = Instant::now();
        let encoded = st.encoder.encode(st.converter.frame(), frame.pts)?;
        let t2 = Instant::now();
        frames_in += 1;
        if is_heartbeat {
            stats.frames_repeated.fetch_add(1, Ordering::Relaxed);
        }
        if encoded.is_empty() {
            stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
        }
        for enc in encoded {
            st.push(&enc, &stats)?;
        }
        if frames_in % preview_every == 0 && !is_heartbeat {
            *preview.image.lock().unwrap() = Some(make_preview(shown, PREVIEW_MAX_SIDE));
            if let Some(cb) = &on_preview {
                cb();
            }
        }
        let t3 = Instant::now();
        let enc_us = (t2 - t1).as_micros() as f64;
        avg_encode_us = if avg_encode_us == 0.0 { enc_us } else { avg_encode_us * 0.9 + enc_us * 0.1 };
        stats.encode_us.store(avg_encode_us as u64, Ordering::Relaxed);
        let total = t3 - t0;
        if total > frame_interval * 2 {
            log::warn!(
                "slow frame: convert {:.1} ms, encode {:.1} ms, mux {:.1} ms{}",
                (t1 - t0).as_secs_f64() * 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                (t3 - t2).as_secs_f64() * 1e3,
                if is_heartbeat { " (heartbeat)" } else { "" }
            );
        } else {
            log::trace!("frame: convert {:?} encode {:?} mux {:?}", t1 - t0, t2 - t1, t3 - t2);
        }
        if !is_heartbeat {
            last_frame = Some(frame);
        }
        push_audio(&mut st.mux, &stats)?;
        if stop.load(Ordering::Relaxed) && video_rx.try_recv().is_err() {
            break;
        }
    }

    let Some(mut st) = state else {
        return Err(anyhow!("no frames were captured"));
    };
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
    converter: Converter,
    encoder: Box<dyn VideoEncoder>,
    mux: Muxer,
    dims: (u32, u32),
    /// Encoded-frame / source ratio per axis, for mouse-effect placement.
    fx_scale: (f32, f32),
    params_sent: bool,
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
            codec: fmt.video_codec,
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
            "encoding {}×{} → {}×{} into {} with {}",
            src.0,
            src.1,
            dims.0,
            dims.1,
            fmt.container.label(),
            encoder.describe()
        );
        Ok(Self { half, scaler, converter, encoder, mux, dims, fx_scale, params_sent: false })
    }

    /// Scales a freshly captured frame to the output size.
    fn prepare(&mut self, frame: RawFrame) -> RawFrame {
        if self.half {
            frame.downscale_half()
        } else if let Some(s) = &mut self.scaler {
            s.scale(&frame)
        } else {
            frame
        }
    }

    fn push(&mut self, enc: &crate::video::EncodedFrame, stats: &Stats) -> Result<()> {
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
                    mono.extend(pcm.chunks_exact(2).map(|lr| (lr[0] + lr[1]) * 0.5));
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
