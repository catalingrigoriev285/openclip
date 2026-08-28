//! Windows backend: Windows.Graphics.Capture through the `windows-capture` crate.
//!
//! Frames stay on the GPU until this module copies them: each callback issues
//! a `CopySubresourceRegion` of the wanted rectangle into one of two
//! persistent staging textures and then maps the *other* one (filled by the
//! previous callback), so the CPU never waits for the copy it just queued.
//! The mapped rows are copied straight into a pooled buffer — one CPU copy
//! per frame, no per-frame texture or heap allocations.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use super::{CaptureConfig, CaptureHandle, FramePool, FrameSink, PhaseLimiter, Rect, Source};
use crate::video::{PixelFormat, RawFrame};

/// Everything the capture callback needs, passed through `Settings::flags`.
struct Flags {
    sink: FrameSink,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    crop: Option<Rect>,
    fps: u32,
    /// Only when WGC cannot throttle itself (Windows 10).
    limit_ourselves: bool,
    pool: Arc<FramePool>,
}

struct Handler {
    flags: Flags,
    limiter: Option<PhaseLimiter>,
    readback: Readback,
    frames: u64,
    priority_set: bool,
}

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let limiter = ctx.flags.limit_ourselves.then(|| PhaseLimiter::new(ctx.flags.fps));
        Ok(Self {
            readback: Readback::new(ctx.device.clone(), ctx.device_context.clone()),
            flags: ctx.flags,
            limiter,
            frames: 0,
            priority_set: false,
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        if !self.priority_set {
            // The delivery thread must not be starved by the encoder's worker threads.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }.ok();
            self.priority_set = true;
        }
        let now = Instant::now();
        if let Some(l) = &mut self.limiter
            && !l.accept(now)
        {
            return Ok(());
        }
        let pts = now.duration_since(self.flags.epoch);
        let (fw, fh) = (frame.width(), frame.height());
        let (x0, y0, x1, y1) = match self.flags.crop {
            Some(r) => {
                let x0 = r.x.min(fw.saturating_sub(1));
                let y0 = r.y.min(fh.saturating_sub(1));
                ((x0), (y0), (r.x + r.width).min(fw).max(x0 + 1), (r.y + r.height).min(fh).max(y0 + 1))
            }
            None => (0, 0, fw, fh),
        };
        let bx = D3D11_BOX { left: x0, top: y0, front: 0, right: x1, bottom: y1, back: 1 };
        let delivered = self
            .readback
            .submit(frame.as_raw_texture(), frame.desc().Format, bx, pts, &self.flags.pool)
            .map_err(|e| -> HandlerError { format!("frame readback: {e:#}").into() })?;
        if let Some(raw) = delivered {
            self.frames += 1;
            if !(self.flags.sink)(raw) {
                control.stop();
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("capture item closed after {} frames", self.frames);
        Ok(())
    }
}

/// Double-buffered GPU → CPU readback.
struct Readback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: [Option<ID3D11Texture2D>; 2],
    size: (u32, u32),
    format: DXGI_FORMAT,
    /// Staging slot holding a copy that has not been read yet, with its timestamp.
    pending: Option<(usize, Duration)>,
}

impl Readback {
    fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Self {
        Self { device, context, staging: [None, None], size: (0, 0), format: DXGI_FORMAT(0), pending: None }
    }

    fn ensure(&mut self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<()> {
        if self.size == (w, h) && self.format == format && self.staging.iter().all(Option::is_some) {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        for slot in &mut self.staging {
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
            *slot = tex;
        }
        self.size = (w, h);
        self.format = format;
        self.pending = None;
        log::debug!("readback staging textures {w}×{h}");
        Ok(())
    }

    /// Queues the copy of `bx` from `src` and returns the frame queued by the
    /// previous call (if any), copied into a pooled buffer.
    fn submit(
        &mut self,
        src: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        bx: D3D11_BOX,
        pts: Duration,
        pool: &FramePool,
    ) -> Result<Option<RawFrame>> {
        let (w, h) = (bx.right - bx.left, bx.bottom - bx.top);
        self.ensure(w, h, format)?;
        let slot = self.pending.map(|(s, _)| s ^ 1).unwrap_or(0);
        let dst = self.staging[slot].as_ref().unwrap();
        unsafe {
            self.context.CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&bx));
            // The capture surface is recycled once this callback returns;
            // make sure the copy is submitted before that.
            self.context.Flush();
        }
        let previous = self.pending.replace((slot, pts));
        let Some((read_slot, read_pts)) = previous else { return Ok(None) };
        let tex = self.staging[read_slot].as_ref().unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.context("Map staging texture")?;
        let row = (w * 4) as usize;
        let pitch = mapped.RowPitch as usize;
        let mut data = pool.take();
        data.reserve(row * h as usize);
        unsafe {
            let base = mapped.pData as *const u8;
            for y in 0..h as usize {
                data.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
            }
            self.context.Unmap(tex, 0);
        }
        Ok(Some(RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: read_pts, mouse: None }))
    }
}

pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    match &config.source {
        Source::Monitor { id } => launch(monitor(*id)?, None, &config, epoch, stop, sink),
        Source::Region { monitor_id, rect } => {
            launch(monitor(*monitor_id)?, Some(*rect), &config, epoch, stop, sink)
        }
        Source::Window { id } => {
            let w = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("window {id} is no longer valid"));
            }
            launch(w, None, &config, epoch, stop, sink)
        }
    }
}

fn launch<T>(
    item: T,
    crop: Option<Rect>,
    config: &CaptureConfig,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    sink: FrameSink,
) -> Result<CaptureHandle>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let fps = config.fps.max(1);
    // Windows 11 throttles delivery itself; Windows 10 delivers every vsync
    // and rejects the setting, so pick frames ourselves there.
    let native_interval = GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false);
    let interval = if native_interval {
        MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / fps as f64))
    } else {
        MinimumUpdateIntervalSettings::Default
    };
    let pool = config.pool.clone().unwrap_or_else(|| FramePool::new(6));
    let flags = Flags { sink, epoch, stop: stop.clone(), crop, fps, limit_ourselves: !native_interval, pool };
    let cursor = if config.show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
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

fn monitor(id: u32) -> Result<Monitor> {
    let m = Monitor::from_raw_hmonitor(id as usize as *mut std::ffi::c_void);
    // Validate the handle by asking for its size.
    m.width().map_err(|e| anyhow!("monitor {id}: {e:?}")).context("monitor not found")?;
    Ok(m)
}
