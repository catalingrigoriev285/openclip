//! The playback thread.
//!
//! Owns the Media Foundation reader **and** the cpal output stream. Both are
//! thread-affine, so neither ever leaves this thread; the GUI only ever sees
//! the atomics in [`Shared`] and the decoded frames on a bounded channel.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::resample::LinearResampler;
use crate::capture::FramePool;
use crate::video::mf::ComGuard;

use super::decode::{Packet, Reader};
use super::output::{fold_to_stereo, Output};
use super::{Command, Frame, PlaybackState, Shared};

/// How many packets to spend looking for the first frame before giving up and
/// letting the GUI show an empty well.
const PRIME_READS: u32 = 400;

/// Idle nap when there is decoded work in hand but nowhere to put it.
const BACKOFF: Duration = Duration::from_millis(2);

/// How many decoded frames the worker may hold back when the GUI queue is full.
///
/// This has to buy roughly as much time as [`output::HIGH_WATER_MS`] of audio,
/// or video falls behind the audio clock and the GUI drops most of what was
/// decoded. Big frames get a shallower queue: at 4K each one is 33 MB.
fn held_cap(width: u32, height: u32) -> usize {
    if width as u64 * height as u64 > 2_500_000 { 3 } else { 8 }
}

pub fn run(
    path: &Path,
    shared: Arc<Shared>,
    pool: Arc<FramePool>,
    cmds: Receiver<Command>,
    frames: SyncSender<Frame>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    // Declared first so COM outlives every interface created below.
    let _com = ComGuard::new();
    let mut worker = match Worker::new(path, shared.clone(), pool, frames, repaint.clone()) {
        Ok(w) => w,
        Err(e) => {
            shared.fail(format!("{e:#}"));
            repaint();
            return;
        }
    };
    worker.main(cmds);
}

struct Worker {
    reader: Reader,
    /// `None` when the file has no sound, or no output device could be opened.
    output: Option<Output>,
    resampler: Option<LinearResampler>,
    shared: Arc<Shared>,
    pool: Arc<FramePool>,
    frames: SyncSender<Frame>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// Decoded but not yet accepted by the GUI queue, oldest first.
    held: VecDeque<Frame>,
    held_cap: usize,
    /// A seek is in flight; everything before this is pre-roll to be discarded.
    target: Option<Duration>,
    /// A frame step is in flight: deliver exactly one frame, then stop.
    stepping: bool,
    /// Every stream reported EOS; the device may still be draining.
    ended: bool,
    interval: Duration,
    last_pts: Duration,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
    /// Frames decoded since the last report, and when that report went out.
    decoded: u32,
    reported: Instant,
}

impl Worker {
    fn new(
        path: &Path,
        shared: Arc<Shared>,
        pool: Arc<FramePool>,
        frames: SyncSender<Frame>,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Worker> {
        let reader = Reader::open(path)?;
        if let Some(d) = reader.duration {
            shared.set_duration(d);
        }
        shared.set_tracks(reader.has_video(), reader.has_audio());
        let interval = reader.video_info.map(|v| v.interval).unwrap_or(super::DEFAULT_FRAME_INTERVAL);
        shared.set_frame_interval(interval);
        if let Some(v) = reader.video_info {
            shared.set_dims(v.width, v.height);
        }

        // Opening the device can fail (headless machine, exclusive-mode
        // hog); that is a note, not an error — the file still plays silently.
        let mut output = None;
        let mut resampler = None;
        if let Some(info) = reader.audio_info {
            match Output::open(shared.clone()) {
                Ok(out) => {
                    shared.clock.set_audio(out.rate, out.rate / 50);
                    if info.rate != out.rate {
                        resampler = Some(LinearResampler::new(info.rate, out.rate, 2));
                    }
                    output = Some(out);
                }
                Err(e) => {
                    log::warn!("no audio output ({e:#}); playing without sound");
                    shared.set_note("no audio output device");
                }
            }
        }

        let held_cap = reader.video_info.map_or(1, |v| held_cap(v.width, v.height));
        let mut me = Worker {
            reader,
            output,
            resampler,
            shared,
            pool,
            frames,
            repaint,
            held: VecDeque::new(),
            held_cap,
            target: None,
            stepping: false,
            ended: false,
            interval,
            last_pts: Duration::ZERO,
            stereo: Vec::new(),
            resampled: Vec::new(),
            decoded: 0,
            reported: Instant::now(),
        };
        me.prime();
        Ok(me)
    }

    /// Decodes far enough to put the opening frame on screen, then starts
    /// playing — opening a clip you just recorded should simply play it.
    fn prime(&mut self) {
        self.shared.set_state(PlaybackState::Paused);
        self.shared.clock.rebase(Duration::ZERO, 0);
        if self.reader.has_video() {
            for _ in 0..PRIME_READS {
                if !self.pump() || self.ended || self.last_pts > Duration::ZERO {
                    break;
                }
            }
        }
        self.start();
    }

    fn main(&mut self, cmds: Receiver<Command>) {
        loop {
            // Block only when there is genuinely nothing to do, so a paused
            // viewer costs no CPU at all.
            if self.idle() {
                match cmds.recv_timeout(Duration::from_millis(100)) {
                    Ok(cmd) => {
                        if !self.handle(cmd) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            loop {
                match cmds.try_recv() {
                    Ok(cmd) => {
                        if !self.handle(cmd) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }

            self.flush_held();
            self.settle_end();

            if self.can_read() {
                if !self.pump() {
                    return;
                }
            } else if !self.idle() {
                std::thread::sleep(BACKOFF);
            }
        }
    }

    /// A seek or a frame step must run to completion whatever the play state.
    fn busy(&self) -> bool {
        self.target.is_some() || self.stepping
    }

    fn playing(&self) -> bool {
        self.shared.state() == PlaybackState::Playing
    }

    fn idle(&self) -> bool {
        !self.playing() && !self.busy() && self.held.is_empty()
    }

    /// Whether to pull another packet. Deliberately never *blocks* on the frame
    /// queue: an occluded window stops repainting, and a worker parked on a full
    /// queue would starve the audio device with it.
    fn can_read(&self) -> bool {
        if self.ended {
            return false;
        }
        if self.busy() {
            return true;
        }
        if !self.playing() {
            return false;
        }
        let audio_full = self
            .output
            .as_ref()
            .is_some_and(|o| o.ring.queued_frames() >= o.high_water_frames());
        // Nothing useful left to do this round: video is buffered as far ahead
        // as it may run and the device has all the sound it can hold.
        !(self.held.len() >= self.held_cap && (self.output.is_none() || audio_full))
    }

    fn handle(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Close => return false,
            Command::Play => self.start(),
            Command::Pause => self.stop(),
            Command::Seek(to) => {
                let resume = self.playing();
                self.do_seek(to, resume);
            }
            Command::Step { from, delta } => self.do_step(from, delta),
        }
        true
    }

    fn ring_frames(&self) -> u64 {
        self.output.as_ref().map_or(0, |o| o.ring.frames_played())
    }

    fn start(&mut self) {
        if self.ended {
            return;
        }
        self.shared.set_state(PlaybackState::Playing);
        self.shared.clock.resume(self.ring_frames());
        if let Some(o) = &self.output {
            o.play();
        }
        (self.repaint)();
    }

    fn stop(&mut self) {
        if self.shared.state() == PlaybackState::Playing {
            self.shared.set_state(PlaybackState::Paused);
        }
        self.shared.clock.freeze();
        if let Some(o) = &self.output {
            o.pause();
        }
        (self.repaint)();
    }

    fn do_seek(&mut self, to: Duration, resume: bool) {
        if let Some(o) = &self.output {
            o.pause();
            o.ring.clear();
        }
        for f in self.held.drain(..) {
            self.pool.recycle(f.image.rgba);
        }
        // Frames already on their way to the GUI belong to the old position.
        self.shared.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = self.reader.seek(to) {
            log::warn!("seek to {to:?}: {e:#}");
        }
        if let Some(r) = &mut self.resampler {
            // Its fractional position and previous sample belong to the old spot.
            *r = LinearResampler::new(r.input_rate(), self.output.as_ref().map_or(48_000, |o| o.rate), 2);
        }
        self.ended = false;
        self.stepping = false;
        self.target = Some(to);
        self.last_pts = to;
        // Show the requested position at once; the landing frame corrects it.
        self.shared.clock.rebase(to, self.ring_frames());
        if resume {
            self.start();
        } else {
            self.stop();
        }
    }

    /// Steps to the frame `delta` away from `from`, the one actually on screen.
    ///
    /// Both directions go through a seek. Stepping forward by taking "the next
    /// packet" would overshoot, because the reader is buffered a third of a
    /// second past whatever the GUI is displaying.
    fn do_step(&mut self, from: Duration, delta: i32) {
        self.stop();
        let step = self.interval * delta.unsigned_abs();
        let to = if delta < 0 { from.saturating_sub(step) } else { from + step };
        self.do_seek(to, false);
        self.stepping = true;
        (self.repaint)();
    }

    /// Once every stream has ended and the device has drained, settle on
    /// [`PlaybackState::Ended`] with the position pinned at the end.
    fn settle_end(&mut self) {
        if !self.ended || self.shared.state() == PlaybackState::Ended {
            return;
        }
        if self.output.as_ref().is_some_and(|o| !o.ring.is_empty()) {
            return;
        }
        let end = self.duration().unwrap_or(self.last_pts);
        self.shared.clock.rebase(end, self.ring_frames());
        self.shared.clock.freeze();
        self.shared.set_state(PlaybackState::Ended);
        if let Some(o) = &self.output {
            o.pause();
        }
        (self.repaint)();
    }

    fn duration(&self) -> Option<Duration> {
        self.reader.duration
    }

    fn pump(&mut self) -> bool {
        // During a seek, everything a half-frame before the target is pre-roll:
        // the reader skips converting it, which is what makes seeks feel quick.
        let pixels_from = self.target.map(|t| t.saturating_sub(self.interval / 2));
        match self.reader.read(pixels_from, &self.pool) {
            Ok(Packet::Video { image, pts }) => self.on_video(image, pts),
            Ok(Packet::VideoSkipped { pts }) => self.last_pts = pts,
            Ok(Packet::Audio { samples, channels, pts }) => self.on_audio(samples, channels, pts),
            Ok(Packet::Idle) => {}
            Ok(Packet::End) => self.ended = true,
            Err(e) => {
                self.shared.fail(format!("{e:#}"));
                (self.repaint)();
                return false;
            }
        }
        true
    }

    fn on_video(&mut self, image: crate::video::preview::PreviewImage, pts: Duration) {
        let landed = self.target.take().is_some() || self.stepping;
        self.stepping = false;
        self.last_pts = pts;
        if landed {
            self.shared.clock.rebase(pts, self.ring_frames());
            if self.playing() {
                self.shared.clock.resume(self.ring_frames());
            }
        }
        self.shared.set_dims(image.width, image.height);
        let seq = self.shared.seq.load(std::sync::atomic::Ordering::Relaxed);
        self.place(Frame { image, pts, seq });
        self.report(pts);
        (self.repaint)();
    }

    /// One line a second: decode rate and how far video trails the clock. If
    /// the drift creeps up, the decoder is not keeping pace with playback.
    fn report(&mut self, pts: Duration) {
        self.decoded += 1;
        let elapsed = self.reported.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let fps = self.decoded as f64 / elapsed.as_secs_f64();
        let drift = self.shared.clock.position().as_secs_f64() - pts.as_secs_f64();
        log::debug!(
            "player: {fps:.1} fps decoded, drift {drift:+.3} s, held {}/{}",
            self.held.len(),
            self.held_cap
        );
        self.decoded = 0;
        self.reported = Instant::now();
    }

    fn on_audio(&mut self, samples: Vec<f32>, channels: usize, pts: Duration) {
        if let Some(t) = self.target {
            if pts + chunk_len(samples.len(), channels, self.audio_rate()) < t {
                return;
            }
            // With no video track the first audio packet is the landing point.
            if !self.reader.has_video() {
                self.target = None;
                self.stepping = false;
                self.last_pts = pts;
                self.shared.clock.rebase(pts, self.ring_frames());
                if self.playing() {
                    self.shared.clock.resume(self.ring_frames());
                }
                (self.repaint)();
            }
        }
        let Some(ring) = self.output.as_ref().map(|o| o.ring.clone()) else { return };
        fold_to_stereo(&samples, channels, &mut self.stereo);
        match &mut self.resampler {
            Some(r) => {
                self.resampled.clear();
                r.process(&self.stereo, &mut self.resampled);
                ring.push(&self.resampled);
            }
            None => ring.push(&self.stereo),
        }
    }

    fn audio_rate(&self) -> u32 {
        self.reader.audio_info.map_or(48_000, |a| a.rate)
    }

    /// Hands a frame to the GUI, or queues it when the channel is full. Only
    /// once the backlog is over its cap is the oldest frame given up — dropping
    /// eagerly here is what starves the GUI of frames it could still have shown.
    fn place(&mut self, frame: Frame) {
        match self.frames.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(f)) => {
                self.held.push_back(f);
                while self.held.len() > self.held_cap {
                    if let Some(old) = self.held.pop_front() {
                        self.pool.recycle(old.image.rgba);
                    }
                }
            }
            Err(TrySendError::Disconnected(f)) => self.pool.recycle(f.image.rgba),
        }
    }

    fn flush_held(&mut self) {
        while let Some(f) = self.held.pop_front() {
            match self.frames.try_send(f) {
                Ok(()) => (self.repaint)(),
                Err(TrySendError::Full(f)) => {
                    self.held.push_front(f);
                    return;
                }
                Err(TrySendError::Disconnected(f)) => self.pool.recycle(f.image.rgba),
            }
        }
    }
}

/// How long an interleaved PCM chunk lasts.
fn chunk_len(samples: usize, channels: usize, rate: u32) -> Duration {
    if rate == 0 || channels == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(samples as f64 / channels as f64 / rate as f64)
}
