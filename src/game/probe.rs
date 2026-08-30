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
