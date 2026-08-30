//! Drives game capture with no GUI: probe a process, inject the hook, and print
//! what the control block says once a second.
//!
//! This is the headless harness for everything in `src/game/` — the analogue of
//! `list_encoders` for the hook.
//!
//! ```sh
//! cargo run --example inject_test -- --pid 1234
//! cargo run --example inject_test -- --exe gfx_sandbox     # match by window title
//! cargo run --example inject_test -- --exe game --record   # also flip to "recording" (red)
//! ```

#[cfg(not(windows))]
fn main() {
    eprintln!("game capture is Windows-only");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(windows)]
mod win {
    use std::time::{Duration, Instant};

    use anyhow::{bail, Result};
    use openclip::game::probe::{classify_modules, is_excluded};
    use openclip::game::{inject, HookSession};
    use openclip_overlay::abi::OverlaySettings;
    use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    pub fn run() -> Result<()> {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        let mut pid = None;
        let mut needle = None;
        let mut record = false;
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--pid" => pid = it.next().and_then(|v| v.parse::<u32>().ok()),
                "--exe" => needle = it.next(),
                "--record" => record = true,
                other => eprintln!("ignoring unknown flag {other}"),
            }
        }

        let Some(dll) = inject::hook_dll_path() else {
            bail!("{} not found; run `cargo build` so it sits beside the exe", inject::HOOK_DLL);
        };
        println!("hook component: {}", dll.display());

        let (pid, hwnd) = match (pid, needle) {
            (Some(pid), _) => (pid, main_window_of(pid).unwrap_or(0)),
            (None, Some(n)) => find_window(&n)?,
            _ => bail!("pass --pid <n> or --exe <window-title-substring>"),
        };
        if hwnd == 0 {
            bail!("no visible top-level window found for pid {pid}; there is nothing to hook");
        }
        println!("target: pid {pid}, hwnd {hwnd:#x}");

        // The control block must exist before injecting: the hook looks for it
        // as soon as it starts and is told nothing else about us.
        let session = HookSession::create(pid, hwnd)?;
        session.set_capture_fps(60);
        session.set_overlay(OverlaySettings {
            enabled: true,
            corner: 0, // top-left, where in-game counters live
            size: 100,
            opacity: 100,
            burn_in: false,
        });
        session.arm(true);

        inject::inject(hwnd)?;
        println!("injected; waiting for the hook to report...");
        if !session.wait_for_hook(Duration::from_secs(5)) {
            bail!("the hook did not attach within 5 s — see %LOCALAPPDATA%\\openclip\\hook-{pid}.log");
        }
        let v = session.hook_version().expect("just reported");
        println!("hook {}.{}.{} attached", v.0, v.1, v.2);

        if record {
            println!("switching the counter to recording (red)");
            session.set_capturing(true);
        }

        // Ctrl-C is the way out; print a line a second until then.
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let c = session.control();
            println!(
                "{:5.1}s  api {:<12} present {:>7}  {:6.1} fps  alive {}  error {:?} {}",
                start.elapsed().as_secs_f32(),
                session.api().label(),
                c.present_count.load(std::sync::atomic::Ordering::Relaxed),
                session.present_fps(),
                session.is_alive(Duration::from_secs(2)),
                session.error(),
                c.error_detail().unwrap_or(""),
            );
            if !session.is_alive(Duration::from_secs(5)) && start.elapsed() > Duration::from_secs(6) {
                println!("the target stopped presenting; giving up");
                break;
            }
        }
        Ok(())
    }

    /// What [`main_window_of`] hands its callback, since `EnumWindows` carries
    /// no typed payload of its own.
    struct Search {
        pid: u32,
        found: isize,
    }

    /// The first visible top-level window belonging to `pid`.
    fn main_window_of(pid: u32) -> Option<isize> {
        let mut search = Search { pid, found: 0 };
        // SAFETY: `EnumWindows` calls back synchronously with our own pointer,
        // which outlives the call.
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&raw mut search as isize));
        }
        (search.found != 0).then_some(search.found)
    }

    unsafe extern "system" fn cb(hwnd: HWND, param: LPARAM) -> windows::core::BOOL {
        // SAFETY: `param` is the `Search` handed to EnumWindows above.
        let search = unsafe { &mut *(param.0 as *mut Search) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == search.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.found = hwnd.0 as isize;
            return windows::core::BOOL(0); // stop enumerating
        }
        TRUE.into()
    }

    /// Finds a visible window whose title contains `needle`, and reports what
    /// the probe makes of the process behind it.
    fn find_window(needle: &str) -> Result<(u32, isize)> {
        let needle = needle.to_ascii_lowercase();
        let mut hits = Vec::new();
        // SAFETY: synchronous enumeration into a local vector.
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut hits as *mut Vec<(u32, isize, String)> as isize));
        }
        let Some((pid, hwnd, title)) = hits.into_iter().find(|(_, _, t)| t.to_ascii_lowercase().contains(&needle))
        else {
            bail!("no visible window whose title contains {needle:?}");
        };
        println!("matched window {title:?}");
        // The real probe runs in openclip; this only reports the pure parts so a
        // refusal is visible here too rather than surfacing as a silent no-op.
        if is_excluded(&title) {
            println!("note: a window like this would be excluded by the real probe");
        }
        Ok((pid, hwnd))
    }

    unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> windows::core::BOOL {
        // SAFETY: `param` is the vector handed to EnumWindows above.
        let out = unsafe { &mut *(param.0 as *mut Vec<(u32, isize, String)>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut buf = [0u16; 256];
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
            if len > 0 {
                let mut pid = 0;
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                out.push((pid, hwnd.0 as isize, String::from_utf16_lossy(&buf[..len])));
            }
        }
        TRUE.into()
    }

    /// Keeps `classify_modules` referenced from the example so its shape stays
    /// honest even before the watcher wires it in.
    #[allow(dead_code)]
    fn describe(modules: &[String]) -> String {
        let (api, ac) = classify_modules(modules);
        match ac {
            Some(ac) => format!("{} (refused: {})", api.label(), ac.label()),
            None => api.label().to_string(),
        }
    }
}
