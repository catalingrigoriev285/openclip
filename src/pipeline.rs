//! Recording pipeline: capture thread → encode thread (H.264 + MP4 muxer),
//! plus an audio thread (cpal → mixer → MP3) feeding the muxer.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::audio::capture::{open_microphone, open_system_loopback, AudioSource};
use crate::audio::mixer::Mixer;
use crate::audio::{Mp3Encoder, Mp3Frame};
use crate::capture::{self, CaptureConfig, CaptureHandle, Source};
use crate::mux::{AudioTrackConfig, Mp4Writer, VideoTrackConfig};
use crate::video::preview::{make_preview, PreviewImage};
use crate::video::{Converter, H264Encoder, RawFrame};

/// Master audio sample rate of the recording.
pub const AUDIO_RATE: u32 = 48_000;
/// Longest side of preview images handed to the GUI.
const PREVIEW_MAX_SIDE: u32 = 640;
/// Audio is mixed this far behind wall-clock so device latency never causes gaps.
const AUDIO_LAG: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub source: Source,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub half_resolution: bool,
    pub show_cursor: bool,
    pub system_audio: bool,
    /// `Some(None)` = default microphone, `Some(Some(name))` = named device.
    pub microphone: Option<Option<String>>,
    pub output: PathBuf,
}

impl RecordConfig {
    pub fn wants_audio(&self) -> bool {
        self.system_audio || self.microphone.is_some()
    }
}

/// Live counters, readable from the GUI.
#[derive(Debug, Default)]
pub struct Stats {
    pub frames_captured: AtomicU64,
    pub frames_encoded: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub audio_frames: AtomicU64,
    pub bytes_written: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub audio_note: Mutex<Option<String>>,
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
        let stats = Arc::new(Stats::default());
        let preview = Arc::new(PreviewSlot::default());
        // Timeline origin (first video pts) shared with the audio thread, in µs; u64::MAX = unknown.
        let audio_origin = Arc::new(AtomicU64::new(u64::MAX));

        let (video_tx, video_rx) = mpsc::sync_channel::<RawFrame>(4);
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Mp3Frame>(256);

        let audio_thread = if config.wants_audio() {
            Some(spawn_audio_thread(
                config.clone(),
                epoch,
                stop.clone(),
                audio_origin.clone(),
                audio_tx,
                stats.clone(),
            )?)
        } else {
            drop(audio_tx);
            None
        };

        let encode_thread = spawn_encode_thread(EncodeArgs {
            config: config.clone(),
            video_rx,
            audio_rx,
            audio_origin,
            stop: stop.clone(),
            stats: stats.clone(),
            preview: preview.clone(),
            on_preview,
        })?;

        let capture = start_capture(&config, epoch, video_tx, stats.clone())?;

        Ok(Recorder {
            stop,
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

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
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
        self.stop.store(true, Ordering::SeqCst);
        if let Some(capture) = self.capture.take() {
            if let Err(e) = capture.stop() {
                log::warn!("capture stop: {e:#}");
            }
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
) -> Result<CaptureHandle> {
    let sink: capture::FrameSink = Box::new(move |frame| {
        stats.frames_captured.fetch_add(1, Ordering::Relaxed);
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
        CaptureConfig { source: config.source.clone(), fps: config.fps, show_cursor: config.show_cursor },
        epoch,
        sink,
    )
    .context("starting screen capture")
}

struct EncodeArgs {
    config: RecordConfig,
    video_rx: Receiver<RawFrame>,
    audio_rx: Receiver<Mp3Frame>,
    audio_origin: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    preview: Arc<PreviewSlot>,
    on_preview: Option<PreviewCallback>,
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
    let EncodeArgs { config, video_rx, audio_rx, audio_origin, stop, stats, preview, on_preview } = args;
    let fps = config.fps.max(1);
    let min_delta = Duration::from_secs_f64(0.75 / fps as f64);
    let preview_every = (fps / 10).max(1) as u64;

    let mut state: Option<EncodeState> = None;
    let mut last_pts: Option<Duration> = None;
    let mut last_size = (0u32, 0u32);

    let mut push_audio = |mux: &mut Mp4Writer<BufWriter<File>>, stats: &Stats| -> Result<()> {
        if mux.has_audio() {
            while let Ok(f) = audio_rx.try_recv() {
                mux.push_audio(&f.data)?;
                stats.audio_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    };

    loop {
        let frame = match video_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(f) => f,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(st) = state.as_mut() {
                    push_audio(&mut st.mux, &stats)?;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut frame = frame;
        if config.half_resolution && frame.width >= 64 && frame.height >= 64 {
            frame = frame.downscale_half();
        }
        if let Some(last) = last_pts {
            if frame.pts.saturating_sub(last) < min_delta {
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }

        let size = (frame.width, frame.height);
        if state.is_none() {
            let st = EncodeState::new(&config, &frame)?;
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
        let st = state.as_mut().unwrap();
        last_pts = Some(frame.pts);

        st.converter.convert(&frame)?;
        if let Some(enc) = st.encoder.encode(&st.converter.yuv(), frame.pts)? {
            if let (Some(sps), Some(pps)) = (st.encoder.sps(), st.encoder.pps()) {
                st.mux.set_parameter_sets(sps, pps);
            }
            st.mux.push_video(&enc.data, enc.pts, enc.keyframe)?;
            let n = stats.frames_encoded.fetch_add(1, Ordering::Relaxed) + 1;
            stats.bytes_written.store(st.mux.bytes_written(), Ordering::Relaxed);
            if n % preview_every == 0 {
                *preview.image.lock().unwrap() = Some(make_preview(&frame, PREVIEW_MAX_SIDE));
                if let Some(cb) = &on_preview {
                    cb();
                }
            }
        }
        push_audio(&mut st.mux, &stats)?;
        if stop.load(Ordering::Relaxed) && video_rx.try_recv().is_err() {
            break;
        }
    }

    let Some(st) = state else {
        return Err(anyhow!("no frames were captured"));
    };
    let EncodeState { mut mux, .. } = st;
    // Audio thread flushes and closes its channel after the stop flag; drain everything.
    if mux.has_audio() {
        for f in audio_rx.iter() {
            mux.push_audio(&f.data)?;
            stats.audio_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    stats.bytes_written.store(mux.bytes_written(), Ordering::Relaxed);
    mux.finalize().context("finalizing MP4")?;
    Ok(())
}

struct EncodeState {
    converter: Converter,
    encoder: H264Encoder,
    mux: Mp4Writer<BufWriter<File>>,
    dims: (u32, u32),
}

impl EncodeState {
    fn new(config: &RecordConfig, first: &RawFrame) -> Result<Self> {
        let converter = Converter::new(first.width, first.height)?;
        let dims = converter.dimensions();
        let encoder = H264Encoder::new(config.fps as f32, config.bitrate_kbps.max(200) * 1000)?;
        if let Some(parent) = config.output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file = File::create(&config.output)
            .with_context(|| format!("creating {}", config.output.display()))?;
        let audio = config.wants_audio().then(|| AudioTrackConfig {
            sample_rate: AUDIO_RATE,
            channels: 2,
            bitrate_bps: 160_000,
            samples_per_frame: 1152,
        });
        let mux = Mp4Writer::new(
            BufWriter::with_capacity(1 << 20, file),
            Some(VideoTrackConfig { width: dims.0, height: dims.1, fps: config.fps as f64 }),
            audio,
        )?;
        log::info!("encoding {}×{} @ {} fps, {} kbps", dims.0, dims.1, config.fps, config.bitrate_kbps);
        Ok(Self { converter, encoder, mux, dims })
    }
}

fn spawn_audio_thread(
    config: RecordConfig,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    origin: Arc<AtomicU64>,
    tx: SyncSender<Mp3Frame>,
    stats: Arc<Stats>,
) -> Result<JoinHandle<Result<()>>> {
    // Open devices on the calling thread so errors surface immediately, then
    // hand them to the worker. cpal streams are created and kept on that thread.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
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
        let _ = ready_tx.send(Ok(()));

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
        let mut mixer = Mixer::new(AUDIO_RATE, epoch, origin);
        for (s, gain) in &sources {
            mixer.add_source(s.queue.clone(), s.sample_rate, s.channels, *gain);
        }
        let mut encoder = Mp3Encoder::new(AUDIO_RATE, 2, 160)?;
        let mut pcm = Vec::new();
        let mut send = |frames: Vec<Mp3Frame>| -> Result<()> {
            for f in frames {
                if tx.send(f).is_err() {
                    return Err(anyhow!("encoder went away"));
                }
            }
            Ok(())
        };
        loop {
            let stopping = stop.load(Ordering::Relaxed);
            let until = if stopping { Instant::now() } else { Instant::now() - AUDIO_LAG };
            pcm.clear();
            mixer.mix_until(until, &mut pcm);
            if !pcm.is_empty() {
                send(encoder.encode(&pcm)?)?;
            }
            if stopping {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        send(encoder.flush()?)?;
        log::info!(
            "audio: {} frames, {} silence samples inserted",
            encoder.frames_emitted(),
            mixer.silence_inserted
        );
        drop(sources);
        Ok(())
    })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err(anyhow!("audio thread did not start")),
    }
}
