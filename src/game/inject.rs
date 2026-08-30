//! Getting `openclip_hook64.dll` loaded into a game.
//!
//! `SetWindowsHookEx` only. It is the documented, first-party way to have the
//! loader map a DLL into another process, it needs no privileged handle to the
//! target, and it is what OBS and Discord use for the same job.
//!
//! `CreateRemoteThread` + `LoadLibraryW` is deliberately **not** implemented as
//! a fallback. It gains nothing where this works, and `VirtualAllocEx` +
//! `WriteProcessMemory` + a remote thread entering `LoadLibrary` is the single
//! most heavily flagged pattern in the whole anti-cheat threat model. A
//! recorder that reaches for it is indistinguishable from a cheat loader, and
//! being distinguishable is the entire point (see the game-capture notes in
//! README.md).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{s, HSTRING, PCSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HOOKPROC, WH_GETMESSAGE,
    WM_NULL,
};

type FarProc = unsafe extern "system" fn() -> isize;
type HookProcFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

/// The file name shipped beside `openclip.exe`.
pub const HOOK_DLL: &str = "openclip_hook64.dll";

/// Where the hook DLL lives, or `None` when this build cannot find it.
///
/// Next to the executable in a real install. In a dev build the exe sits in
/// `target/<profile>/` and cargo puts the DLL right beside it, so the same
/// lookup works without a special case.
pub fn hook_dll_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let beside = exe.parent()?.join(HOOK_DLL);
        if beside.is_file() {
            return Some(beside);
        }
        // Examples live one directory deeper (`target/debug/examples`), which is
        // the only case where the DLL is not a sibling.
        let up = exe.parent()?.parent()?.join(HOOK_DLL);
        up.is_file().then_some(up)
    })
    .clone()
}

/// Loads the hook DLL into **openclip itself**.
///
/// `SetWindowsHookExW` needs an `HINSTANCE` for a module loaded in the calling
/// process, so this is unavoidable. It also means openclip holds the DLL open
/// from the first injection onward, which is why the self-updater has to rename
/// it aside rather than overwrite it.
fn local_module() -> Result<HMODULE> {
    static MODULE: OnceLock<isize> = OnceLock::new();
    let raw = MODULE.get_or_init(|| {
        let Some(path) = hook_dll_path() else { return 0 };
        // SAFETY: loading our own signed DLL by absolute path.
        match unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())) } {
            Ok(h) => h.0 as isize,
            Err(e) => {
                log::warn!("game: cannot load {}: {e}", path.display());
                0
            }
        }
    });
    if *raw == 0 {
        bail!("the game-capture component ({HOOK_DLL}) is missing or could not be loaded");
    }
    Ok(HMODULE(*raw as *mut std::ffi::c_void))
}

/// Injects the hook into the process owning `hwnd`, waiting until `attached`
/// reports success or `timeout` passes.
///
/// The control block must already exist for that pid: the hook looks for it as
/// soon as it starts, and creating it first is what lets the DLL attach without
/// being told anything by us.
///
/// The wait is not optional. Windows only maps the DLL when the hooked thread
/// next pumps a message, so removing the message hook straight after installing
/// it — the obvious-looking thing to do — means the DLL is essentially never
/// loaded. The hook has to stay in place until the DLL is in.
pub fn inject(hwnd: isize, attached: impl Fn() -> bool, timeout: Duration) -> Result<()> {
    let module = local_module()?;
    // SAFETY: `GetProcAddress` on a module we just loaded; the symbol is
    // exported by our own DLL with exactly this signature.
    let proc = unsafe { GetProcAddress(module, PCSTR(s!("openclip_hook_proc").as_ptr())) }
        .ok_or_else(|| anyhow!("{HOOK_DLL} does not export openclip_hook_proc; it is the wrong version"))?;

    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    // SAFETY: `hwnd` is checked by the caller's probe; a dead window simply
    // yields thread 0 and is rejected below.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread == 0 {
        bail!("the window went away before it could be hooked");
    }

    // SAFETY: installing a message hook on one thread of the target. The
    // procedure lives in our DLL, which the loader maps into that process.
    // `GetProcAddress` hands back an untyped `FARPROC`; this is the one place
    // the exported procedure's real signature has to be asserted.
    let hook_proc: HOOKPROC = Some(unsafe { std::mem::transmute::<FarProc, HookProcFn>(proc) });
    let hook = unsafe { SetWindowsHookExW(WH_GETMESSAGE, hook_proc, Some(HINSTANCE(module.0)), thread) }
        .context("installing the message hook")?;

    // The loader maps the DLL when the hooked thread next pumps a message, so
    // nudge it rather than waiting on whatever the game does on its own — a
    // fullscreen title between frames can go a long time without pumping. Keep
    // nudging: one message can be consumed before the hook is fully in place.
    let deadline = Instant::now() + timeout;
    let mut attached_ok = false;
    while Instant::now() < deadline {
        let _ = unsafe { PostThreadMessageW(thread, WM_NULL, Default::default(), Default::default()) };
        if attached() {
            attached_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // The message hook is only a loading mechanism. Once the DLL is in it pins
    // itself and works from its own thread, so this comes back out — leaving it
    // installed would put our procedure on every message that thread handles.
    let _ = unsafe { UnhookWindowsHookEx(hook) };

    if !attached_ok {
        bail!("the hook did not attach within {:.0} s", timeout.as_secs_f32());
    }
    Ok(())
}

/// Whether a hook DLL is present at all, for the UI to disable Game mode
/// with an honest message instead of failing at the moment someone presses REC.
pub fn is_available() -> bool {
    hook_dll_path().is_some()
}

/// The directory the DLL is expected in, for error messages.
pub fn expected_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}
