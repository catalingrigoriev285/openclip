//! Deciding whether a window belongs to a game we may hook.
//!
//! The interesting parts — which graphics API a module list implies, whether an
//! anti-cheat is present, whether an executable is one of the many non-games
//! that also load Direct3D — are pure functions over plain strings, so they are
//! tested without a process to look at.

use openclip_overlay::abi::GfxApi;

/// Why a window was not hooked. Every variant maps to one localized message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// openclip's own window.
    OwnProcess,
    /// Nothing graphical is loaded, or the window is too small to be a game.
    NotAGame,
    /// A known non-game that happens to render with Direct3D.
    Excluded,
    /// 32-bit. Only x64 games are supported; see the module docs in `game`.
    NotX64,
    /// Direct3D 9 only, which this hook does not implement.
    D3d9Only,
    /// The process is protected or running elevated and cannot be inspected.
    AccessDenied,
    /// An anti-cheat is loaded. **Never overridable.**
    AntiCheat(AntiCheat),
}

/// The anti-cheat systems worth naming in a refusal message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AntiCheat {
    EasyAntiCheat,
    BattlEye,
    Vanguard,
    GameGuard,
    PunkBuster,
    Denuvo,
    Other(String),
}

impl AntiCheat {
    /// The product name, shown to the user. Not translated: these are brands.
    pub fn label(&self) -> &str {
        match self {
            AntiCheat::EasyAntiCheat => "Easy Anti-Cheat",
            AntiCheat::BattlEye => "BattlEye",
            AntiCheat::Vanguard => "Riot Vanguard",
            AntiCheat::GameGuard => "nProtect GameGuard",
            AntiCheat::PunkBuster => "PunkBuster",
            AntiCheat::Denuvo => "Denuvo",
            AntiCheat::Other(name) => name,
        }
    }
}

/// A window we are willing to hook.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub pid: u32,
    pub hwnd: isize,
    /// Executable file name, e.g. `"game.exe"`.
    pub exe: String,
    /// Best guess from the loaded modules; the hook confirms it at runtime.
    pub api: GfxApi,
}

/// Applications that load Direct3D but are not games.
///
/// Browsers and Electron apps all render with D3D11 and would otherwise be
/// hooked the moment they came to the foreground, which is not what anyone
/// means by "record my game".
const EXCLUDED: &[&str] = &[
    // Shell and system UI
    "explorer.exe",
    "dwm.exe",
    "searchhost.exe",
    "searchapp.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "applicationframehost.exe",
    "textinputhost.exe",
    "sihost.exe",
    "lockapp.exe",
    "taskmgr.exe",
    "systemsettings.exe",
    // Browsers
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    // Electron and friends
    "code.exe",
    "discord.exe",
    "slack.exe",
    "spotify.exe",
    "teams.exe",
    "obs64.exe",
    // Us
    "openclip.exe",
];

/// Substrings that identify an anti-cheat module, and what to call it.
const ANTI_CHEAT: &[(&str, AntiCheat)] = &[
    ("easyanticheat", AntiCheat::EasyAntiCheat),
    ("beclient", AntiCheat::BattlEye),
    ("beservice", AntiCheat::BattlEye),
    ("battleye", AntiCheat::BattlEye),
    ("vgc", AntiCheat::Vanguard),
    ("vgk", AntiCheat::Vanguard),
    ("vanguard", AntiCheat::Vanguard),
    ("gameguard", AntiCheat::GameGuard),
    ("npggnt", AntiCheat::GameGuard),
    ("xhunter", AntiCheat::GameGuard),
    ("pnkbstr", AntiCheat::PunkBuster),
    ("denuvo", AntiCheat::Denuvo),
    ("mhyprot", AntiCheat::Other(String::new())),
    ("equ8", AntiCheat::Other(String::new())),
    ("treyarch_anticheat", AntiCheat::Other(String::new())),
];

/// Whether `exe` is a known non-game. Case-insensitive.
pub fn is_excluded(exe: &str) -> bool {
    let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe).to_ascii_lowercase();
    EXCLUDED.contains(&name.as_str())
}

/// What a process's loaded modules say about it.
///
/// Returns the graphics API to expect and any anti-cheat found. The anti-cheat
/// answer takes priority over everything else at the call site: openclip refuses
/// such a process outright rather than warning and proceeding.
pub fn classify_modules(modules: &[String]) -> (GfxApi, Option<AntiCheat>) {
    let lower: Vec<String> = modules
        .iter()
        .map(|m| m.rsplit(['\\', '/']).next().unwrap_or(m).to_ascii_lowercase())
        .collect();

    let anti_cheat = lower.iter().find_map(|m| {
        ANTI_CHEAT.iter().find(|(needle, _)| m.contains(needle)).map(|(_, kind)| match kind {
            // The catch-all entries carry no name of their own; use the module's.
            AntiCheat::Other(_) => AntiCheat::Other(m.trim_end_matches(".dll").to_string()),
            other => other.clone(),
        })
    });

    let has = |name: &str| lower.iter().any(|m| m == name);
    // Vulkan first: a Vulkan game often also has dxgi loaded for presentation
    // plumbing, and guessing D3D there would mislabel it. D3D12 before D3D11 for
    // the same reason — a D3D12 game routinely loads d3d11 for video playback.
    let api = if has("vulkan-1.dll") {
        GfxApi::Vulkan
    } else if has("d3d12.dll") {
        GfxApi::D3D12
    } else if has("d3d11.dll") || has("dxgi.dll") {
        GfxApi::D3D11
    } else if has("opengl32.dll") {
        GfxApi::OpenGl
    } else {
        GfxApi::Unknown
    };
    (api, anti_cheat)
}

/// Whether the module list is Direct3D 9 and nothing newer — a real case for
/// older titles, and one this hook deliberately does not cover.
pub fn is_d3d9_only(modules: &[String]) -> bool {
    let (api, _) = classify_modules(modules);
    if api != GfxApi::Unknown {
        return false;
    }
    modules.iter().any(|m| m.to_ascii_lowercase().contains("d3d9.dll"))
}

/// The smallest window that could plausibly be a game.
pub const MIN_GAME_SIZE: (i32, i32) = (640, 480);

/// Decides whether `hwnd` belongs to a game openclip may hook.
///
/// Everything here is a *read* — no handle with more rights than
/// `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ` is ever opened, and in
/// particular never `PROCESS_DUP_HANDLE` or `PROCESS_ALL_ACCESS`.
pub fn probe_window(hwnd: isize) -> Result<Candidate, Refusal> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, GWL_STYLE, GW_OWNER,
        WS_CHILD,
    };

    let handle = HWND(hwnd as *mut std::ffi::c_void);
    // SAFETY: window queries on a handle the caller just read from the OS; a
    // window that has since closed reports zero and is rejected below.
    unsafe {
        if !IsWindowVisible(handle).as_bool() {
            return Err(Refusal::NotAGame);
        }
        // Owned windows are dialogs and tool windows, never the game itself.
        if GetWindow(handle, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return Err(Refusal::NotAGame);
        }
        if GetWindowLongPtrW(handle, GWL_STYLE) & WS_CHILD.0 as isize != 0 {
            return Err(Refusal::NotAGame);
        }
        let mut rect = Default::default();
        if GetWindowRect(handle, &mut rect).is_err() {
            return Err(Refusal::NotAGame);
        }
        if rect.right - rect.left < MIN_GAME_SIZE.0 || rect.bottom - rect.top < MIN_GAME_SIZE.1 {
            return Err(Refusal::NotAGame);
        }

        let mut pid = 0;
        GetWindowThreadProcessId(handle, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return Err(Refusal::OwnProcess);
        }
        probe_process(pid, hwnd)
    }
}

/// The process half of [`probe_window`], split out so a pid can be probed
/// directly (the headless harness does).
pub fn probe_process(pid: u32, hwnd: isize) -> Result<Candidate, Refusal> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_UNKNOWN;
    use windows::Win32::System::Threading::{
        IsWow64Process2, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: the handle is closed on every path out of this block.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid) else {
            // Protected or running elevated. Not something to work around.
            return Err(Refusal::AccessDenied);
        };
        let finish = |result: Result<Candidate, Refusal>| {
            let _ = CloseHandle(process);
            result
        };

        // A 32-bit game reports a non-zero WOW64 machine. Only x64 is supported,
        // and saying so plainly beats failing later for no visible reason.
        let mut wow64 = IMAGE_FILE_MACHINE_UNKNOWN;
        let mut native = IMAGE_FILE_MACHINE_UNKNOWN;
        if IsWow64Process2(process, &mut wow64, Some(&mut native)).is_ok() && wow64 != IMAGE_FILE_MACHINE_UNKNOWN {
            return finish(Err(Refusal::NotX64));
        }

        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let exe = match QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        ) {
            Ok(()) => {
                let path = String::from_utf16_lossy(&buf[..len as usize]);
                path.rsplit('\\').next().unwrap_or(&path).to_string()
            }
            Err(_) => return finish(Err(Refusal::AccessDenied)),
        };
        if is_excluded(&exe) {
            return finish(Err(Refusal::Excluded));
        }

        let modules = module_names(process);
        let (api, anti_cheat) = classify_modules(&modules);
        // The anti-cheat answer wins over everything else, including "we could
        // not work out the renderer". There is no override for this.
        if let Some(ac) = anti_cheat {
            return finish(Err(Refusal::AntiCheat(ac)));
        }
        if api == GfxApi::Unknown {
            return finish(Err(if is_d3d9_only(&modules) { Refusal::D3d9Only } else { Refusal::NotAGame }));
        }
        finish(Ok(Candidate { pid, hwnd, exe, api }))
    }
}

/// The base names of every 64-bit module loaded in `process`.
///
/// # Safety
/// `process` must be a live handle with `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`.
unsafe fn module_names(process: windows::Win32::Foundation::HANDLE) -> Vec<String> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleBaseNameW, LIST_MODULES_64BIT};

    let mut modules = vec![HMODULE::default(); 1024];
    let mut needed = 0u32;
    // SAFETY: the buffer and its byte size agree; failure leaves `needed` unset
    // and is treated as "no modules", which simply means "not a game".
    let ok = unsafe {
        EnumProcessModulesEx(
            process,
            modules.as_mut_ptr(),
            (modules.len() * size_of::<HMODULE>()) as u32,
            &mut needed,
            LIST_MODULES_64BIT,
        )
    };
    if ok.is_err() {
        return Vec::new();
    }
    let count = (needed as usize / size_of::<HMODULE>()).min(modules.len());
    let mut names = Vec::with_capacity(count);
    for module in &modules[..count] {
        let mut buf = [0u16; 260];
        // SAFETY: a module handle from the enumeration above.
        let len = unsafe { GetModuleBaseNameW(process, Some(*module), &mut buf) } as usize;
        if len > 0 {
            names.push(String::from_utf16_lossy(&buf[..len]));
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identifies_the_graphics_api() {
        assert_eq!(classify_modules(&mods(&["d3d11.dll", "dxgi.dll"])).0, GfxApi::D3D11);
        assert_eq!(classify_modules(&mods(&["opengl32.dll"])).0, GfxApi::OpenGl);
        assert_eq!(classify_modules(&mods(&["kernel32.dll"])).0, GfxApi::Unknown);
        // A D3D12 game usually has d3d11 loaded too, for video; the newer API wins.
        assert_eq!(classify_modules(&mods(&["d3d11.dll", "d3d12.dll", "dxgi.dll"])).0, GfxApi::D3D12);
        // So does a Vulkan game, and there the D3D modules are not the renderer.
        assert_eq!(classify_modules(&mods(&["vulkan-1.dll", "dxgi.dll", "d3d11.dll"])).0, GfxApi::Vulkan);
    }

    #[test]
    fn module_paths_are_matched_by_file_name() {
        let full = mods(&["C:\\Windows\\System32\\d3d11.dll"]);
        assert_eq!(classify_modules(&full).0, GfxApi::D3D11);
        // Case never matters on Windows.
        assert_eq!(classify_modules(&mods(&["D3D11.DLL"])).0, GfxApi::D3D11);
    }

    #[test]
    fn finds_anti_cheat_modules() {
        for (module, expected) in [
            ("EasyAntiCheat_x64.dll", AntiCheat::EasyAntiCheat),
            ("BEClient_x64.dll", AntiCheat::BattlEye),
            ("vgc.dll", AntiCheat::Vanguard),
            ("PnkBstrA.exe", AntiCheat::PunkBuster),
        ] {
            let (_, ac) = classify_modules(&mods(&["d3d11.dll", module]));
            assert_eq!(ac, Some(expected), "{module} should have been recognised");
        }
        // An unlisted one is still reported, named after its own module.
        let (_, ac) = classify_modules(&mods(&["d3d11.dll", "mhyprot3.dll"]));
        assert!(matches!(ac, Some(AntiCheat::Other(ref n)) if n.contains("mhyprot")));
        // A clean game is not accused of anything.
        assert_eq!(classify_modules(&mods(&["d3d11.dll", "dxgi.dll"])).1, None);
    }

    #[test]
    fn anti_cheat_is_found_whatever_the_api() {
        // The refusal must not depend on recognising the renderer.
        let (api, ac) = classify_modules(&mods(&["EasyAntiCheat_x64.dll"]));
        assert_eq!(api, GfxApi::Unknown);
        assert_eq!(ac, Some(AntiCheat::EasyAntiCheat));
    }

    #[test]
    fn excludes_browsers_shell_and_ourselves() {
        for exe in ["chrome.exe", "explorer.exe", "Code.exe", "openclip.exe", "C:\\Windows\\explorer.exe"] {
            assert!(is_excluded(exe), "{exe} should be excluded");
        }
        for exe in ["game.exe", "Cyberpunk2077.exe", "hl2.exe"] {
            assert!(!is_excluded(exe), "{exe} should not be excluded");
        }
    }

    #[test]
    fn spots_a_direct3d_9_only_process() {
        assert!(is_d3d9_only(&mods(&["d3d9.dll", "kernel32.dll"])));
        // Not "9 only" once something newer is there.
        assert!(!is_d3d9_only(&mods(&["d3d9.dll", "d3d11.dll"])));
        assert!(!is_d3d9_only(&mods(&["kernel32.dll"])));
    }

    #[test]
    fn anti_cheat_labels_are_human_readable() {
        assert_eq!(AntiCheat::EasyAntiCheat.label(), "Easy Anti-Cheat");
        assert_eq!(AntiCheat::Other("acme".into()).label(), "acme");
    }
}
