//! Handing the game's back buffer to openclip.
//!
//! The frame never leaves the GPU: it is copied into a shared texture that
//! openclip opens in its own process and reads back there. That is the whole
//! reason game mode is faster than desktop capture — no compositor round trip,
//! and it works in exclusive fullscreen where there is no desktop to capture.
//!
//! Two textures, alternated, each guarded by its own keyed mutex: the hook fills
//! one while openclip is still reading the other, so a slow readback can never
//! stall a game's present. The convention is `AcquireSync(0)` → write →
//! `ReleaseSync(1)` here, and `AcquireSync(1)` → read → `ReleaseSync(0)` there.
//!
//! Sharing is **by name** (`IDXGIResource1::CreateSharedHandle` with a name,
//! `ID3D11Device1::OpenSharedResourceByName` on the other side). The alternative
//! is duplicating an NT handle across processes, which needs openclip to open
//! the game with `PROCESS_DUP_HANDLE` — a right anti-cheat quite reasonably
//! treats as hostile, and which this design never asks for.

use std::sync::atomic::Ordering;

use openclip_overlay::abi::{self, Control, HookError, Transport, TEX_SLOTS};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIKeyedMutex, IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

use crate::logging::hlog;

/// One shared texture and the keyed mutex guarding it.
struct Slot {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    /// Kept open for as long as the texture is shared; the name is what the
    /// other side actually uses, but closing this would revoke it.
    ///
    /// Held as an `isize` rather than a `HANDLE` so the publisher stays `Send`:
    /// it lives in a `static Mutex` and is touched from whichever game thread
    /// happens to be presenting.
    handle: isize,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.handle == 0 {
            return;
        }
        // SAFETY: a handle created by `CreateSharedHandle` and owned here.
        let _ = unsafe { CloseHandle(HANDLE(self.handle as *mut std::ffi::c_void)) };
    }
}

pub struct Publisher {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    slots: Vec<Slot>,
    size: (u32, u32),
    format: DXGI_FORMAT,
    generation: u64,
    next: usize,
    /// Landing pad for a multisampled back buffer, which cannot be copied
    /// straight into a single-sampled shared texture.
    resolve: Option<ID3D11Texture2D>,
    limiter: Decimator,
}

impl Publisher {
    pub fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Self {
        Self {
            device,
            context,
            slots: Vec::new(),
            size: (0, 0),
            format: DXGI_FORMAT(0),
            generation: 0,
            next: 0,
            resolve: None,
            limiter: Decimator::new(),
        }
    }

    /// Copies `back` into the next shared slot and tells openclip about it.
    ///
    /// Returns `false` when this present was skipped — either the rate limiter
    /// dropped it, or openclip has not finished with the slot. Neither is worth
    /// reporting: a game presenting at 300 fps into a 60 fps recording is
    /// *expected* to have most of its frames skipped.
    pub fn publish(&mut self, control: &Control, back: &ID3D11Texture2D, now_qpc: i64) -> windows::core::Result<bool> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `back` is the swapchain's own buffer.
        unsafe { back.GetDesc(&mut desc) };

        let fps = control.capture_fps.load(Ordering::Relaxed).max(1);
        if !self.limiter.accept(now_qpc, control.qpc_freq, fps) {
            return Ok(false);
        }

        self.ensure(control, desc.Width, desc.Height, desc.Format, desc.SampleDesc.Count)?;
        if self.slots.is_empty() {
            return Ok(false);
        }

        let slot_index = self.next;
        let slot = &self.slots[slot_index];
        // A zero timeout is deliberate: if openclip still holds this slot, drop
        // the frame rather than stalling the game's render thread on it.
        if unsafe { slot.mutex.AcquireSync(0, 0) }.is_err() {
            return Ok(false);
        }

        // SAFETY: both textures are ours and match in size and format; the
        // keyed mutex is held for the duration of the copy.
        unsafe {
            if desc.SampleDesc.Count > 1 {
                let Some(resolve) = &self.resolve else {
                    let _ = slot.mutex.ReleaseSync(1);
                    return Ok(false);
                };
                self.context.ResolveSubresource(resolve, 0, back, 0, desc.Format);
                self.context.CopyResource(&slot.texture, resolve);
            } else {
                self.context.CopyResource(&slot.texture, back);
            }
            // The copy has to be on its way before openclip is told the slot is
            // ready, or it can map a texture the GPU has not written yet.
            self.context.Flush();
            slot.mutex.ReleaseSync(1)?;
        }

        control.qpc[slot_index].store(now_qpc as u64, Ordering::Relaxed);
        control.slot.store(slot_index as u32, Ordering::Relaxed);
        // Release ordering: everything above must be visible to openclip before
        // it can observe the new sequence number.
        control.frame_seq.fetch_add(1, Ordering::Release);
        self.next = (self.next + 1) % self.slots.len();
        Ok(true)
    }

    /// Drops every shared resource. Called before `ResizeBuffers` runs, because
    /// nothing may still reference the swapchain's buffers at that point.
    pub fn release(&mut self) {
        self.slots.clear();
        self.resolve = None;
        self.size = (0, 0);
    }

    fn ensure(
        &mut self,
        control: &Control,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        samples: u32,
    ) -> windows::core::Result<()> {
        if self.size == (w, h) && self.format == format && !self.slots.is_empty() {
            return Ok(());
        }
        self.release();
        self.generation += 1;
        let generation = self.generation;
        let pid = std::process::id();

        // SAFETY: plain resource creation on the game's own device.
        unsafe {
            for index in 0..TEX_SLOTS {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let texture = tex.expect("created above");

                let name = HSTRING::from(abi::texture_name(pid, index, generation));
                let resource: IDXGIResource1 = texture.cast()?;
                let handle = resource.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR(name.as_ptr()),
                )?;
                let mutex: IDXGIKeyedMutex = texture.cast()?;
                self.slots.push(Slot { texture, mutex, handle: handle.0 as isize });
            }

            if samples > 1 {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                self.resolve = tex;
            }
        }

        self.size = (w, h);
        self.format = format;
        self.next = 0;

        // Publish the names before the generation, so openclip never sees a
        // generation it cannot yet resolve a texture name for.
        for index in 0..TEX_SLOTS {
            // SAFETY: `tex_name` is a plain byte array in the shared mapping and
            // this is its only writer.
            unsafe {
                let field = (&raw const control.tex_name[index]) as *mut [u8; abi::NAME_MAX];
                abi::write_cstr(&mut *field, &abi::texture_name(pid, index, generation));
            }
        }
        control.width.store(w, Ordering::Relaxed);
        control.height.store(h, Ordering::Relaxed);
        control.dxgi_format.store(format.0 as u32, Ordering::Relaxed);
        control.adapter_luid.store(self.adapter_luid(), Ordering::Relaxed);
        control.transport.store(Transport::SharedTexture as u32, Ordering::Relaxed);
        control.generation.store(generation, Ordering::Release);
        hlog!("publishing {w}×{h} format {:?} as generation {generation}", format.0);
        Ok(())
    }

    /// The adapter the shared textures live on.
    ///
    /// openclip has to open them on the *same* adapter. On a hybrid laptop the
    /// game runs on the discrete GPU and openclip may not, and a cross-adapter
    /// shared texture simply does not open — this is the difference between
    /// working and mysteriously not.
    fn adapter_luid(&self) -> u64 {
        // SAFETY: querying the device for its adapter; every step is checked.
        unsafe {
            let Ok(dxgi) = self.device.cast::<IDXGIDevice>() else { return 0 };
            let Ok(adapter) = dxgi.GetAdapter() else { return 0 };
            let Ok(desc) = adapter.GetDesc() else { return 0 };
            ((desc.AdapterLuid.HighPart as u64) << 32) | desc.AdapterLuid.LowPart as u64
        }
    }
}

/// Keeps published frames on the recording's frame grid.
///
/// A game may present at several hundred frames a second into a 60 fps
/// recording; without this every one of them would cost a full-resolution GPU
/// copy for nothing. Skipping here rather than on openclip's side is what makes
/// that saving real — the copy is the expensive part, not the readback.
struct Decimator {
    next_due: i64,
}

impl Decimator {
    fn new() -> Self {
        Self { next_due: 0 }
    }

    fn accept(&mut self, now: i64, freq: i64, fps: u32) -> bool {
        if freq <= 0 {
            return true;
        }
        // The same 0.85 slack `capture::min_update_interval` uses: asking for
        // exactly `1/fps` means a source running at precisely `fps` loses every
        // other frame to rounding.
        let interval = (freq as f64 * 0.85 / fps as f64) as i64;
        if self.next_due == 0 || now >= self.next_due {
            // Re-anchor rather than accumulating, so a stalled game does not
            // come back owing a burst of frames.
            self.next_due = now + interval;
            return true;
        }
        false
    }
}

/// Reports a format the pipeline cannot take, once.
pub fn report_unsupported_format(control: &Control, format: DXGI_FORMAT) {
    control.error_code.store(HookError::FormatUnsupported as u32, Ordering::Relaxed);
    // SAFETY: `error_text` is a plain byte array in the shared mapping.
    unsafe {
        let field = (&raw const control.error_text) as *mut [u8; 160];
        abi::write_cstr(&mut *field, &format!("back buffer format {} is not supported yet", format.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimator_holds_the_frame_rate() {
        const FREQ: i64 = 6_000_000;
        let mut d = Decimator::new();
        // A game presenting at 600 fps into a 60 fps recording.
        let step = FREQ / 600;
        let mut now = FREQ;
        let mut taken = 0;
        for _ in 0..600 {
            if d.accept(now, FREQ, 60) {
                taken += 1;
            }
            now += step;
        }
        // 0.85 slack means slightly more than 60 get through, never fewer.
        assert!((60..=72).contains(&taken), "expected about 60 frames, took {taken}");
    }

    #[test]
    fn decimator_takes_the_first_frame_and_survives_a_broken_clock() {
        let mut d = Decimator::new();
        assert!(d.accept(1_000, 6_000_000, 60), "the first frame must always be taken");
        assert!(d.accept(1, 0, 60), "a zero frequency must not divide by zero");
    }

    #[test]
    fn decimator_reanchors_after_a_stall() {
        const FREQ: i64 = 6_000_000;
        let mut d = Decimator::new();
        assert!(d.accept(FREQ, FREQ, 60));
        // A ten-second freeze must not then let ten seconds of frames through.
        let after = FREQ + FREQ * 10;
        assert!(d.accept(after, FREQ, 60));
        assert!(!d.accept(after + 1, FREQ, 60), "the grid should have re-anchored");
    }
}
