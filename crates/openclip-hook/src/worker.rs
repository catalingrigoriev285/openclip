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

        // The scan is the loop body, not a step before it: an OpenGL game loads
        // the module that owns the `SwapBuffers` import seconds after it starts
        // (for a Java game, long after the JVM is up), so a single pass at
        // attach time would miss it. Installing is idempotent.
        let mut backends = Backends::default();
        while !crate::shutting_down() && !shared.should_stop() {
            backends.scan(control);
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

/// Which graphics APIs have been hooked, and how long we have been looking.
///
/// A game may not have created its device yet — launchers and splash screens
/// routinely load Direct3D late, and an OpenGL game loads the module holding the
/// `SwapBuffers` import later still — so this keeps looking rather than giving
/// up on the first pass.
#[derive(Default)]
struct Backends {
    dxgi: bool,
    /// Never latched: unlike the single DXGI vtable, import slots live in every
    /// module, so a module loaded later carries an unpatched one. Re-scanning is
    /// cheap and [`crate::iat`] skips slots it has already done.
    opengl: bool,
    /// Set once DXGI has refused for a reason retrying cannot change, so the
    /// failure is not re-reported four times a second.
    dxgi_refused: bool,
    passes: u32,
}

impl Backends {
    fn scan(&mut self, control: &openclip_overlay::abi::Control) {
        // Being here at all is proof the hook is alive, which matters when no
        // backend has attached yet: without this the heartbeat only moves once
        // a frame is presented, and openclip would give up on a game whose API
        // it cannot hook rather than saying so.
        control.heartbeat_qpc.store(ipc::qpc() as u64, Ordering::Relaxed);

        // `GetModuleHandleW` never loads anything: it only reports what the game
        // has already brought in itself.
        if !self.dxgi && !self.dxgi_refused && (module_loaded("d3d11.dll") || module_loaded("dxgi.dll")) {
            if crate::dxgi::install() {
                self.dxgi = true;
            } else {
                // An unexpected vtable or a device we cannot use: retrying will
                // not change either. The hook stays attached and inert so
                // openclip can still see it and its error.
                self.dxgi_refused = true;
                hlog!("dxgi: not hooked");
            }
        }
        if module_loaded("opengl32.dll") && crate::opengl::install() && !self.opengl {
            self.opengl = true;
        }

        self.passes = self.passes.saturating_add(1);
        if self.passes == 20 && !self.dxgi && !self.opengl {
            hlog!("still waiting for a graphics API after 5 s");
            if module_loaded("opengl32.dll") {
                // The single most useful line in the log when an OpenGL game
                // does not light up: it says whether any module imports the
                // function at all, which is the whole premise of the IAT patch.
                crate::iat::log_swapbuffers_importers();
            }
        }
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
