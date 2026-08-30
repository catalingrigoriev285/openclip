//! openclip's side of the shared control block.
//!
//! Created **before** injecting, so the hook finds it the moment it starts, and
//! kept alive for as long as openclip is interested in that game. The hook polls
//! for it, which means disarming and re-arming a game later needs no second
//! injection: the DLL is still mapped and simply notices the block reappear.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use openclip_overlay::abi::{self, Control, GfxApi, HookError, OverlaySettings, HOOK_ABI_VERSION, HOOK_MAGIC};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// A control block openclip owns, for one target process.
pub struct HookSession {
    pid: u32,
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    control: *mut Control,
    /// Signalled by the hook when a frame slot has been filled.
    ready: HANDLE,
    /// Signalled by us to ask the hook to detach.
    stop: HANDLE,
}

// The mapping outlives every borrow, and all mutable fields are atomics.
unsafe impl Send for HookSession {}
unsafe impl Sync for HookSession {}

impl HookSession {
    /// Creates the block and both events for `pid`.
    pub fn create(pid: u32, hwnd: isize) -> Result<Self> {
        // SAFETY: plain Win32; every handle is checked and closed on drop.
        unsafe {
            let mapping = CreateFileMappingW(
                windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size_of::<Control>() as u32,
                &HSTRING::from(abi::control_name(pid)),
            )
            .context("creating the hook control block")?;

            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<Control>());
            if view.Value.is_null() {
                let _ = CloseHandle(mapping);
                return Err(anyhow!("mapping the hook control block failed"));
            }
            let control = view.Value as *mut Control;
            // A fresh mapping is zero-filled, so only the non-zero header needs
            // writing. Ordering matters: `magic` last would still be fine here
            // because the hook is not running yet, but writing it first would
            // let a *previously injected* hook read a half-filled header.
            let c = &mut *control;
            c.host_pid = std::process::id();
            c.target_pid = pid;
            c.target_hwnd = hwnd as u64;
            c.qpc_freq = qpc_frequency();
            c.struct_size = size_of::<Control>() as u32;
            c.abi_version = HOOK_ABI_VERSION;
            c.magic = HOOK_MAGIC;

            let ready = CreateEventW(None, false, false, &HSTRING::from(abi::ready_event_name(pid)))
                .context("creating the hook frame event")?;
            let stop = CreateEventW(None, true, false, &HSTRING::from(abi::stop_event_name(pid)))
                .context("creating the hook stop event")?;

            Ok(Self { pid, mapping, view, control, ready, stop })
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn control(&self) -> &Control {
        // SAFETY: mapped for the lifetime of `self`.
        unsafe { &*self.control }
    }

    /// Whether the hook has attached and reported itself.
    pub fn is_hooked(&self) -> bool {
        self.control().hook_version.load(Ordering::Relaxed) != 0
    }

    /// The hook's version as `major.minor.patch`, once it has reported.
    pub fn hook_version(&self) -> Option<(u16, u8, u8)> {
        let v = self.control().hook_version.load(Ordering::Relaxed);
        (v != 0).then(|| ((v >> 16) as u16, (v >> 8) as u8, v as u8))
    }

    /// Whether the hook has presented recently. A game that hung, exited or was
    /// never really hooked stops moving this.
    pub fn is_alive(&self, within: Duration) -> bool {
        let beat = self.control().heartbeat_qpc.load(Ordering::Relaxed);
        if beat == 0 {
            return false;
        }
        let freq = self.control().qpc_freq.max(1) as f64;
        let age = (qpc_now() as u64).saturating_sub(beat) as f64 / freq;
        age <= within.as_secs_f64()
    }

    pub fn api(&self) -> GfxApi {
        self.control().api()
    }

    pub fn error(&self) -> HookError {
        self.control().error()
    }

    /// The game's own present rate, which is what the counter shows.
    pub fn present_fps(&self) -> f32 {
        self.control().present_fps()
    }

    /// Draw the counter (green).
    pub fn arm(&self, on: bool) {
        self.control().armed.store(on as u32, Ordering::Relaxed);
    }

    /// Publish frames, and turn the counter red.
    pub fn set_capturing(&self, on: bool) {
        self.control().capturing.store(on as u32, Ordering::Relaxed);
    }

    pub fn set_capture_fps(&self, fps: u32) {
        self.control().capture_fps.store(fps.max(1), Ordering::Relaxed);
    }

    pub fn set_overlay(&self, settings: OverlaySettings) {
        self.control().set_overlay_settings(settings);
    }

    /// Blocks until the hook reports itself, or `timeout` passes.
    pub fn wait_for_hook(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.is_hooked() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// Waits for the hook to publish a frame. `false` on timeout.
    pub fn wait_for_frame(&self, timeout: Duration) -> bool {
        // SAFETY: `ready` is a live auto-reset event owned by this struct.
        unsafe { WaitForSingleObject(self.ready, timeout.as_millis() as u32) == WAIT_OBJECT_0 }
    }

    /// Asks the hook to detach. It stays mapped in the game either way — a DLL
    /// whose code is on a live present path cannot be safely unloaded — but it
    /// puts the vtable back and goes inert.
    pub fn request_stop(&self) {
        self.control().stop.store(1, Ordering::Relaxed);
        // SAFETY: `stop` is a live manual-reset event owned by this struct.
        let _ = unsafe { SetEvent(self.stop) };
    }
}

impl Drop for HookSession {
    fn drop(&mut self) {
        self.request_stop();
        // SAFETY: every handle was created here and is closed exactly once.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.ready);
            let _ = CloseHandle(self.stop);
        }
    }
}

pub fn qpc_frequency() -> i64 {
    let mut f = 0;
    // SAFETY: writes one i64; cannot fail on any supported Windows version.
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    f.max(1)
}

pub fn qpc_now() -> i64 {
    let mut v = 0;
    // SAFETY: as above.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}
