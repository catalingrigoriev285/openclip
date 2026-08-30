//! Finding and mapping openclip's control block from inside the game.
//!
//! openclip creates the mapping *before* injecting, so it always exists by the
//! time the worker looks — but the worker keeps looking anyway. That is what
//! lets openclip disarm and re-arm a game it has already hooked without
//! injecting a second time: the DLL stays mapped, notices the mapping reappear,
//! and picks up where it left off.

use std::sync::atomic::Ordering;

use openclip_overlay::abi::{self, Control, HookError, HOOK_ABI_VERSION};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, OpenProcess, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_SYNCHRONIZE,
};

use crate::logging::hlog;

/// A mapped [`Control`], unmapped on drop.
pub struct Shared {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    control: *mut Control,
    /// Signalled after a frame slot is filled.
    ready: HANDLE,
    /// openclip signals this to ask us to unhook.
    stop: HANDLE,
    /// openclip's own process, watched so a crash cannot leave the counter on.
    host: Option<HANDLE>,
}

// The pointer is into a shared mapping that outlives the struct's use, and every
// field of `Control` is either write-once-before-injection or an atomic.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Opens openclip's control block for this process, or `None` if there is
    /// none (openclip is not armed for us) or it speaks a different ABI.
    pub fn open() -> Option<Self> {
        let pid = std::process::id();
        let name = HSTRING::from(abi::control_name(pid));
        // SAFETY: plain Win32 calls; every handle is checked and closed on drop.
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &name) }.ok()?;
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<Control>()) };
        if view.Value.is_null() {
            let _ = unsafe { CloseHandle(mapping) };
            return None;
        }
        let control = view.Value as *mut Control;

        // Check the contract before touching anything else in the struct: if the
        // layouts disagree, every other field is at the wrong offset.
        let ok = unsafe { &*control }.is_compatible();
        if !ok {
            let c = unsafe { &*control };
            hlog!(
                "abi mismatch: mapping says magic={:#x} version={} size={}, this hook wants magic={:#x} version={} size={}",
                c.magic,
                c.abi_version,
                c.struct_size,
                abi::HOOK_MAGIC,
                HOOK_ABI_VERSION,
                size_of::<Control>()
            );
            // Report it anyway when the header at least looks like ours, so
            // openclip can say "the hook component is out of date" instead of
            // silently never attaching.
            if c.magic == abi::HOOK_MAGIC {
                c.error_code.store(HookError::AbiMismatch as u32, Ordering::Relaxed);
                c.hook_version.store(hook_version(), Ordering::Relaxed);
            }
            let _ = unsafe { UnmapViewOfFile(view) };
            let _ = unsafe { CloseHandle(mapping) };
            return None;
        }

        let ready = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(abi::ready_event_name(pid))) }
            .unwrap_or_default();
        let stop = unsafe {
            OpenEventW(
                windows::Win32::System::Threading::SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000), // SYNCHRONIZE
                false,
                &HSTRING::from(abi::stop_event_name(pid)),
            )
        }
        .unwrap_or_default();

        let host_pid = unsafe { &*control }.host_pid;
        let host = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, host_pid) }.ok();
        if host.is_none() {
            hlog!("cannot watch openclip (pid {host_pid}); a crash there will not be noticed");
        }

        hlog!("attached to openclip control block for pid {pid}");
        Some(Self { mapping, view, control, ready, stop, host })
    }

    pub fn control(&self) -> &Control {
        // SAFETY: mapped for the lifetime of `self` and checked compatible above.
        unsafe { &*self.control }
    }

    /// Wakes openclip's capture thread after a slot has been filled.
    #[allow(dead_code)] // used by the graphics backends
    pub fn signal_ready(&self) {
        if !self.ready.is_invalid() {
            let _ = unsafe { SetEvent(self.ready) };
        }
    }

    #[allow(dead_code)] // used by the graphics backends
    pub fn stop_handle(&self) -> HANDLE {
        self.stop
    }

    /// True once openclip has asked us to detach, or has died.
    ///
    /// The liveness check is not belt-and-braces: the shared section stays alive
    /// while *we* hold a view of it, so a crashed or force-killed openclip
    /// leaves `armed` set forever and the counter stuck on someone's game with
    /// nothing left to turn it off. Watching the host process is the only way
    /// to notice.
    pub fn should_stop(&self) -> bool {
        if self.control().stop.load(Ordering::Relaxed) != 0 {
            return true;
        }
        if !self.host_alive() {
            hlog!("openclip (pid {}) is gone; detaching", self.control().host_pid);
            return true;
        }
        false
    }

    fn host_alive(&self) -> bool {
        let Some(host) = self.host else { return true };
        // SAFETY: a handle we opened and own; a zero timeout only polls.
        unsafe { WaitForSingleObject(host, 0) != WAIT_OBJECT_0 }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: each handle was created here and is dropped exactly once.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
            if !self.ready.is_invalid() {
                let _ = CloseHandle(self.ready);
            }
            if !self.stop.is_invalid() {
                let _ = CloseHandle(self.stop);
            }
            if let Some(host) = self.host {
                let _ = CloseHandle(host);
            }
        }
    }
}

/// Claims `Local\openclip.hook.<pid>.instance` so a second injection into the
/// same game is a no-op. The handle is intentionally leaked: it must live as
/// long as the DLL is mapped, which is until the game exits.
pub fn claim_instance() -> bool {
    let name = HSTRING::from(abi::instance_mutex_name(std::process::id()));
    // SAFETY: creating a named event; the handle is deliberately never closed.
    let existing = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) };
    if let Ok(h) = existing {
        let _ = unsafe { CloseHandle(h) };
        return false;
    }
    unsafe { CreateEventW(None, true, false, &name) }.is_ok()
}

/// This build's version, as `major << 16 | minor << 8 | patch`.
pub fn hook_version() -> u32 {
    const fn part(s: &str) -> u32 {
        // `parse` is not const; these come from Cargo and are plain decimals.
        let b = s.as_bytes();
        let mut v = 0u32;
        let mut i = 0;
        while i < b.len() {
            v = v * 10 + (b[i] - b'0') as u32;
            i += 1;
        }
        v
    }
    (part(env!("CARGO_PKG_VERSION_MAJOR")) << 16)
        | (part(env!("CARGO_PKG_VERSION_MINOR")) << 8)
        | part(env!("CARGO_PKG_VERSION_PATCH"))
}

/// `QueryPerformanceCounter`, the clock both sides timestamp frames with.
pub fn qpc() -> i64 {
    let mut v = 0;
    // SAFETY: writes one i64; cannot fail on any supported Windows version.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}
