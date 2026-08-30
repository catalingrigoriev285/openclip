//! The safety net every hook body runs inside.
//!
//! Shared by all backends on purpose: the fault count is a property of *this
//! hook in this game*, not of one graphics API. A hook that has already gone
//! wrong three times in the DXGI path has no business still drawing from the
//! OpenGL one.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};

use openclip_overlay::abi::HookError;

use crate::logging::hlog;
use crate::worker;

/// Panics caught inside the hooks. At [`MAX_FAULTS`] we stop drawing for good:
/// a broken overlay is a nuisance, a game that crashes every frame is not.
static FAULTS: AtomicU32 = AtomicU32::new(0);
const MAX_FAULTS: u32 = 3;

/// Runs overlay work, swallowing panics.
///
/// Rust has no stable SEH, so this cannot catch a hardware fault — but it does
/// catch our own bugs, and killing someone's game over one would be far worse
/// than losing the counter. After [`MAX_FAULTS`] the hook stops trying.
pub fn guard(what: &str, f: impl FnOnce()) {
    if !armed() {
        return;
    }
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        let n = FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
        hlog!("panic inside the {what} hook ({n}/{MAX_FAULTS})");
        if n >= MAX_FAULTS {
            hlog!("disarming: too many faults");
            report(HookError::SelfDisarmed, "the overlay faulted repeatedly and stopped itself");
        }
    }
}

/// Whether the hooks should still do anything at all.
pub fn armed() -> bool {
    !crate::shutting_down() && !crate::detached() && FAULTS.load(Ordering::Relaxed) < MAX_FAULTS
}

/// Stops all overlay work for the life of the process. For failures that
/// retrying cannot fix — a device we cannot use, an unexpected vtable.
pub fn disarm() {
    FAULTS.store(MAX_FAULTS, Ordering::Relaxed);
}

/// Tells openclip what went wrong, so the status card can say it.
pub fn report(code: HookError, detail: &str) {
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
