//! The DXGI present hook: the one attach point D3D11 and D3D12 share.
//!
//! Both APIs present through the same `IDXGISwapChain` vtable, so a single
//! patch of three slots (see [`crate::vtable`]) catches every game on either.
//! What differs is only what can be *done* with the back buffer once `Present`
//! is called, and that is what a [`Surface`] provides: a D3D11 device, context
//! and render target for the frame about to go out. D3D11 hands its own back
//! buffer over directly; D3D12 wraps it through D3D11On12.
//!
//! Every swapchain in the process shares the vtable, so one patch also covers
//! the game's real swapchain, any launcher window, and any swapchain created
//! later — which is what makes alt-enter and fullscreen transitions a non-event.
//!
//! Order inside `Present` matters and is deliberate: the counter is drawn
//! *after* the frame has been published for recording, so the number a player
//! sees on screen does not end up baked into the file. `burn_in` swaps the two.

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use openclip_overlay::abi::{Control, GfxApi, HookError};
use openclip_overlay::fps::HookState;
use windows::core::{Interface, HRESULT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{IDXGISwapChain, IDXGISwapChain1, DXGI_PRESENT_TEST};

use crate::d3d11::Renderer;
use crate::fault;
use crate::logging::hlog;
use crate::vtable;
use crate::worker;

/// `IDXGISwapChain::Present` — `IUnknown`(3) + `IDXGIObject`(4) + `IDXGIDeviceSubObject`(1).
const SLOT_PRESENT: usize = 8;
/// `IDXGISwapChain::ResizeBuffers`, five methods after `Present`.
const SLOT_RESIZE_BUFFERS: usize = 13;
/// `IDXGISwapChain1::Present1` — `IDXGISwapChain1` starts at 18; this is its 5th.
const SLOT_PRESENT1: usize = 22;

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> HRESULT;
type ResizeBuffersFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, DXGI_FORMAT, u32) -> HRESULT;

struct Originals {
    present: PresentFn,
    present1: Option<Present1Fn>,
    resize_buffers: ResizeBuffersFn,
}

static ORIGINALS: OnceLock<Originals> = OnceLock::new();
static SURFACE: Mutex<Option<Box<dyn Surface>>> = Mutex::new(None);
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);
static METER: Mutex<FpsMeter> = Mutex::new(FpsMeter::new());

// ----- what a backend has to provide -----------------------------------------

/// The D3D11 view of the frame about to be presented.
///
/// Held by value with cloned interface pointers so it can outlive the borrow of
/// the surface that produced it — the renderer needs the device and the back
/// buffer at the same time, and the D3D12 surface has to stay mutable in
/// between to release its wrapped resources afterwards.
pub(crate) struct Target {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// What gets copied into openclip's shared texture.
    pub back: ID3D11Texture2D,
    /// What the counter is drawn onto — the same surface, as a render target.
    pub rtv: ID3D11RenderTargetView,
    pub size: (u32, u32),
}

/// A graphics API's answer to "give me this frame's back buffer as D3D11".
///
/// One is chosen per process the first time a swapchain is presented, from the
/// type of device behind it, and kept until `ResizeBuffers` or a device loss.
pub(crate) trait Surface: Send {
    /// Which API this is, for the status card.
    fn api(&self) -> GfxApi;

    /// Makes the back buffer of the frame being presented usable from D3D11.
    fn acquire(&mut self, swap: &IDXGISwapChain) -> windows::core::Result<Target>;

    /// Hands it back. Always called when [`acquire`](Self::acquire) succeeded,
    /// including on the path where drawing failed.
    fn release(&mut self);

    /// Drops everything that references the swapchain's buffers, before
    /// `ResizeBuffers` runs.
    fn release_swapchain_resources(&mut self);
}

// ----- installation ----------------------------------------------------------

/// Points the DXGI vtable at us. Safe to call more than once; only the first
/// call does anything.
pub fn install() -> bool {
    if ORIGINALS.get().is_some() {
        return true;
    }
    let Some(swap) = probe::dummy_swapchain() else {
        hlog!("dxgi: could not create a probe swapchain; not hooking");
        return false;
    };

    // SAFETY: `swap` is a live COM object, and each slot is checked to point
    // into dxgi.dll before it is replaced.
    unsafe {
        let vt = vtable::vtable_of(swap.as_raw());
        let present = vtable::slot(vt, SLOT_PRESENT);
        let resize = vtable::slot(vt, SLOT_RESIZE_BUFFERS);
        // If these do not live in dxgi.dll the index is wrong for this Windows
        // build, or something else got there first. Either way, patching blind
        // would redirect an unknown function inside someone's game.
        for (what, addr) in [("Present", present), ("ResizeBuffers", resize)] {
            if !vtable::is_in_module(addr, "dxgi.dll") {
                hlog!("dxgi: {what} points into {:?}, not dxgi.dll; refusing to patch", vtable::module_of(addr));
                fault::report(HookError::VtableUnexpected, "unexpected DXGI vtable layout");
                return false;
            }
        }

        let Ok(old_present) = vtable::swap(vt, SLOT_PRESENT, present_hook as *mut c_void) else {
            hlog!("dxgi: could not make the Present slot writable");
            return false;
        };
        let old_resize = vtable::swap(vt, SLOT_RESIZE_BUFFERS, resize_buffers_hook as *mut c_void).ok();

        // Present1 only exists on IDXGISwapChain1. Games that use it would
        // otherwise bypass the Present hook entirely.
        let present1 = swap.cast::<IDXGISwapChain1>().ok().and_then(|s1| {
            let vt1 = vtable::vtable_of(s1.as_raw());
            let addr = vtable::slot(vt1, SLOT_PRESENT1);
            vtable::is_in_module(addr, "dxgi.dll")
                .then(|| vtable::swap(vt1, SLOT_PRESENT1, present1_hook as *mut c_void).ok())
                .flatten()
        });

        let _ = ORIGINALS.set(Originals {
            present: std::mem::transmute::<*mut c_void, PresentFn>(old_present),
            present1: present1.map(|p| std::mem::transmute::<*mut c_void, Present1Fn>(p)),
            resize_buffers: std::mem::transmute::<*mut c_void, ResizeBuffersFn>(
                old_resize.unwrap_or(present /* never used; ResizeBuffers is optional */),
            ),
        });
    }
    hlog!("dxgi: hooked Present{}", if ORIGINALS.get().unwrap().present1.is_some() { " and Present1" } else { "" });
    true
}

// ----- the hooks -------------------------------------------------------------

unsafe extern "system" fn present_hook(swap: *mut c_void, interval: u32, flags: u32) -> HRESULT {
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    // A test present renders nothing; counting it would inflate the rate and
    // publishing it would record a frame the player never saw.
    if flags & DXGI_PRESENT_TEST.0 == 0 {
        fault::guard("present", || on_present(swap));
    }
    unsafe { (originals.present)(swap, interval, flags) }
}

unsafe extern "system" fn present1_hook(
    swap: *mut c_void,
    interval: u32,
    flags: u32,
    params: *const c_void,
) -> HRESULT {
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    if flags & DXGI_PRESENT_TEST.0 == 0 {
        fault::guard("present", || on_present(swap));
    }
    match originals.present1 {
        Some(p) => unsafe { p(swap, interval, flags, params) },
        // Present1 was not hooked, so this cannot be reached.
        None => unsafe { (originals.present)(swap, interval, flags) },
    }
}

unsafe extern "system" fn resize_buffers_hook(
    swap: *mut c_void,
    count: u32,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    flags: u32,
) -> HRESULT {
    // Everything referencing the swapchain's buffers has to go *before* the
    // original runs. An outstanding back-buffer reference makes ResizeBuffers
    // fail with DXGI_ERROR_INVALID_CALL, which black-screens the game — the
    // single most common way an overlay breaks one.
    fault::guard("resize", || {
        if let Ok(mut guard) = SURFACE.lock() {
            if let Some(s) = guard.as_mut() {
                s.release_swapchain_resources();
            }
            *guard = None;
        }
        // The renderer's own resources are device-scoped, not swapchain-scoped,
        // but its publisher holds shared textures sized to the old back buffer.
        // Releasing them explicitly (rather than leaving it to drop order) is
        // what guarantees nothing still references a swapchain buffer when the
        // original `ResizeBuffers` runs.
        if let Ok(mut guard) = RENDERER.lock() {
            if let Some(r) = guard.as_mut() {
                r.release_swapchain_resources();
            }
            *guard = None;
        }
    });
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    unsafe { (originals.resize_buffers)(swap, count, width, height, format, flags) }
}

fn on_present(swap: *mut c_void) {
    let Some(shared) = worker::shared() else { return };
    let control = shared.control();

    let now = crate::ipc::qpc();
    let fps = METER.lock().map(|mut m| m.tick(now, control.qpc_freq)).unwrap_or(0.0);
    control.present_count.fetch_add(1, Ordering::Relaxed);
    control.present_fps_milli.store((fps * 1000.0) as u32, Ordering::Relaxed);
    control.heartbeat_qpc.store(now as u64, Ordering::Relaxed);

    let armed = control.armed.load(Ordering::Relaxed) != 0;
    let capturing = control.capturing.load(Ordering::Relaxed) != 0;
    let settings = control.overlay_settings();
    let wants_badge = armed && settings.enabled;
    if !wants_badge && !capturing {
        return;
    }
    let state = if capturing { HookState::Recording } else { HookState::Ready };

    // SAFETY: `swap` is the live swapchain the game just called Present on; the
    // borrow does not take a reference, so nothing is released underneath it.
    let Some(swap) = (unsafe { IDXGISwapChain::from_raw_borrowed(&swap) }) else { return };
    let Ok(mut surface) = SURFACE.lock() else { return };
    if surface.is_none() {
        match select_surface(swap) {
            Some(s) => {
                hlog!("dxgi: {} swapchain; overlay attached", s.api().label());
                control.api.store(s.api() as u32, Ordering::Relaxed);
                *surface = Some(s);
            }
            None => {
                // Nothing retrying will fix: the device behind this swapchain is
                // one we have no backend for. Report it and stop trying.
                fault::report(HookError::NoDevice, "this graphics device is not supported");
                fault::disarm();
                return;
            }
        }
    }
    let Some(surface) = surface.as_mut() else { return };

    let target = match surface.acquire(swap) {
        Ok(t) => t,
        Err(e) => {
            hlog!("dxgi: cannot reach the back buffer: {e}");
            surface.release_swapchain_resources();
            return;
        }
    };

    let outcome = render(control, &target, now, fps, state, settings, capturing, wants_badge);
    surface.release();

    match outcome {
        Ok(true) => shared.signal_ready(),
        Ok(false) => {}
        Err(e) => {
            hlog!("dxgi: {e}");
            // The device may have been reset under us; rebuild on the next frame
            // rather than carrying on with objects that belong to a dead device.
            surface.release_swapchain_resources();
            if let Ok(mut r) = RENDERER.lock() {
                *r = None;
            }
        }
    }
}

/// Publishes and draws in the order `burn_in` asks for. `Ok(true)` means a frame
/// reached openclip.
#[allow(clippy::too_many_arguments)]
fn render(
    control: &Control,
    target: &Target,
    now: i64,
    fps: f32,
    state: HookState,
    settings: openclip_overlay::abi::OverlaySettings,
    capturing: bool,
    wants_badge: bool,
) -> windows::core::Result<bool> {
    let Ok(mut guard) = RENDERER.lock() else { return Ok(false) };
    let rebuild = guard.as_ref().is_none_or(|r| !r.matches(&target.device));
    if rebuild {
        *guard = Some(Renderer::new(&target.device, &target.context)?);
    }
    let renderer = guard.as_mut().expect("built above");

    // Publish before drawing, so the recorded frame is clean and the counter is
    // only on screen. `burn_in` is the request to have it in the file too, and
    // then it has to be painted first.
    let mut published = false;
    if capturing && !settings.burn_in {
        published = renderer.publish(control, target, now)?;
    }
    if wants_badge {
        renderer.draw(target, fps, state, settings)?;
    }
    if capturing && settings.burn_in {
        published = renderer.publish(control, target, now)?;
    }
    Ok(published)
}

/// Picks the backend for a swapchain from the type of device behind it.
fn select_surface(swap: &IDXGISwapChain) -> Option<Box<dyn Surface>> {
    if let Some(s) = crate::d3d11::Surface11::for_swapchain(swap) {
        return Some(Box::new(s));
    }
    // A D3D12 swapchain has no `ID3D11Device` behind it at all, so it lands
    // here. Saying so precisely beats a generic "no device": the counter is
    // already being measured correctly, and only the frame path is missing.
    if crate::d3d12::is_d3d12(swap) {
        hlog!("dxgi: Direct3D 12 swapchain; frame capture for it is not built yet");
        fault::report(HookError::NoDevice, "Direct3D 12 games are not supported yet");
    }
    None
}

// ----- the frame-rate meter --------------------------------------------------

/// The game's present rate over a one-second window, lightly smoothed.
///
/// A window rather than a per-frame reciprocal: `1/frame_time` flickers far too
/// hard to read, which is why every in-game counter averages. The EWMA on top
/// stops the reading jumping when the window boundary lands badly.
pub(crate) struct FpsMeter {
    window_start: i64,
    frames: u32,
    smoothed: f32,
}

impl FpsMeter {
    pub(crate) const fn new() -> Self {
        Self { window_start: 0, frames: 0, smoothed: 0.0 }
    }

    pub(crate) fn tick(&mut self, now: i64, freq: i64) -> f32 {
        if freq <= 0 {
            return self.smoothed;
        }
        if self.window_start == 0 {
            self.window_start = now;
        }
        self.frames += 1;
        let elapsed = now - self.window_start;
        if elapsed >= freq {
            let instant = self.frames as f64 * freq as f64 / elapsed as f64;
            self.smoothed =
                if self.smoothed <= 0.0 { instant as f32 } else { self.smoothed * 0.7 + instant as f32 * 0.3 };
            self.window_start = now;
            self.frames = 0;
        }
        self.smoothed
    }
}

// ----- probing ---------------------------------------------------------------

mod probe {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDeviceAndSwapChain, D3D11_SDK_VERSION};
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    /// A throwaway 1×1 swapchain, purely to read the DXGI vtable off it.
    ///
    /// Every swapchain in the process shares that vtable, so this never has to
    /// find the game's own — which matters, because the game may not have
    /// created it yet when we attach. It is a D3D11 swapchain even in a D3D12
    /// game: the vtable belongs to DXGI, not to the device behind it.
    pub fn dummy_swapchain() -> Option<IDXGISwapChain> {
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC { Width: 1, Height: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM, ..Default::default() },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 1,
            // The desktop window is always valid and is never rendered to: the
            // swapchain is released before this function returns.
            OutputWindow: unsafe { GetDesktopWindow() },
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };
        let mut swap = None;
        let mut device = None;
        // SAFETY: standard device creation; everything is released on drop.
        let created = unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                None,
            )
        };
        created.ok()?;
        swap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_reports_the_rate_over_a_window() {
        // 6 MHz divides evenly by 60, so the window closes exactly on the last
        // present rather than a tick short of it.
        const FREQ: i64 = 6_000_000;
        let step = FREQ / 60;
        let mut m = FpsMeter::new();
        let mut now = FREQ; // a non-zero start, as QPC always is
        for i in 0..=60 {
            let fps = m.tick(now, FREQ);
            // Nothing is reported until the first window closes.
            if i < 60 {
                assert_eq!(fps, 0.0, "reported a rate {i} presents in, before a full window");
            }
            now += step;
        }
        assert!((m.smoothed - 60.0).abs() < 2.0, "expected about 60, got {}", m.smoothed);

        // A second identical window must not drift.
        for _ in 0..60 {
            m.tick(now, FREQ);
            now += step;
        }
        assert!((m.smoothed - 60.0).abs() < 2.0, "drifted to {} over two windows", m.smoothed);
    }

    #[test]
    fn meter_survives_a_broken_clock() {
        let mut m = FpsMeter::new();
        // A zero frequency would divide by zero; it must simply report nothing.
        assert_eq!(m.tick(1_000, 0), 0.0);
    }
}
