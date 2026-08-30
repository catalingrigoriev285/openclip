//! Arm and wait: watch for a game coming to the foreground, and hook it.
//!
//! This is the shape Game Recording Mode has, and it is the right one
//! — you cannot alt-tab into a fullscreen game to pick it from a list, because
//! the moment you alt-tab it is no longer the thing you wanted to pick. So
//! openclip arms instead, and whatever graphics application comes to the front
//! next gets hooked and starts showing its frame-rate counter.
//!
//! Polling `GetForegroundWindow` rather than `SetWinEventHook`: the event hook
//! has to be installed from a thread with a message loop, which would tie this
//! to eframe's, and it misses the case where a game goes exclusive-fullscreen
//! without a foreground change. A 400 ms poll costs nothing and has neither
//! problem.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use openclip_overlay::abi::{GfxApi, OverlaySettings};

use super::probe::{self, Candidate, Refusal};
use super::{inject, HookSession};

/// How often to look at the foreground window.
const POLL: Duration = Duration::from_millis(400);
/// How long to give the hook to report itself after injection.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// What the watcher has to say. Consumed by the GUI exactly like
/// [`crate::ui::updater::UpdateState`]'s channel.
pub enum WatchEvent {
    /// Armed, nothing hooked yet.
    Waiting,
    /// A window came forward and was rejected. Reported rather than swallowed,
    /// because "nothing happened" is the worst possible feedback here.
    Refused { exe: String, reason: Refusal },
    /// The hook is in and reporting.
    Hooked { exe: String, api: GfxApi, session: Box<HookSession> },
    /// Injection was attempted and failed.
    Failed { exe: String, error: String },
}

/// What the GUI shows.
pub enum WatchState {
    Off,
    Waiting,
    Refused { exe: String, reason: Refusal },
    Hooked { exe: String, api: GfxApi, session: Box<HookSession> },
    Failed { exe: String, error: String },
}

impl WatchState {
    /// The hooked game's pid, once there is one — this is what makes
    /// [`crate::capture::Source::Game`] available.
    pub fn hooked_pid(&self) -> Option<u32> {
        match self {
            WatchState::Hooked { session, .. } => Some(session.pid()),
            _ => None,
        }
    }

    pub fn session(&self) -> Option<&HookSession> {
        match self {
            WatchState::Hooked { session, .. } => Some(session),
            _ => None,
        }
    }

    pub fn is_armed(&self) -> bool {
        !matches!(self, WatchState::Off)
    }
}

/// Owns the watcher thread and the state it reports.
pub struct GameWatcher {
    stop: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<WatchEvent>>,
    thread: Option<JoinHandle<()>>,
    /// Executables the user has said never to hook, shared with the thread.
    ignored: Arc<Mutex<HashSet<String>>>,
    pub state: WatchState,
}

impl Default for GameWatcher {
    fn default() -> Self {
        Self { stop: Arc::new(AtomicBool::new(false)), rx: None, thread: None, ignored: Default::default(), state: WatchState::Off }
    }
}

impl GameWatcher {
    /// Starts watching. Idempotent.
    ///
    /// `repaint` is called whenever there is something new to show, so the GUI
    /// wakes up rather than waiting for its next idle frame.
    pub fn arm(&mut self, ignored: &[String], repaint: impl Fn() + Send + 'static) {
        if self.thread.is_some() {
            return;
        }
        if !inject::is_available() {
            self.state = WatchState::Failed {
                exe: String::new(),
                error: crate::t!(MSG_GAME_DLL_MISSING).to_string(),
            };
            return;
        }
        *self.ignored.lock().unwrap() = ignored.iter().map(|s| s.to_ascii_lowercase()).collect();
        self.stop.store(false, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        let stop = self.stop.clone();
        let skip = self.ignored.clone();
        let spawned = std::thread::Builder::new()
            .name("openclip-gamewatch".into())
            .spawn(move || watch(tx, stop, skip, repaint));
        match spawned {
            Ok(handle) => {
                self.thread = Some(handle);
                self.rx = Some(rx);
                self.state = WatchState::Waiting;
            }
            Err(e) => self.state = WatchState::Failed { exe: String::new(), error: e.to_string() },
        }
    }

    /// Stops watching and lets go of whatever is hooked.
    pub fn disarm(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.rx = None;
        // Dropping the session tells the hook to detach and take its counter
        // with it, which is what "no longer in Game mode" should look like.
        self.state = WatchState::Off;
    }

    /// Never hook this executable again while openclip is running.
    pub fn ignore(&mut self, exe: &str) {
        self.ignored.lock().unwrap().insert(exe.to_ascii_lowercase());
        if matches!(&self.state, WatchState::Hooked { exe: e, .. } if e.eq_ignore_ascii_case(exe)) {
            self.state = WatchState::Waiting;
        }
    }

    /// Picks up whatever the thread has found (called every frame).
    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(event) = rx.try_recv() {
            self.state = match event {
                WatchEvent::Waiting => WatchState::Waiting,
                WatchEvent::Refused { exe, reason } => WatchState::Refused { exe, reason },
                WatchEvent::Hooked { exe, api, session } => WatchState::Hooked { exe, api, session },
                WatchEvent::Failed { exe, error } => WatchState::Failed { exe, error },
            };
        }
        // A hooked game that stopped presenting has exited or hung; go back to
        // waiting so the next one is picked up.
        if let WatchState::Hooked { session, .. } = &self.state
            && !session.is_alive(Duration::from_secs(5))
        {
            self.state = WatchState::Waiting;
        }
    }

    /// Pushes the counter's appearance to whatever is hooked. Cheap enough to
    /// call every frame: it is one atomic store.
    pub fn push_overlay(&self, settings: OverlaySettings) {
        if let Some(session) = self.state.session() {
            session.set_overlay(settings);
            session.arm(settings.enabled);
        }
    }
}

impl Drop for GameWatcher {
    fn drop(&mut self) {
        self.disarm();
    }
}

fn watch(
    tx: mpsc::Sender<WatchEvent>,
    stop: Arc<AtomicBool>,
    ignored: Arc<Mutex<HashSet<String>>>,
    repaint: impl Fn(),
) {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    // Windows already looked at and decided about, so the same refusal is not
    // re-sent (and the same game not re-probed) four times a second.
    let mut seen: HashSet<isize> = HashSet::new();
    let mut hooked: Option<u32> = None;

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
        // SAFETY: a plain query; a null result just means nothing is focused.
        let hwnd = unsafe { GetForegroundWindow() }.0 as isize;
        if hwnd == 0 || seen.contains(&hwnd) {
            continue;
        }
        seen.insert(hwnd);
        // Bounded, so a long session flicking between windows cannot grow this
        // without limit.
        if seen.len() > 256 {
            seen.clear();
        }

        let candidate = match probe::probe_window(hwnd) {
            Ok(c) => c,
            Err(Refusal::OwnProcess | Refusal::NotAGame | Refusal::Excluded) => continue, // silent by design
            Err(reason) => {
                // Everything else is worth telling the user about: a refusal
                // they cannot see looks exactly like a broken feature.
                let exe = exe_of(hwnd);
                let _ = tx.send(WatchEvent::Refused { exe, reason });
                repaint();
                continue;
            }
        };

        if ignored.lock().unwrap().contains(&candidate.exe.to_ascii_lowercase()) {
            continue;
        }
        if hooked == Some(candidate.pid) {
            continue;
        }

        match attach(&candidate) {
            Ok(session) => {
                log::info!("game: hooked {} (pid {}, {})", candidate.exe, candidate.pid, candidate.api.label());
                hooked = Some(candidate.pid);
                let _ = tx.send(WatchEvent::Hooked {
                    exe: candidate.exe,
                    api: candidate.api,
                    session: Box::new(session),
                });
            }
            Err(e) => {
                log::warn!("game: could not hook {}: {e:#}", candidate.exe);
                let _ = tx.send(WatchEvent::Failed { exe: candidate.exe, error: format!("{e:#}") });
            }
        }
        repaint();
    }
}

/// Creates the control block and gets the hook in.
fn attach(candidate: &Candidate) -> anyhow::Result<HookSession> {
    // The block first, always: the hook looks for it the moment it starts and is
    // told nothing else about us.
    let session = HookSession::create(candidate.pid, candidate.hwnd)?;
    session.arm(true);
    inject::inject(candidate.hwnd, || session.is_hooked(), ATTACH_TIMEOUT)?;
    Ok(session)
}

/// The executable behind a window, for a message. Best effort.
fn exe_of(hwnd: isize) -> String {
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    // SAFETY: read-only queries; the handle is closed before returning.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(HWND(hwnd as *mut std::ffi::c_void), Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let name = match QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        ) {
            Ok(()) => {
                let path = String::from_utf16_lossy(&buf[..len as usize]);
                path.rsplit('\\').next().unwrap_or(&path).to_string()
            }
            Err(_) => String::new(),
        };
        let _ = CloseHandle(process);
        name
    }
}

/// The localized explanation for a refusal.
pub fn refusal_message(exe: &str, reason: &Refusal) -> String {
    let exe = if exe.is_empty() { "?" } else { exe };
    match reason {
        Refusal::AntiCheat(ac) => crate::t!(MSG_GAME_REFUSED_ANTICHEAT, exe, ac.label()),
        Refusal::NotX64 => crate::t!(MSG_GAME_REFUSED_X86, exe),
        Refusal::D3d9Only => crate::t!(MSG_GAME_REFUSED_D3D9, exe),
        Refusal::AccessDenied => crate::t!(MSG_GAME_REFUSED_PROTECTED, exe),
        // The quiet ones never reach the user; this keeps the match total.
        Refusal::OwnProcess | Refusal::NotAGame | Refusal::Excluded => String::new(),
    }
}
