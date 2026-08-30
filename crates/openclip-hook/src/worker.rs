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

    hlog!("waiting for a graphics API to appear");
    // TODO(phase 2): detect d3d11/d3d12/opengl32/vulkan-1 and install the
    // present hooks. Until then the hook attaches, reports its version and
    // heartbeat, and openclip can see it is alive.
    idle_until_stopped();
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
