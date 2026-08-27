//! Windows backend: Windows.Graphics.Capture through the `windows-capture` crate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use super::{CaptureConfig, CaptureHandle, FpsLimiter, FrameSink, Rect, Source};
use crate::video::{PixelFormat, RawFrame};

/// Everything the capture callback needs, passed through `Settings::flags`.
struct Flags {
    sink: FrameSink,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    crop: Option<Rect>,
    fps: u32,
}

struct Handler {
    flags: Flags,
    limiter: FpsLimiter,
    scratch: Vec<u8>,
    frames: u64,
}

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let fps = ctx.flags.fps;
        Ok(Self { flags: ctx.flags, limiter: FpsLimiter::new(fps), scratch: Vec::new(), frames: 0 })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        let now = Instant::now();
        if !self.limiter.accept(now) {
            return Ok(());
        }
        let pts = now.duration_since(self.flags.epoch);
        let (fw, fh) = (frame.width(), frame.height());
        let buffer = match self.flags.crop {
            Some(r) => {
                let x0 = r.x.min(fw.saturating_sub(1));
                let y0 = r.y.min(fh.saturating_sub(1));
                let x1 = (r.x + r.width).min(fw).max(x0 + 1);
                let y1 = (r.y + r.height).min(fh).max(y0 + 1);
                frame.buffer_crop(x0, y0, x1, y1)
            }
            None => frame.buffer(),
        }
        .map_err(|e| -> HandlerError { format!("frame buffer: {e:?}").into() })?;
        let (w, h) = (buffer.width(), buffer.height());
        let data = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();
        self.frames += 1;
        let raw = RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts };
        if !(self.flags.sink)(raw) {
            control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("capture item closed after {} frames", self.frames);
        Ok(())
    }
}

pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let (item, crop): (GraphicsCaptureItemType, Option<Rect>) = match &config.source {
        Source::Monitor { id } => (monitor_item(*id)?, None),
        Source::Region { monitor_id, rect } => (monitor_item(*monitor_id)?, Some(*rect)),
        Source::Window { id } => {
            let w = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("window {id} is no longer valid"));
            }
            (w.try_into().map_err(|e| anyhow!("window capture item: {e:?}"))?, None)
        }
    };
    let flags = Flags { sink, epoch, stop: stop.clone(), crop, fps: config.fps };
    let cursor = if config.show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let interval = MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / config.fps.max(1) as f64));
    let settings = Settings::new(
        item,
        cursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    let control = Handler::start_free_threaded(settings)
        .map_err(|e| anyhow!("failed to start Windows.Graphics.Capture: {e:?}"))?;

    let control = Arc::new(Mutex::new(Some(control)));
    let stopper = {
        let stop = stop.clone();
        Box::new(move || -> Result<()> {
            stop.store(true, Ordering::SeqCst);
            let ctl = control.lock().unwrap().take();
            if let Some(ctl) = ctl {
                ctl.stop().map_err(|e| anyhow!("stopping capture: {e:?}"))?;
            }
            Ok(())
        })
    };
    Ok(CaptureHandle::new(stop, stopper))
}

fn monitor_item(id: u32) -> Result<GraphicsCaptureItemType> {
    let m = Monitor::from_raw_hmonitor(id as usize as *mut std::ffi::c_void);
    m.try_into().map_err(|e| anyhow!("monitor capture item: {e:?}")).context("monitor not found")
}
