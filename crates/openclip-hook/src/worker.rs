//! The hook's own thread: everything `DllMain` is not allowed to do.
//!
//! It pins the DLL, waits for openclip's control block to appear, installs the
//! graphics hooks, and then watches for openclip letting go. It never blocks a
//! game's render thread — the hooks themselves only read atomics out of the
//! control block.
//!
//! The loop matters: after openclip detaches (or dies), the worker drops the
//! block and goes back to waiting. The DLL is mapped for the life of the game,
//! so a second recording session later needs no second injection.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};

use crate::ipc::{self, Shared};
use crate::logging::hlog;

/// The control block, while one is attached. The graphics hooks reach it through
/// here rather than being handed it, because they run on the game's threads.
static SHARED: RwLock<Option<Arc<Shared>>> = RwLock::new(None);

/// The attached control block, if any.
///
/// Cheap enough for a present hook: an uncontended read lock and an `Arc` clone.
/// The write side is only taken when openclip attaches or lets go.
pub fn shared() -> Option<Arc<Shared>> {
    SHARED.read().ok()?.clone()
}

pub fn run() {
    // Pin ourselves. The alternative — letting the DLL be unloaded — races with
    // whatever game thread is inside a hooked function at that moment, and
    // there is no way to make that safe. It stays mapped until the game exits.
    pin_self();

    if !ipc::claim_instance() {
        hlog!("another openclip hook is already live in this process; standing down");
        return;
    }

    hlog!("openclip hook {:#08x} starting in pid {}", ipc::hook_version(), std::process::id());

    while !crate::shutting_down() {
        let Some(shared) = wait_for_control_block() else { return };
        let control = shared.control();
        control.hook_version.store(ipc::hook_version(), Ordering::Relaxed);
        control.heartbeat_qpc.store(ipc::qpc() as u64, Ordering::Relaxed);
        *SHARED.write().expect("not poisoned") = Some(shared.clone());
        crate::clear_detached();

        install_backends(&shared);
        while !crate::shutting_down() && !shared.should_stop() {
            std::thread::sleep(Duration::from_millis(250));
        }

        // Stop drawing before letting go of the block, so no present can read a
        // control block that is about to be unmapped.
        crate::set_detached();
        *SHARED.write().expect("not poisoned") = None;
        drop(shared);
        hlog!("detached; waiting to be armed again");
    }
}

/// Polls until openclip's control block exists for this process.
///
/// It normally exists already — openclip creates it before injecting — but
/// polling is what lets openclip arm this game again later without injecting a
/// second time.
fn wait_for_control_block() -> Option<Arc<Shared>> {
    loop {
        if crate::shutting_down() {
            return None;
        }
        if let Some(s) = Shared::open() {
            return Some(Arc::new(s));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Waits for a graphics API to turn up and hooks it.
///
/// A game may not have created its device yet — launchers and splash screens
/// routinely load Direct3D late — so this keeps looking rather than giving up on
/// the first pass. Installing is idempotent, so a second arming is a no-op.
fn install_backends(shared: &Shared) {
    let mut waited = 0u32;
    while !crate::shutting_down() && !shared.should_stop() {
        // `GetModuleHandleW` never loads anything: it only reports what the game
        // has already brought in itself.
        if module_loaded("d3d11.dll") || module_loaded("dxgi.dll") {
            if crate::d3d11::install() {
                return;
            }
            // Installing failed for a reason retrying will not change (an
            // unexpected vtable, a device we cannot use). The hook stays
            // attached and inert so openclip can still see it and its error.
            hlog!("no graphics backend could be hooked");
            return;
        }
        if waited == 20 {
            hlog!("still waiting for a graphics API after 5 s");
        }
        waited = waited.saturating_add(1);
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn module_loaded(name: &str) -> bool {
    // SAFETY: `GetModuleHandleW` only queries; it never loads the module.
    unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(&windows::core::HSTRING::from(name)) }.is_ok()
}

/// Adds a reference to this module that is never released.
fn pin_self() {
    let mut module = Default::default();
    // SAFETY: takes the address of a function in this module, which is always
    // mapped while this code runs.
    unsafe {
        let _ = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(pin_self as *const u16),
            &mut module,
        );
    }
}
