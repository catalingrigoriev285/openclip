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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};
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

use super::{CaptureConfig, CaptureHandle, FramePool, FrameSink, LiveRect, PhaseLimiter, Source};
use crate::video::{PixelFormat, RawFrame};

/// Everything the capture callback needs, passed through `Settings::flags`.
struct Flags {
    sink: FrameSink,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    /// Re-read every frame so the UI can drag the region while recording.
    crop: Option<LiveRect>,
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
        let (x0, y0, x1, y1) = match self.flags.crop.as_ref().map(LiveRect::get) {
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

/// The [`PixelFormat`] a DXGI surface maps to, or `None` for one the pipeline
/// cannot take. HDR and 10-bit back buffers land here — a game using one is
/// refused with a note rather than recorded with mangled colour.
pub(crate) fn pixel_format(format: DXGI_FORMAT) -> Option<PixelFormat> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Some(PixelFormat::Bgra),
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Some(PixelFormat::Rgba),
        _ => None,
    }
}

/// Double-buffered GPU → CPU readback.
///
/// Shared with the game-capture backend, which reads the hook's shared texture
/// exactly the same way Windows.Graphics.Capture's surface is read here.
pub(crate) struct Readback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: [Option<ID3D11Texture2D>; 2],
    size: (u32, u32),
    format: DXGI_FORMAT,
    /// Byte order of `format`, resolved once when the staging textures are made.
    pixels: PixelFormat,
    /// Staging slot holding a copy that has not been read yet, with its timestamp.
    pending: Option<(usize, Duration)>,
}

impl Readback {
    pub(crate) fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Self {
        Self {
            device,
            context,
            staging: [None, None],
            size: (0, 0),
            format: DXGI_FORMAT(0),
            pixels: PixelFormat::Bgra,
            pending: None,
        }
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
        self.pixels = pixel_format(format)
            .ok_or_else(|| anyhow!("unsupported surface format {:?}; 8-bit BGRA or RGBA only", format.0))?;
        self.pending = None;
        log::debug!("readback staging textures {w}×{h}");
        Ok(())
    }

    /// Queues the copy of `bx` from `src` and returns the frame queued by the
    /// previous call (if any), copied into a pooled buffer.
    pub(crate) fn submit(
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
        Ok(Some(RawFrame { data, width: w, height: h, stride: w * 4, format: self.pixels, pts: read_pts, mouse: None }))
    }
}

pub fn start(config: CaptureConfig, epoch: Instant, sink: FrameSink) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let crop = config.crop();
    match &config.source {
        Source::Monitor { id } => launch(monitor(*id)?, None, &config, epoch, stop, sink),
        Source::Region { monitor_id, .. } => launch(monitor(*monitor_id)?, crop, &config, epoch, stop, sink),
        Source::Window { id } => {
            let w = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("window {id} is no longer valid"));
            }
            launch(w, None, &config, epoch, stop, sink)
        }
        // Frames come from the hook inside the game, not from this backend.
        Source::Game { .. } => super::hook::start(config, epoch, sink),
    }
}

fn launch<T>(
    item: T,
    crop: Option<LiveRect>,
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
    let native_interval = match GraphicsCaptureApi::is_minimum_update_interval_supported() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("capture: minimum-update-interval probe failed ({e:?}); limiting frames ourselves");
            false
        }
    };
    let interval = if native_interval {
        let floor = super::min_update_interval(fps);
        log::info!("capture: {fps} fps throttled by WGC ({:.2} ms minimum update interval)", floor.as_secs_f64() * 1e3);
        MinimumUpdateIntervalSettings::Custom(floor)
    } else {
        log::info!("capture: {fps} fps throttled by PhaseLimiter (WGC minimum update interval unsupported)");
        MinimumUpdateIntervalSettings::Default
    };
    let pool = config.pool.clone().unwrap_or_else(|| FramePool::new(6));
    let flags = Flags { sink, epoch, stop: stop.clone(), crop, fps, limit_ourselves: !native_interval, pool };
    // Both of these are rejected outright — the session never starts — when the
    // OS lacks the property behind them, so ask only for what it supports.
    let border_ok = supported("border", GraphicsCaptureApi::is_border_settings_supported());
    let cursor_ok = supported("cursor", GraphicsCaptureApi::is_cursor_settings_supported());
    let mut notes = Vec::new();
    if !border_ok {
        notes.push(crate::t!(NOTE_CAPTURE_BORDER_UNSUPPORTED).to_string());
    }
    // The system default captures the cursor, which is what `show_cursor` asks
    // for anyway — only hiding it cannot be honoured.
    if !cursor_ok && !config.show_cursor {
        notes.push(crate::t!(NOTE_CAPTURE_CURSOR_UNSUPPORTED).to_string());
    }
    let settings = Settings::new(
        item,
        cursor_setting(config.show_cursor, cursor_ok),
        border_setting(border_ok),
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    let control = Handler::start_free_threaded(settings)
        .map_err(|e| anyhow!("failed to start Windows.Graphics.Capture: {e}"))?;

    let control = Arc::new(Mutex::new(Some(control)));
    let stopper = {
        let stop = stop.clone();
        Box::new(move || -> Result<()> {
            stop.store(true, Ordering::SeqCst);
            let ctl = control.lock().unwrap().take();
            if let Some(ctl) = ctl {
                ctl.stop().map_err(|e| anyhow!("stopping capture: {e}"))?;
            }
            Ok(())
        })
    };
    Ok(CaptureHandle::new(stop, stopper).with_note((!notes.is_empty()).then(|| notes.join("; "))))
}

/// Whether an optional capture setting can be changed on this OS. A failed
/// probe counts as unsupported: asking for a setting the session rejects makes
/// it refuse to start at all, so the safe answer is the system default.
/// `OPENCLIP_LEGACY_WGC` forces every answer to `false` — the Windows 10 path,
/// which has no `IsBorderRequired` (and no `IsCursorCaptureEnabled` before 2004).
fn supported(what: &str, probe: Result<bool, windows_capture::graphics_capture_api::Error>) -> bool {
    if std::env::var_os("OPENCLIP_LEGACY_WGC").is_some() {
        log::info!("capture: OPENCLIP_LEGACY_WGC set; treating the {what} setting as unsupported");
        return false;
    }
    match probe {
        Ok(true) => true,
        Ok(false) => {
            log::info!("capture: this Windows version cannot change the {what} setting; using the system default");
            false
        }
        Err(e) => {
            log::warn!("capture: {what}-setting probe failed ({e}); using the system default");
            false
        }
    }
}

/// Hide the capture border where the OS allows it (Windows 11); elsewhere the
/// system default, which draws the yellow frame around the captured item.
fn border_setting(supported: bool) -> DrawBorderSettings {
    if supported { DrawBorderSettings::WithoutBorder } else { DrawBorderSettings::Default }
}

/// Follow `show_cursor` where the OS allows it; elsewhere the system default,
/// which includes the cursor.
fn cursor_setting(show_cursor: bool, supported: bool) -> CursorCaptureSettings {
    match (supported, show_cursor) {
        (false, _) => CursorCaptureSettings::Default,
        (true, true) => CursorCaptureSettings::WithCursor,
        (true, false) => CursorCaptureSettings::WithoutCursor,
    }
}

fn monitor(id: u32) -> Result<Monitor> {
    let m = Monitor::from_raw_hmonitor(id as usize as *mut std::ffi::c_void);
    // Validate the handle by asking for its size.
    m.width().map_err(|e| anyhow!("monitor {id}: {e:?}")).context("monitor not found")?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_settings_fall_back_to_the_system_default() {
        // Windows 11: exactly what was asked for.
        assert_eq!(border_setting(true), DrawBorderSettings::WithoutBorder);
        assert_eq!(cursor_setting(true, true), CursorCaptureSettings::WithCursor);
        assert_eq!(cursor_setting(false, true), CursorCaptureSettings::WithoutCursor);
        // Windows 10: the session refuses to start unless both are `Default`.
        assert_eq!(border_setting(false), DrawBorderSettings::Default);
        assert_eq!(cursor_setting(true, false), CursorCaptureSettings::Default);
        assert_eq!(cursor_setting(false, false), CursorCaptureSettings::Default);
    }
}
