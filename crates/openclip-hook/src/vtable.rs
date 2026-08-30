//! Redirecting one slot of a COM vtable.
//!
//! This is how the graphics hooks attach, in preference to an inline detour, and
//! the choice is deliberate:
//!
//! - It allocates **no executable memory** and builds no trampoline, so nothing
//!   in the process ends up with an RWX page that looks like a code cave.
//! - It does not modify a byte of `dxgi.dll`'s `.text`, so code-integrity checks
//!   over the signed module still pass.
//! - Putting the original pointer back is a single aligned store, which makes
//!   detaching safe. An inline detour cannot be removed while another thread is
//!   inside its trampoline, and openclip needs to be able to let go of a game.
//!
//! Every DXGI swapchain in a process shares one vtable, so a single patch
//! catches all of them — including ones created later, which is what makes
//! alt-enter and swapchain recreation a non-event.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::Threading::GetCurrentProcess;

/// The vtable pointer of a COM object.
///
/// # Safety
/// `iface` must be a live COM interface pointer.
pub unsafe fn vtable_of(iface: *mut c_void) -> *mut *mut c_void {
    unsafe { *(iface as *mut *mut *mut c_void) }
}

/// Reads the function pointer in `index` without changing anything.
///
/// # Safety
/// `vtable` must point at a vtable with more than `index` entries.
pub unsafe fn slot(vtable: *mut *mut c_void, index: usize) -> *mut c_void {
    unsafe { *vtable.add(index) }
}

/// Points slot `index` at `new`, returning what was there before.
///
/// # Safety
/// `vtable` must point at a vtable with more than `index` entries, and `new`
/// must have exactly the calling convention and signature of the method being
/// replaced.
pub unsafe fn swap(vtable: *mut *mut c_void, index: usize, new: *mut c_void) -> windows::core::Result<*mut c_void> {
    let entry = unsafe { vtable.add(index) };
    let mut previous = PAGE_PROTECTION_FLAGS(0);
    unsafe { VirtualProtect(entry as *const c_void, size_of::<*mut c_void>(), PAGE_READWRITE, &mut previous)? };
    let old = unsafe { *entry };
    unsafe { *entry = new };
    // Put the original protection back straight away; leaving a writable page in
    // a game's address space is exactly the sort of thing not to leave lying
    // around. A failure here cannot be acted on, but the patch itself is done.
    let mut ignored = PAGE_PROTECTION_FLAGS(0);
    let _ = unsafe { VirtualProtect(entry as *const c_void, size_of::<*mut c_void>(), previous, &mut ignored) };
    let _ = unsafe { FlushInstructionCache(GetCurrentProcess(), None, 0) };
    Ok(old)
}

/// The file name of the module `addr` belongs to, lowercased.
///
/// Used to check that a vtable slot points where it should *before* patching it.
/// If some other overlay has already hooked the swapchain, or a vtable index
/// turns out to be wrong on a future Windows build, the address will not be in
/// `dxgi.dll` — and refusing to patch is far better than redirecting an unknown
/// function inside someone's game.
pub fn module_of(addr: *const c_void) -> Option<String> {
    let mut module = HMODULE::default();
    // SAFETY: `UNCHANGED_REFCOUNT` means no reference is taken, so nothing leaks.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = [0u16; 260];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len]);
    Some(path.rsplit('\\').next().unwrap_or(&path).to_ascii_lowercase())
}

/// Whether `addr` lives in `expected` (a lowercase file name like `"dxgi.dll"`).
pub fn is_in_module(addr: *const c_void, expected: &str) -> bool {
    module_of(addr).is_some_and(|m| m == expected)
}
