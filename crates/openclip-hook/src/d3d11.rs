//! Direct3D 11 / DXGI: the present hook, the frame-rate meter and the counter.
//!
//! The hook attaches by pointing three DXGI vtable slots at us (see
//! [`crate::vtable`]). Every swapchain in the process shares that vtable, so one
//! patch covers the game's real swapchain, any launcher window, and any
//! swapchain created later — which is what makes alt-enter and fullscreen
//! transitions a non-event here.
//!
//! Order inside `Present` matters and is deliberate: the counter is drawn
//! *after* the frame has been published for recording, so the number a player
//! sees on screen does not end up baked into the file. `burn_in` swaps the two.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use openclip_overlay::abi::{GfxApi, HookError, OverlaySettings};
use openclip_overlay::fps::{FpsBadge, HookState};
use openclip_overlay::layout::Corner;
use openclip_overlay::fps;
use windows::core::{Interface, HRESULT};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R8G8B8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{IDXGISwapChain, IDXGISwapChain1, DXGI_PRESENT_TEST, DXGI_SWAP_CHAIN_DESC};

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
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);
static METER: Mutex<FpsMeter> = Mutex::new(FpsMeter::new());
/// Panics caught inside the hooks. At [`MAX_FAULTS`] we stop drawing for good:
/// a broken overlay is a nuisance, a game that crashes every frame is not.
static FAULTS: AtomicU32 = AtomicU32::new(0);
const MAX_FAULTS: u32 = 3;

/// Points the DXGI vtable at us. Safe to call more than once; only the first
/// call does anything.
pub fn install() -> bool {
    if ORIGINALS.get().is_some() {
        return true;
    }
    let Some(swap) = probe::dummy_swapchain() else {
        hlog!("d3d11: could not create a probe swapchain; not hooking");
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
                hlog!("d3d11: {what} points into {:?}, not dxgi.dll; refusing to patch", vtable::module_of(addr));
                report_error(HookError::VtableUnexpected, "unexpected DXGI vtable layout");
                return false;
            }
        }

        let Ok(old_present) = vtable::swap(vt, SLOT_PRESENT, present_hook as *mut c_void) else {
            hlog!("d3d11: could not make the Present slot writable");
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
    hlog!("d3d11: hooked Present{}", if ORIGINALS.get().unwrap().present1.is_some() { " and Present1" } else { "" });
    true
}

fn report_error(code: HookError, detail: &str) {
    let Some(shared) = worker::shared() else { return };
    let control = shared.control();
    control.error_code.store(code as u32, Ordering::Relaxed);
    // SAFETY: `error_text` is a plain byte array in the shared mapping and this
    // is the only writer.
    unsafe {
        let text = &raw const control.error_text as *mut [u8; 160];
        openclip_overlay::abi::write_cstr(&mut *text, detail);
    }
}

// ----- the hooks -------------------------------------------------------------

unsafe extern "system" fn present_hook(swap: *mut c_void, interval: u32, flags: u32) -> HRESULT {
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    // A test present renders nothing; counting it would inflate the rate and
    // publishing it would record a frame the player never saw.
    if flags & DXGI_PRESENT_TEST.0 == 0 {
        guard(|| on_present(swap));
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
        guard(|| on_present(swap));
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
    guard(|| {
        if let Ok(mut r) = RENDERER.lock() {
            *r = None;
        }
    });
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    unsafe { (originals.resize_buffers)(swap, count, width, height, format, flags) }
}

/// Runs overlay work, swallowing panics.
///
/// Rust has no stable SEH, so this cannot catch a hardware fault — but it does
/// catch our own bugs, and killing someone's game over one would be far worse
/// than losing the counter. After [`MAX_FAULTS`] the hook stops trying.
fn guard(f: impl FnOnce()) {
    if crate::shutting_down() || FAULTS.load(Ordering::Relaxed) >= MAX_FAULTS {
        return;
    }
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        let n = FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
        hlog!("panic inside the present hook ({n}/{MAX_FAULTS})");
        if n >= MAX_FAULTS {
            hlog!("disarming: too many faults");
            report_error(HookError::SelfDisarmed, "the overlay faulted repeatedly and stopped itself");
        }
    }
}

fn on_present(swap: *mut c_void) {
    let Some(shared) = worker::shared() else { return };
    let control = shared.control();

    let now = crate::ipc::qpc();
    let fps = METER.lock().map(|mut m| m.tick(now, control.qpc_freq)).unwrap_or(0.0);
    control.present_count.fetch_add(1, Ordering::Relaxed);
    control.present_fps_milli.store((fps * 1000.0) as u32, Ordering::Relaxed);
    control.heartbeat_qpc.store(now as u64, Ordering::Relaxed);
    control.api.store(GfxApi::D3D11 as u32, Ordering::Relaxed);

    if control.armed.load(Ordering::Relaxed) == 0 {
        return;
    }
    let settings = control.overlay_settings();
    if !settings.enabled {
        return;
    }
    let state = if control.capturing.load(Ordering::Relaxed) != 0 { HookState::Recording } else { HookState::Ready };

    // SAFETY: `swap` is the live swapchain the game just called Present on; the
    // borrow does not take a reference, so nothing is released underneath it.
    let Some(swap) = (unsafe { IDXGISwapChain::from_raw_borrowed(&swap) }) else { return };
    let Ok(mut guard) = RENDERER.lock() else { return };
    if guard.is_none() {
        match Renderer::new(swap) {
            Ok(r) => *guard = Some(r),
            Err(e) => {
                hlog!("d3d11: cannot set up the overlay: {e}");
                report_error(HookError::NoDevice, "the overlay could not be created on this device");
                FAULTS.store(MAX_FAULTS, Ordering::Relaxed);
                return;
            }
        }
    }
    if let Some(r) = guard.as_mut()
        && let Err(e) = r.draw(swap, fps, state, settings)
    {
        hlog!("d3d11: overlay draw failed: {e}");
        *guard = None; // rebuild next frame; the device may have been reset
    }
}

// ----- the frame-rate meter --------------------------------------------------

/// The game's present rate over a one-second window, lightly smoothed.
///
/// A window rather than a per-frame reciprocal: `1/frame_time` flickers far too
/// hard to read, which is why every in-game counter averages. The EWMA on top
/// stops the reading jumping when the window boundary lands badly.
struct FpsMeter {
    window_start: i64,
    frames: u32,
    smoothed: f32,
}

impl FpsMeter {
    const fn new() -> Self {
        Self { window_start: 0, frames: 0, smoothed: 0.0 }
    }

    fn tick(&mut self, now: i64, freq: i64) -> f32 {
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

// ----- the overlay renderer --------------------------------------------------

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    rtv: ID3D11RenderTargetView,
    /// Back-buffer size, so the counter scales with the game's resolution.
    size: (u32, u32),
    badge: FpsBadge,
    /// The uploaded sprite and the dimensions it was made for.
    texture: Option<(u32, u32, ID3D11Texture2D, ID3D11ShaderResourceView)>,
    /// What the uploaded texture currently shows.
    shown: Option<(String, [u8; 3])>,
}

impl Renderer {
    fn new(swap: &IDXGISwapChain) -> windows::core::Result<Self> {
        // SAFETY: standard D3D11 resource creation on the game's own device.
        unsafe {
            let device: ID3D11Device = swap.GetDevice()?;
            let context = device.GetImmediateContext()?;
            let desc: DXGI_SWAP_CHAIN_DESC = swap.GetDesc()?;

            let mut vs = None;
            device.CreateVertexShader(include_bytes!("../shaders/overlay_vs.dxbc"), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(include_bytes!("../shaders/overlay_ps.dxbc"), None, Some(&mut ps))?;

            // Straight alpha, which is what the sprite carries.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&blend_desc, Some(&mut blend))?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: 32, // two float4 registers
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            device.CreateBuffer(&cb_desc, None, Some(&mut constants))?;

            let back: ID3D11Texture2D = swap.GetBuffer(0)?;
            let mut rtv = None;
            device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;

            Ok(Self {
                device,
                context,
                vs: vs.expect("created above"),
                ps: ps.expect("created above"),
                blend: blend.expect("created above"),
                sampler: sampler.expect("created above"),
                constants: constants.expect("created above"),
                rtv: rtv.expect("created above"),
                size: (desc.BufferDesc.Width, desc.BufferDesc.Height),
                badge: FpsBadge::new().ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?,
                texture: None,
                shown: None,
            })
        }
    }

    fn draw(
        &mut self,
        _swap: &IDXGISwapChain,
        fps: f32,
        state: HookState,
        settings: OverlaySettings,
    ) -> windows::core::Result<()> {
        let (fw, fh) = self.size;
        let overlay = fps::FpsOverlay {
            enabled: settings.enabled,
            position: Corner::ALL[(settings.corner as usize).min(3)],
            size: settings.size as u32,
            opacity: settings.opacity as u32,
            in_recording: settings.burn_in,
        };
        let height = overlay.badge_height(fh);
        let text = fps::format_fps(fps);
        let rgb = state.color();

        let sprite = self.badge.sprite_for(height, &text, rgb);
        let (sw, sh) = (sprite.width, sprite.height);
        let Some((x, y)) = overlay.place((sw, sh), (fw, fh)) else {
            return Ok(()); // too small a window to put a counter on
        };
        // `sprite` borrows `self.badge`; copy what the upload needs and let go.
        let changed = self.shown.as_ref().is_none_or(|(t, c)| t != &text || c != &rgb);
        if changed {
            let pixels = sprite.rgba.clone();
            self.upload(sw, sh, &pixels)?;
            self.shown = Some((text, rgb));
        }
        let Some((_, _, _, srv)) = &self.texture else { return Ok(()) };
        let srv = srv.clone();

        // Clip space: x right, y up, origin centre.
        let ndc = [
            (x as f32 / fw as f32) * 2.0 - 1.0,
            1.0 - (y as f32 / fh as f32) * 2.0,
            (sw as f32 / fw as f32) * 2.0,
            -(sh as f32 / fh as f32) * 2.0,
        ];
        let opacity = (overlay.opacity.min(100) as f32) / 100.0;

        // SAFETY: every call below is on the game's immediate context, wrapped
        // by a state block that puts back everything we touch.
        unsafe {
            let mapped = {
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(&self.constants, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                m
            };
            let data = mapped.pData as *mut f32;
            std::ptr::copy_nonoverlapping(ndc.as_ptr(), data, 4);
            *data.add(4) = opacity;
            *data.add(5) = 0.0;
            *data.add(6) = 0.0;
            *data.add(7) = 0.0;
            self.context.Unmap(&self.constants, 0);

            let saved = StateBlock::capture(&self.context);
            self.set_state(&srv, fw, fh);
            self.context.Draw(4, 0);
            saved.restore(&self.context);
        }
        Ok(())
    }

    /// Binds our pipeline. Everything set here is captured by [`StateBlock`].
    ///
    /// The quad comes from `SV_VertexID`, so there is no vertex buffer and no
    /// input layout to bind — one less piece of state to get wrong.
    unsafe fn set_state(&self, srv: &ID3D11ShaderResourceView, fw: u32, fh: u32) {
        let ctx = &self.context;
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: fw as f32,
                Height: fh as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.RSSetState(None);
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            ctx.OMSetBlendState(&self.blend, Some(&[0.0; 4]), 0xffff_ffff);
            ctx.OMSetDepthStencilState(None, 0);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            // A geometry shader would run over our triangles and draw nonsense.
            // Hull and domain shaders are ignored for a non-patch topology, so
            // they can be left exactly as the game had them.
            ctx.GSSetShader(None, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
    }

    /// Puts the composed sprite into a texture the shader can sample.
    fn upload(&mut self, w: u32, h: u32, rgba: &[u8]) -> windows::core::Result<()> {
        let fits = matches!(&self.texture, Some((tw, th, ..)) if *tw == w && *th == h);
        if !fits {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            // SAFETY: a plain 2D texture plus its view, both checked below.
            unsafe {
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.expect("created above");
                let mut srv = None;
                self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
                self.texture = Some((w, h, tex, srv.expect("created above")));
            }
        }
        let Some((_, _, tex, _)) = &self.texture else { return Ok(()) };
        // SAFETY: dynamic texture, mapped for write and unmapped below; the row
        // loop respects the pitch the driver hands back.
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            let row = w as usize * 4;
            for y in 0..h as usize {
                let dst = (m.pData as *mut u8).add(y * m.RowPitch as usize);
                std::ptr::copy_nonoverlapping(rgba.as_ptr().add(y * row), dst, row);
            }
            self.context.Unmap(tex, 0);
        }
        Ok(())
    }
}

/// Everything [`Renderer::set_state`] touches, so the game gets it all back.
///
/// The `*Get*` methods add a reference to whatever they return; the `windows`
/// crate's smart pointers release it on drop, which removes the leak that hand-
/// written versions of this in C++ are famous for.
struct StateBlock {
    viewports: Vec<D3D11_VIEWPORT>,
    rasterizer: Option<ID3D11RasterizerState>,
    render_targets: [Option<ID3D11RenderTargetView>; 8],
    depth: Option<ID3D11DepthStencilView>,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    depth_stencil: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    input_layout: Option<ID3D11InputLayout>,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    vs: Option<ID3D11VertexShader>,
    ps: Option<ID3D11PixelShader>,
    gs: Option<ID3D11GeometryShader>,
    vs_cb: [Option<ID3D11Buffer>; 1],
    ps_cb: [Option<ID3D11Buffer>; 1],
    ps_srv: [Option<ID3D11ShaderResourceView>; 1],
    ps_sampler: [Option<ID3D11SamplerState>; 1],
}

impl StateBlock {
    /// # Safety
    /// `ctx` must be the immediate context the draw will run on.
    unsafe fn capture(ctx: &ID3D11DeviceContext) -> Self {
        unsafe {
            // One more than the maximum index: the full set D3D11 can hold.
            let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_MAX_INDEX + 1;
            let mut viewports = vec![D3D11_VIEWPORT::default(); count as usize];
            ctx.RSGetViewports(&mut count, Some(viewports.as_mut_ptr()));
            viewports.truncate(count as usize);

            let mut render_targets: [Option<ID3D11RenderTargetView>; 8] = Default::default();
            let mut depth = None;
            ctx.OMGetRenderTargets(Some(&mut render_targets), Some(&mut depth));

            let mut blend = None;
            let mut blend_factor = [0.0f32; 4];
            let mut sample_mask = 0u32;
            ctx.OMGetBlendState(Some(&mut blend), Some(&mut blend_factor), Some(&mut sample_mask));

            let mut depth_stencil = None;
            let mut stencil_ref = 0u32;
            ctx.OMGetDepthStencilState(Some(&mut depth_stencil), Some(&mut stencil_ref));

            let topology = ctx.IAGetPrimitiveTopology();
            let input_layout = ctx.IAGetInputLayout().ok();

            let mut vs = None;
            ctx.VSGetShader(&mut vs, None, None);
            let mut ps = None;
            ctx.PSGetShader(&mut ps, None, None);
            let mut gs = None;
            ctx.GSGetShader(&mut gs, None, None);

            let mut vs_cb: [Option<ID3D11Buffer>; 1] = Default::default();
            ctx.VSGetConstantBuffers(0, Some(&mut vs_cb));
            let mut ps_cb: [Option<ID3D11Buffer>; 1] = Default::default();
            ctx.PSGetConstantBuffers(0, Some(&mut ps_cb));
            let mut ps_srv: [Option<ID3D11ShaderResourceView>; 1] = Default::default();
            ctx.PSGetShaderResources(0, Some(&mut ps_srv));
            let mut ps_sampler: [Option<ID3D11SamplerState>; 1] = Default::default();
            ctx.PSGetSamplers(0, Some(&mut ps_sampler));

            Self {
                viewports,
                rasterizer: ctx.RSGetState().ok(),
                render_targets,
                depth,
                blend,
                blend_factor,
                sample_mask,
                depth_stencil,
                stencil_ref,
                input_layout,
                topology,
                vs,
                ps,
                gs,
                vs_cb,
                ps_cb,
                ps_srv,
                ps_sampler,
            }
        }
    }

    /// # Safety
    /// Must be called on the same context [`capture`](Self::capture) read.
    unsafe fn restore(self, ctx: &ID3D11DeviceContext) {
        unsafe {
            ctx.RSSetViewports(Some(&self.viewports));
            ctx.RSSetState(self.rasterizer.as_ref());
            ctx.OMSetRenderTargets(Some(&self.render_targets), self.depth.as_ref());
            ctx.OMSetBlendState(self.blend.as_ref(), Some(&self.blend_factor), self.sample_mask);
            ctx.OMSetDepthStencilState(self.depth_stencil.as_ref(), self.stencil_ref);
            ctx.IASetInputLayout(self.input_layout.as_ref());
            ctx.IASetPrimitiveTopology(self.topology);
            ctx.VSSetShader(self.vs.as_ref(), None);
            ctx.PSSetShader(self.ps.as_ref(), None);
            ctx.GSSetShader(self.gs.as_ref(), None);
            ctx.VSSetConstantBuffers(0, Some(&self.vs_cb));
            ctx.PSSetConstantBuffers(0, Some(&self.ps_cb));
            ctx.PSSetShaderResources(0, Some(&self.ps_srv));
            ctx.PSSetSamplers(0, Some(&self.ps_sampler));
        }
    }
}

// ----- probing ---------------------------------------------------------------

mod probe {
    use super::*;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    /// A throwaway 1×1 swapchain, purely to read the DXGI vtable off it.
    ///
    /// Every swapchain in the process shares that vtable, so this never has to
    /// find the game's own — which matters, because the game may not have
    /// created it yet when we attach.
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
