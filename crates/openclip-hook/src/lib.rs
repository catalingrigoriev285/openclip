//! `openclip_hook64.dll` — the code openclip loads into a game so it can grab
//! frames at the source and draw the frame-rate counter into the game's own
//! picture.
//!
//! Loaded with `SetWindowsHookEx`, the documented first-party mechanism OBS and
//! Discord use. There is deliberately **no** stealth here: no manual mapping, no
//! reflective loading, no PEB unlinking, no thread hijacking, no direct
//! syscalls. The DLL is plainly named, carries a version resource, refuses to
//! attach to anything running an anti-cheat, and announces itself on screen with
//! a visible badge. Anything else would make a recording tool indistinguishable
//! from a cheat loader.
//!
//! ## What may happen in `DllMain`
//!
//! Almost nothing. `DllMain` runs under the loader lock, and a game that
//! deadlocks on load is a far worse bug than a missing overlay. Specifically, do
//! **not** add any of the following to [`DllMain`]:
//!
//! - `LoadLibrary` / `GetModuleHandle` on a module that may not be loaded yet,
//!   or any COM call (`CoInitialize`, creating a D3D device)
//! - file, registry or network access — including opening the log
//! - any wait (`WaitForSingleObject`, a mutex, a channel)
//! - `std::thread::spawn`, which initialises Rust's std TLS under the lock; the
//!   worker is started with a raw `CreateThread` instead
//! - `println!` / `log!` — nothing has a logger yet
//!
//! Everything real happens on the worker thread in [`worker`].

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::BOOL;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::Win32::System::Threading::CreateThread;
use windows::Win32::UI::WindowsAndMessaging::CallNextHookEx;

mod d3d11;
mod ipc;
mod logging;
mod vtable;
mod worker;

/// This module's own `HINSTANCE`, needed to pin the DLL and to resolve resources.
static SELF_MODULE: AtomicIsize = AtomicIsize::new(0);
/// Set on `DLL_PROCESS_DETACH`. Every hook checks it and chains straight
/// through: the process is going away and touching the GPU is pointless.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Used by the graphics backends to resolve their own module.
#[allow(dead_code)]
pub(crate) fn self_module() -> HINSTANCE {
    HINSTANCE(SELF_MODULE.load(Ordering::Relaxed) as *mut c_void)
}

pub(crate) fn shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::Relaxed)
}

/// Entry point. Read the module docs before adding anything here.
///
/// # Safety
/// Called by the Windows loader with the loader lock held.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            SELF_MODULE.store(hinst.0 as isize, Ordering::Relaxed);
            // We never care about thread attach/detach, and not being called for
            // them is one less thing running under the loader lock in a game
            // that spawns worker threads constantly.
            unsafe { DisableThreadLibraryCalls(hinst.into()).ok() };
            // A raw thread, not `std::thread::spawn`: see the module docs.
            let handle = unsafe { CreateThread(None, 0, Some(worker_entry), None, Default::default(), None) };
            if let Ok(h) = handle {
                let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
            }
        }
        DLL_PROCESS_DETACH => {
            // Set a flag and return. Do not join, unhook or free anything: other
            // threads may be inside a hooked function right now.
            SHUTTING_DOWN.store(true, Ordering::Relaxed);
        }
        _ => {}
    }
    TRUE
}

unsafe extern "system" fn worker_entry(_: *mut c_void) -> u32 {
    worker::run();
    0
}

/// The `WH_GETMESSAGE` procedure openclip installs to get this DLL loaded.
///
/// It does nothing but chain: the hook is purely a loading mechanism, and once
/// the DLL is mapped the worker thread pins it and takes over. openclip removes
/// the message hook immediately afterwards.
///
/// # Safety
/// Called by the Windows message dispatcher.
#[unsafe(no_mangle)]
pub extern "system" fn openclip_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
