//! OpenGL: the `SwapBuffers` hook, the readback and the counter.
//!
//! Attaching is [`crate::iat`]'s job rather than [`crate::vtable`]'s, because
//! there is no COM object here — a GL game calls `gdi32!SwapBuffers` (or the
//! undocumented `opengl32!wglSwapBuffers`, which has the same signature) as a
//! plain imported function. Both are patched; whichever the game actually uses
//! is the one that fires.
//!
//! The order inside the hook is the mirror image of the DXGI one, and has to be:
//!
//! ```text
//! count → read back → publish → draw the counter → chain to the real SwapBuffers
//! ```
//!
//! In DXGI the frame survives `Present`, so the counter can be drawn after the
//! copy. Here the back buffer is *undefined* the moment `SwapBuffers` returns,
//! so everything that reads it must happen first — and the counter still has to
//! be drawn before the swap, or it would never reach the screen.
//!
//! Two things about GL make this more work than the D3D11 path:
//!
//! - **The pixels come back bottom-up.** GL's origin is the lower left, D3D11's
//!   the upper left, so the rows are reversed while they are copied into the
//!   staging texture. It is a copy that has to happen anyway, so the flip is
//!   free.
//! - **A core profile has no fixed-function pipeline.** Minecraft 1.17 and
//!   later ask for OpenGL 3.2 core, where `glBegin`, `glOrtho` and
//!   `glPushAttrib` are all *removed* — and those are exactly what the `windows`
//!   crate binds. So the counter is drawn with a small shader program built from
//!   `wglGetProcAddress`-resolved entry points, and the immediate-mode path is
//!   kept only as a fallback for contexts too old to have shaders.

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use openclip_overlay::abi::{Control, GfxApi, HookError, OverlaySettings};
use openclip_overlay::fps::{self, FpsBadge, HookState};
use openclip_overlay::layout::Corner;
use windows::core::{s, BOOL};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{WindowFromDC, HDC};
use windows::Win32::Graphics::OpenGL::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::fault;
use crate::iat;
use crate::logging::hlog;
use crate::publish::Publisher;
use crate::worker;

mod gl;
use gl::Ext;

type SwapBuffersFn = unsafe extern "system" fn(HDC) -> BOOL;

struct Originals {
    /// `gdi32!SwapBuffers` — what GLFW, SDL and most engines actually call.
    gdi: Option<SwapBuffersFn>,
    /// `opengl32!wglSwapBuffers` — undocumented, so it has no binding in the
    /// `windows` crate and is resolved by name; some engines use it directly.
    wgl: Option<SwapBuffersFn>,
}

static ORIGINALS: OnceLock<Originals> = OnceLock::new();
static STATE: Mutex<Option<GlOverlay>> = Mutex::new(None);
static METER: Mutex<crate::dxgi::FpsMeter> = Mutex::new(crate::dxgi::FpsMeter::new());

/// Points every import of `SwapBuffers` at us.
///
/// Unlike the DXGI patch this is worth repeating: the module that owns the slot
/// (`glfw.dll` for Minecraft) is loaded seconds after the JVM starts, long after
/// the hook's worker thread comes up. The worker calls this on its poll loop and
/// [`iat::patch_all`] skips slots it has already done.
pub fn install() -> bool {
    let gdi = iat::patch_all(&iat::Target {
        module: s!("gdi32.dll"),
        name: s!("SwapBuffers"),
        replacement: gdi_swap_hook as *const c_void,
    });
    let wgl = iat::patch_all(&iat::Target {
        module: s!("opengl32.dll"),
        name: s!("wglSwapBuffers"),
        replacement: wgl_swap_hook as *const c_void,
    });
    if gdi.is_none() && wgl.is_none() {
        return false;
    }
    // The original addresses only need recording once; later passes patch new
    // modules but resolve the same two functions.
    if ORIGINALS.get().is_none() {
        // SAFETY: both addresses came from `GetProcAddress` on the function
        // whose signature this type spells, `BOOL(HDC)` for each.
        let originals = unsafe {
            Originals {
                gdi: gdi.map(|(f, _)| std::mem::transmute::<*const c_void, SwapBuffersFn>(f)),
                wgl: wgl.map(|(f, _)| std::mem::transmute::<*const c_void, SwapBuffersFn>(f)),
            }
        };
        let _ = ORIGINALS.set(originals);
        hlog!(
            "opengl: hooked SwapBuffers in {} module(s) via gdi32 and {} via opengl32",
            gdi.map(|(_, n)| n).unwrap_or(0),
            wgl.map(|(_, n)| n).unwrap_or(0),
        );
    }
    true
}

// ----- the hooks -------------------------------------------------------------

unsafe extern "system" fn gdi_swap_hook(hdc: HDC) -> BOOL {
    fault::guard("swapbuffers", || on_swap(hdc));
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    match originals.gdi {
        Some(f) => unsafe { f(hdc) },
        // Unreachable: this hook is only installed when `gdi` resolved.
        None => BOOL(0),
    }
}

unsafe extern "system" fn wgl_swap_hook(hdc: HDC) -> BOOL {
    fault::guard("wglswapbuffers", || on_swap(hdc));
    let originals = ORIGINALS.get().expect("installed before the hook can run");
    match originals.wgl {
        Some(f) => unsafe { f(hdc) },
        None => BOOL(0),
    }
}

fn on_swap(hdc: HDC) {
    let Some(shared) = worker::shared() else { return };
    let control = shared.control();

    // Unlike DXGI, where one vtable patch covers every swapchain, this fires for
    // every device context in the process — splash screens, tool windows, the
    // launcher. Only the DC that owns the current GL context is the game's.
    // SAFETY: plain queries; a null context simply means this is not our DC.
    let (context, current_dc) = unsafe { (wglGetCurrentContext(), wglGetCurrentDC()) };
    if context.is_invalid() || current_dc != hdc {
        return;
    }
    let Some(size) = client_size(hdc) else { return };

    let now = crate::ipc::qpc();
    let fps = METER.lock().map(|mut m| m.tick(now, control.qpc_freq)).unwrap_or(0.0);
    control.present_count.fetch_add(1, Ordering::Relaxed);
    control.present_fps_milli.store((fps * 1000.0) as u32, Ordering::Relaxed);
    control.heartbeat_qpc.store(now as u64, Ordering::Relaxed);
    control.api.store(GfxApi::OpenGl as u32, Ordering::Relaxed);

    let armed = control.armed.load(Ordering::Relaxed) != 0;
    let capturing = control.capturing.load(Ordering::Relaxed) != 0;
    let settings = control.overlay_settings();
    let wants_badge = armed && settings.enabled;
    if !wants_badge && !capturing {
        return;
    }
    let state = if capturing { HookState::Recording } else { HookState::Ready };

    let Ok(mut guard) = STATE.lock() else { return };
    if guard.as_ref().is_none_or(|o| o.context != context.0 as isize) {
        match GlOverlay::new(context.0 as isize) {
            Ok(o) => *guard = Some(o),
            Err(e) => {
                hlog!("opengl: cannot set up the overlay: {e}");
                fault::report(HookError::NoDevice, "the OpenGL overlay could not be created");
                fault::disarm();
                return;
            }
        }
    }
    let Some(overlay) = guard.as_mut() else { return };

    // Read back before drawing, so the recorded frame is clean and the counter
    // is only on screen; `burn_in` is the request to have it in the file too,
    // and then it has to be painted first.
    let mut published = false;
    if capturing && !settings.burn_in {
        published = overlay.publish(control, size, now);
    }
    if wants_badge {
        overlay.draw(size, fps, state, settings);
    }
    if capturing && settings.burn_in {
        published = overlay.publish(control, size, now);
    }
    // Everything above touched GL. Whatever it provoked has to be cleared here:
    // a game that polls `glGetError` in its own render loop would otherwise
    // attribute our error to its own draw — which is exactly how an
    // `IllegalStateException: OpenGL error 1282` crash report gets written.
    // SAFETY: still on the render thread with the game's context current.
    unsafe { gl::drain_errors() };

    if published {
        shared.signal_ready();
    }
}

/// The size of the window behind a device context — the true size of the
/// default framebuffer, which the viewport may well not be.
fn client_size(hdc: HDC) -> Option<(u32, u32)> {
    // SAFETY: read-only window queries; a DC with no window reports failure.
    unsafe {
        let hwnd = WindowFromDC(hdc);
        if hwnd == HWND::default() {
            return None;
        }
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).ok()?;
        let (w, h) = ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32);
        (w > 0 && h > 0).then_some((w, h))
    }
}

// ----- the overlay -----------------------------------------------------------

/// Everything tied to one GL context.
///
/// Keyed on the `HGLRC` because every one of these objects — the badge texture,
/// the shader program, the resolved extension pointers — belongs to the context
/// that created it. A game that recreates its context (a resolution change on
/// some engines does) gets a fresh set rather than silently drawing nothing.
struct GlOverlay {
    context: isize,
    ext: Ext,
    badge: FpsBadge,
    /// The uploaded sprite, its dimensions, and what it currently reads.
    texture: u32,
    tex_size: (u32, u32),
    shown: Option<(String, [u8; 3])>,
    draw: gl::Painter,
    /// The D3D11 side, built the first time a frame is actually wanted.
    bridge: Option<Bridge>,
    /// One reusable buffer for `glReadPixels`, so a 1080p readback is not an
    /// 8 MB allocation on the render thread every frame.
    pixels: Vec<u8>,
    /// Whether the one-time state description has been logged.
    described: bool,
    /// Set once capture has been refused, so the note is not rewritten every
    /// frame.
    refused: bool,
    /// The pixel format this implementation will actually accept for a
    /// readback. Negotiated once per context; see [`gl::ReadFormat`].
    read: gl::ReadFormat,
}

impl GlOverlay {
    fn new(context: isize) -> Result<Self, String> {
        let ext = Ext::load();
        let draw = gl::Painter::new(&ext)?;
        // SAFETY: the swap hook runs on the render thread with this context
        // current, which is what the queries need.
        let read = unsafe { gl::ReadFormat::negotiate() };
        hlog!(
            "opengl: context {context:#x}, {} — {draw}, reading {}",
            gl::version_string(),
            if read.bgra { "BGRA" } else { "RGBA" },
        );
        if !read.readable {
            hlog!("opengl: multisampled default framebuffer — the counter will show but frames cannot be read");
        }
        Ok(Self {
            context,
            ext,
            badge: FpsBadge::new().ok_or("the bundled font could not be parsed")?,
            texture: 0,
            tex_size: (0, 0),
            shown: None,
            draw,
            bridge: None,
            pixels: Vec::new(),
            described: false,
            refused: false,
            read,
        })
    }

    /// Reads the back buffer and hands it to openclip. Returns whether a frame
    /// was actually sent.
    fn publish(&mut self, control: &Control, size: (u32, u32), now: i64) -> bool {
        let bridge = match &mut self.bridge {
            Some(b) => b,
            None => match Bridge::new() {
                Ok(b) => self.bridge.insert(b),
                Err(e) => {
                    hlog!("opengl: no Direct3D device for the frame transport: {e}");
                    fault::report(HookError::NoDevice, "no Direct3D device for the OpenGL transport");
                    fault::disarm();
                    return false;
                }
            },
        };
        if !self.read.readable {
            if !self.refused {
                self.refused = true;
                hlog!("opengl: the default framebuffer is multisampled; frames cannot be read from it");
                fault::report(
                    HookError::FormatUnsupported,
                    "this game's OpenGL surface is multisampled and cannot be captured",
                );
            }
            return false;
        }
        // `glReadPixels` stalls the GL pipeline, so the rate limiter has to be
        // consulted *before* paying for it — the opposite of the D3D11 path,
        // where the copy is a GPU-side blit and asking afterwards is fine.
        if !bridge.publisher.due(control, now) {
            return false;
        }

        let (w, h) = size;
        self.pixels.resize(w as usize * h as usize * 4, 0);
        // SAFETY: the buffer is exactly `w * h * 4` bytes, which is what
        // BGRA/UNSIGNED_BYTE at 4-byte pack alignment writes for this rectangle.
        unsafe {
            // A game that left a framebuffer bound would otherwise have that
            // read instead of the back buffer, and one that left a pixel pack
            // buffer bound would have the pixels written into *it* rather than
            // into the buffer whose pointer is passed here.
            let saved_fbo = self.ext.read_framebuffer();
            self.ext.bind_read_framebuffer(0);
            let store = gl::PixelStore::take_pack(&self.ext, 4);
            glReadPixels(
                0,
                0,
                w as i32,
                h as i32,
                self.read.gl_format,
                GL_UNSIGNED_BYTE,
                self.pixels.as_mut_ptr() as *mut c_void,
            );
            store.restore(&self.ext);
            self.ext.bind_read_framebuffer(saved_fbo);
        }

        match bridge.upload_and_publish(control, &self.pixels, size, self.read, now) {
            Ok(sent) => sent,
            Err(e) => {
                hlog!("opengl: {e}");
                self.bridge = None;
                false
            }
        }
    }

    fn draw(&mut self, size: (u32, u32), fps: f32, state: HookState, settings: OverlaySettings) {
        if !self.described {
            self.described = true;
            // SAFETY: the swap hook runs on the render thread with the game's
            // context current, which is what these queries need.
            hlog!("opengl: game state at first draw — {}", unsafe { gl::describe_state(&self.ext) });
        }
        let (fw, fh) = size;
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
            return; // too small a window to put a counter on
        };
        let changed = self.shown.as_ref().is_none_or(|(t, c)| t != &text || c != &rgb);
        if changed {
            let pixels = sprite.rgba.clone();
            self.upload(sw, sh, &pixels);
            self.shown = Some((text, rgb));
        }
        if self.texture == 0 {
            return;
        }

        // Clip space: x right, y **up** — the opposite of D3D11 — so the height
        // is negative to walk down from the top edge, exactly as the D3D11 path
        // does, and the sprite's first row lands at the top.
        let rect = [
            (x as f32 / fw as f32) * 2.0 - 1.0,
            1.0 - (y as f32 / fh as f32) * 2.0,
            (sw as f32 / fw as f32) * 2.0,
            -(sh as f32 / fh as f32) * 2.0,
        ];
        let opacity = (overlay.opacity.min(100) as f32) / 100.0;
        // SAFETY: the painter saves every piece of state it touches and puts it
        // back before returning; see `gl::Painter::paint`.
        unsafe { self.draw.paint(&self.ext, self.texture, rect, opacity, size) };
    }

    /// Puts the composed sprite into a GL texture the painter can sample.
    fn upload(&mut self, w: u32, h: u32, rgba: &[u8]) {
        // SAFETY: standard GL 1.1 texture calls on the game's current context;
        // the previous binding is restored so nothing is left changed.
        unsafe {
            let mut previous = 0i32;
            glGetIntegerv(GL_TEXTURE_BINDING_2D, &mut previous);
            if self.texture == 0 {
                glGenTextures(1, &mut self.texture);
                if self.texture == 0 {
                    return;
                }
            }
            glBindTexture(GL_TEXTURE_2D, self.texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR as i32);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR as i32);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, gl::GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, gl::GL_CLAMP_TO_EDGE);
            // Say explicitly that level 0 is the only level there is. A texture
            // whose mip chain is declared but not supplied is *incomplete*, and
            // an incomplete texture samples as opaque black rather than failing.
            glTexParameteri(GL_TEXTURE_2D, gl::GL_TEXTURE_BASE_LEVEL, 0);
            glTexParameteri(GL_TEXTURE_2D, gl::GL_TEXTURE_MAX_LEVEL, 0);
            let store = gl::PixelStore::take_unpack(&self.ext, 4);
            if (w, h) == self.tex_size {
                glTexSubImage2D(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    rgba.as_ptr() as *const c_void,
                );
            } else {
                glTexImage2D(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA as i32,
                    w as i32,
                    h as i32,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    rgba.as_ptr() as *const c_void,
                );
                self.tex_size = (w, h);
            }
            store.restore(&self.ext);
            glBindTexture(GL_TEXTURE_2D, previous as u32);
        }
    }
}

impl Drop for GlOverlay {
    fn drop(&mut self) {
        // Only safe while the context that owns them is current, which it is:
        // the overlay is dropped from inside the swap hook.
        if self.texture != 0 {
            // SAFETY: a texture name this object created on the current context.
            unsafe { glDeleteTextures(1, &self.texture) };
        }
        self.draw.destroy(&self.ext);
    }
}

// ----- the Direct3D side -----------------------------------------------------

/// Gets OpenGL pixels into openclip's shared texture.
///
/// The shared-texture transport is Direct3D 11 all the way through (see
/// [`crate::publish`]), so the GL backend keeps a small D3D11 device of its own
/// purely to stage into. That device is ours, not the game's, so the adapter it
/// lands on is the one openclip is told to open — which is what keeps a hybrid
/// laptop from handing over a texture the other GPU cannot read.
struct Bridge {
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    staging: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    size: (u32, u32),
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    publisher: Publisher,
}

impl Bridge {
    fn new() -> windows::core::Result<Self> {
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION};

        let mut device = None;
        let mut context = None;
        // SAFETY: standard device creation; both out-params are checked below.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.expect("created above");
        let context = context.expect("created above");
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            staging: None,
            size: (0, 0),
            format: Default::default(),
            publisher: Publisher::new(device, context),
        })
    }

    /// Copies a bottom-up BGRA readback into the staging texture the right way
    /// up, then hands it to the publisher.
    fn upload_and_publish(
        &mut self,
        control: &Control,
        pixels: &[u8],
        size: (u32, u32),
        read: gl::ReadFormat,
        now: i64,
    ) -> windows::core::Result<bool> {
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
        };

        // The staging texture takes whatever channel order the readback could
        // legally be asked for, so no swizzle ever runs on the render thread —
        // the pipeline handles both orders already.
        let format = if read.bgra { DXGI_FORMAT_B8G8R8A8_UNORM } else { DXGI_FORMAT_R8G8B8A8_UNORM };
        let (w, h) = size;
        if self.staging.is_none() || self.size != size || self.format != format {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut tex = None;
            // SAFETY: a plain 2D texture on our own device.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            self.staging = tex;
            self.size = size;
            self.format = format;
        }
        let Some(staging) = &self.staging else { return Ok(false) };

        let row = w as usize * 4;
        if pixels.len() < row * h as usize {
            return Ok(false);
        }
        // SAFETY: a dynamic texture, mapped for write and unmapped below. Rows
        // are walked in reverse because GL's origin is the *lower* left, and
        // the destination row respects the pitch the driver reports.
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            for y in 0..h as usize {
                let src = pixels.as_ptr().add((h as usize - 1 - y) * row);
                let dst = (m.pData as *mut u8).add(y * m.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, dst, row);
            }
            self.context.Unmap(staging, 0);
        }
        self.publisher.publish_due(control, staging, now)
    }
}
