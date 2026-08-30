//! Game recording: loading openclip's hook into a game so frames can be taken
//! at the source and a frame-rate counter drawn into the game's own picture.
//!
//! Windows only, and 64-bit games only. A 32-bit target is reported as such
//! rather than failing quietly — see [`probe`].
//!
//! ## Posture
//!
//! This subsystem loads code into another process, so it is built to be obvious
//! rather than stealthy: a plainly-named DLL with a version resource, loaded
//! with a documented first-party API, announced on screen by the counter it
//! draws, and refused outright for any process running an anti-cheat. There is
//! no override for that refusal, and no evasion of any kind.

#[cfg(windows)]
pub mod inject;
#[cfg(windows)]
pub mod probe;
#[cfg(windows)]
pub mod shared;

#[cfg(windows)]
pub use inject::{hook_dll_path, is_available, HOOK_DLL};
#[cfg(windows)]
pub use probe::{AntiCheat, Candidate, Refusal};
#[cfg(windows)]
pub use shared::HookSession;

/// Whether this build can record games at all.
pub fn supported() -> bool {
    cfg!(windows)
}
