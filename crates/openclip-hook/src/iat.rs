//! Redirecting an imported function by rewriting the caller's import table.
//!
//! OpenGL has no COM vtable to patch, so [`crate::vtable`]'s trick does not
//! reach it. The alternative most overlays use is an inline detour — rewriting
//! the first bytes of `gdi32!SwapBuffers` and jumping to a generated trampoline
//! — and this deliberately does not do that. An import address table is *data*:
//! patching it allocates no executable memory, modifies no byte of a signed
//! module's `.text`, and reverses with the same single aligned store the vtable
//! path uses. The posture in [`crate::vtable`]'s module docs is preserved
//! exactly.
//!
//! The cost is that it only catches calls made *through* an import slot. A
//! module that resolves `SwapBuffers` with `GetProcAddress` and calls it through
//! a stored pointer is invisible here. In practice the OpenGL games that matter
//! import it statically — Minecraft swaps through `glfw.dll`, whose import table
//! carries a plain `gdi32.dll → SwapBuffers` entry.
//!
//! Two details make this work in a real process:
//!
//! - **Match by address, not by name.** The name arrays are missing for ordinal
//!   imports and stripped for bound ones, and API-set redirection means the name
//!   in the table is not the name of the module that ends up serving the call.
//!   Comparing the slot's current *value* against `GetProcAddress` handles all
//!   three, and is one comparison.
//! - **Scan every module, repeatedly.** The slot that matters is rarely in the
//!   exe: for a Java game the JVM starts long before `glfw.dll` is even loaded.
//!   [`patch_all`] is idempotent and meant to be re-run from the worker's poll
//!   loop, so a module that arrives late is picked up on the next pass.
//!
//! Everything here runs on a game's own threads eventually, and a malformed PE
//! header would fault rather than panic (`catch_unwind` cannot catch an access
//! violation), so every offset is range-checked against the module's image size
//! before it is dereferenced and every loop is capped.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::{s, PCSTR};
use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleInformation, MODULEINFO};
use windows::Win32::System::SystemServices::{IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_IMPORT_DESCRIPTOR};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::logging::hlog;

/// Modules to look at in one pass. A process with more than this many is not
/// one we are going to hook anyway.
const MAX_MODULES: usize = 1024;
/// Caps on the PE walk, so a corrupt table cannot spin inside a render thread.
const MAX_DESCRIPTORS: usize = 4096;
const MAX_THUNKS: usize = 65536;

/// Slots already pointing at us, so a re-scan does not patch twice (and does not
/// record our own hook as the "original").
static PATCHED: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// One function to redirect.
pub struct Target {
    /// The module exporting it, e.g. `b"gdi32.dll\0"`.
    pub module: PCSTR,
    /// Its exported name, e.g. `b"SwapBuffers\0"`.
    pub name: PCSTR,
    /// What to point the slot at.
    pub replacement: *const c_void,
}

// SAFETY: the fields are a static string, a static string and a function
// pointer into this module. None of them is thread-affine.
unsafe impl Send for Target {}
unsafe impl Sync for Target {}

/// Points every import of `target.name` in every loaded module at
/// `target.replacement`, and returns the original address.
///
/// Idempotent: slots already patched are skipped, and calling it again after new
/// modules have loaded patches only the new ones. `Ok(None)` means the function
/// could not be resolved at all — the module exporting it is not loaded, which
/// for `opengl32.dll` in a game that never touches OpenGL is normal.
pub fn patch_all(target: &Target) -> Option<(*const c_void, usize)> {
    // SAFETY: both are static NUL-terminated strings; `GetModuleHandleA` only
    // queries and never loads.
    let original = unsafe {
        let module = GetModuleHandleA(target.module).ok()?;
        GetProcAddress(module, target.name)? as *const c_void
    };

    let mut patched = 0usize;
    for module in loaded_modules() {
        // Our own imports must be left alone: patching them would make the
        // chain-through call re-enter the hook, which is an infinite recursion
        // inside the game's render thread.
        if module.0 == crate::self_module().0 {
            continue;
        }
        patched += patch_module(module, original, target.replacement);
    }
    (patched > 0).then_some((original, patched))
}

/// Rewrites every import slot in `module` that currently points at `original`.
fn patch_module(module: HMODULE, original: *const c_void, replacement: *const c_void) -> usize {
    let Some(size) = image_size(module) else { return 0 };
    let base = module.0 as usize;
    // Everything below is bounds-checked against `[base, base + size)` before it
    // is read, because a bad RVA here would fault, not panic.
    let within = |addr: usize, len: usize| addr >= base && addr.checked_add(len).is_some_and(|e| e <= base + size);

    // SAFETY: `base` is a mapped module and every dereference is preceded by a
    // `within` check covering exactly the bytes read.
    unsafe {
        let dos = base as *const IMAGE_DOS_HEADER;
        if !within(base, size_of::<IMAGE_DOS_HEADER>()) || (*dos).e_magic != IMAGE_DOS_SIGNATURE {
            return 0;
        }
        let nt_addr = base + (*dos).e_lfanew as usize;
        if !within(nt_addr, size_of::<IMAGE_NT_HEADERS64>()) {
            return 0;
        }
        let nt = nt_addr as *const IMAGE_NT_HEADERS64;
        // `IMAGE_OPTIONAL_HEADER64` is `packed(4)`; reading a field through a
        // reference is a hard error, so the directory is copied out by value.
        let dir = std::ptr::addr_of!((*nt).OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT.0 as usize])
            .read_unaligned();
        if dir.VirtualAddress == 0 || dir.Size == 0 {
            return 0;
        }

        let mut count = 0usize;
        let mut desc = (base + dir.VirtualAddress as usize) as *const IMAGE_IMPORT_DESCRIPTOR;
        for _ in 0..MAX_DESCRIPTORS {
            if !within(desc as usize, size_of::<IMAGE_IMPORT_DESCRIPTOR>()) {
                break;
            }
            let first = std::ptr::addr_of!((*desc).FirstThunk).read_unaligned();
            if first == 0 {
                break; // the terminating all-zero descriptor
            }
            // The thunk array is walked as raw pointer-sized words rather than
            // `IMAGE_THUNK_DATA64`, which lives behind a feature this crate has
            // no other use for. Once bound, every entry *is* a function address.
            let mut thunk = (base + first as usize) as *mut usize;
            for _ in 0..MAX_THUNKS {
                if !within(thunk as usize, size_of::<usize>()) {
                    break;
                }
                let value = *thunk;
                if value == 0 {
                    break;
                }
                if value == original as usize {
                    count += usize::from(write_slot(thunk, replacement));
                }
                thunk = thunk.add(1);
            }
            desc = desc.add(1);
        }
        count
    }
}

/// Points one already-located slot at `replacement`. Returns whether it changed.
///
/// # Safety
/// `slot` must be a writable-after-`VirtualProtect` pointer-sized location
/// inside a mapped module's import table.
unsafe fn write_slot(slot: *mut usize, replacement: *const c_void) -> bool {
    let mut seen = PATCHED.lock().expect("not poisoned");
    let seen = seen.get_or_insert_with(HashSet::new);
    if !seen.insert(slot as usize) {
        return false; // already ours
    }

    // The import table lives in read-only `.rdata`. Same two-call dance as
    // `vtable::swap`: open it, store, put the original protection straight back
    // rather than leaving a writable page lying around in a game.
    let mut previous = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the caller guarantees `slot` points into a mapped image.
    unsafe {
        if VirtualProtect(slot as *const c_void, size_of::<usize>(), PAGE_READWRITE, &mut previous).is_err() {
            seen.remove(&(slot as usize));
            return false;
        }
        *slot = replacement as usize;
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot as *const c_void, size_of::<usize>(), previous, &mut ignored);
        let _ = FlushInstructionCache(GetCurrentProcess(), None, 0);
    }
    true
}

/// Every module currently mapped into this process.
fn loaded_modules() -> Vec<HMODULE> {
    let mut modules = vec![HMODULE::default(); MAX_MODULES];
    let mut needed = 0u32;
    // SAFETY: the buffer and the byte count agree; a failure leaves the list
    // empty, which simply means nothing is patched this pass.
    let ok = unsafe {
        EnumProcessModules(
            GetCurrentProcess(),
            modules.as_mut_ptr(),
            (modules.len() * size_of::<HMODULE>()) as u32,
            &mut needed,
        )
    };
    if ok.is_err() {
        return Vec::new();
    }
    let count = (needed as usize / size_of::<HMODULE>()).min(modules.len());
    modules.truncate(count);
    modules
}

/// The mapped size of a module, which every RVA is checked against.
fn image_size(module: HMODULE) -> Option<usize> {
    let mut info = MODULEINFO::default();
    // SAFETY: a module handle from `EnumProcessModules` in our own process.
    unsafe {
        GetModuleInformation(GetCurrentProcess(), module, &mut info, size_of::<MODULEINFO>() as u32).ok()?;
    }
    (info.SizeOfImage > 0).then_some(info.SizeOfImage as usize)
}

/// Logs which modules import `SwapBuffers`, to make a failure to attach
/// diagnosable from the hook log alone.
pub fn log_swapbuffers_importers() {
    let Ok(gdi) = (unsafe { GetModuleHandleA(s!("gdi32.dll")) }) else { return };
    // SAFETY: a live module handle and a static name.
    let Some(addr) = (unsafe { GetProcAddress(gdi, s!("SwapBuffers")) }) else { return };
    let addr = addr as usize;
    let mut names = Vec::new();
    for module in loaded_modules() {
        if imports_address(module, addr) {
            names.push(module_name(module));
        }
    }
    hlog!("opengl: modules importing SwapBuffers: {names:?}");
}

/// Whether `module`'s import table holds `addr`, without patching anything.
fn imports_address(module: HMODULE, addr: usize) -> bool {
    let Some(size) = image_size(module) else { return false };
    let base = module.0 as usize;
    let within = |a: usize, len: usize| a >= base && a.checked_add(len).is_some_and(|e| e <= base + size);
    // SAFETY: as in `patch_module`; every read is bounds-checked first.
    unsafe {
        let dos = base as *const IMAGE_DOS_HEADER;
        if !within(base, size_of::<IMAGE_DOS_HEADER>()) || (*dos).e_magic != IMAGE_DOS_SIGNATURE {
            return false;
        }
        let nt_addr = base + (*dos).e_lfanew as usize;
        if !within(nt_addr, size_of::<IMAGE_NT_HEADERS64>()) {
            return false;
        }
        let nt = nt_addr as *const IMAGE_NT_HEADERS64;
        let dir = std::ptr::addr_of!((*nt).OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT.0 as usize])
            .read_unaligned();
        if dir.VirtualAddress == 0 {
            return false;
        }
        let mut desc = (base + dir.VirtualAddress as usize) as *const IMAGE_IMPORT_DESCRIPTOR;
        for _ in 0..MAX_DESCRIPTORS {
            if !within(desc as usize, size_of::<IMAGE_IMPORT_DESCRIPTOR>()) {
                return false;
            }
            let first = std::ptr::addr_of!((*desc).FirstThunk).read_unaligned();
            if first == 0 {
                return false;
            }
            let mut thunk = (base + first as usize) as *const usize;
            for _ in 0..MAX_THUNKS {
                if !within(thunk as usize, size_of::<usize>()) {
                    break;
                }
                match *thunk {
                    0 => break,
                    v if v == addr => return true,
                    _ => {}
                }
                thunk = thunk.add(1);
            }
            desc = desc.add(1);
        }
        false
    }
}

fn module_name(module: HMODULE) -> String {
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    let mut buf = [0u16; MAX_PATH as usize];
    // SAFETY: a live module handle from the enumeration above.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let path = String::from_utf16_lossy(&buf[..len]);
    path.rsplit(['\\', '/']).next().unwrap_or(&path).to_ascii_lowercase()
}
