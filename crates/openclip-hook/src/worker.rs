//! The hook's own thread: everything `DllMain` is not allowed to do.
//!
//! It pins the DLL, waits for openclip's control block to appear, installs the
//! graphics hooks, and then idles until asked to stop. It never blocks a game's
//! render thread — the hooks themselves only read atomics out of the control
//! block.

use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};

use crate::ipc::{self, Shared};
use crate::logging::hlog;

/// The control block, once found. The graphics hooks reach it through here
/// rather than being handed it, because they run on the game's threads.
static SHARED: OnceLock<Shared> = OnceLock::new();

pub fn shared() -> Option<&'static Shared> {
    SHARED.get()
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

    // openclip creates the mapping before injecting, so this normally succeeds
    // first time. Polling anyway is what lets it disarm and re-arm us later
    // without a second injection.
    let shared = loop {
        if crate::shutting_down() {
            return;
        }
        if let Some(s) = Shared::open() {
            break s;
        }
        std::thread::sleep(Duration::from_millis(250));
    };

    let control = shared.control();
    control.hook_version.store(ipc::hook_version(), Ordering::Relaxed);
    control.heartbeat_qpc.store(ipc::qpc() as u64, Ordering::Relaxed);
    let _ = SHARED.set(shared);

    install_backends();
    idle_until_stopped();
}

/// Waits for a graphics API to turn up and hooks it.
///
/// A game may not have created its device yet when we attach — launchers and
/// splash screens routinely load D3D late — so this keeps looking rather than
/// giving up on the first pass.
fn install_backends() {
    let Some(shared) = shared() else { return };
    let mut waited = 0u32;
    while !crate::shutting_down() && !shared.should_stop() {
        // `GetModuleHandleW` never loads anything: it only reports what the game
        // has already brought in itself.
        if module_loaded("d3d11.dll") || module_loaded("dxgi.dll") {
            if crate::d3d11::install() {
                return;
            }
            // Installing failed for a reason that will not change by retrying
            // (an unexpected vtable, a device we cannot use). The hook stays
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

fn idle_until_stopped() {
    let Some(shared) = shared() else { return };
    while !crate::shutting_down() && !shared.should_stop() {
        std::thread::sleep(Duration::from_millis(250));
    }
    hlog!("stopping");
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
