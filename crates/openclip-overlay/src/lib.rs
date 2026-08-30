//! Overlay drawing shared between openclip and the code injected into games.
//!
//! openclip composes the watermark badge here and blends it into captured
//! frames; the hook DLL composes the FPS counter here and uploads it to the
//! game's GPU. Keeping one rasteriser means the two overlays cannot drift apart
//! in metrics, corner placement or colour — and the hook gets text rendering
//! without linking egui, which it could not do anyway inside someone else's
//! process.
//!
//! Deliberately dependency-light for the same reason: this code is mapped into
//! other people's games, so it installs no logger, starts no runtime and
//! allocates nothing at load time.

pub mod abi;
pub mod fps;
pub mod layout;
pub mod sprite;
pub mod text;

pub use abi::{Control, GfxApi, HookError, OverlaySettings, Transport, HOOK_ABI_VERSION, HOOK_MAGIC};
pub use fps::{format_fps, FpsBadge, FpsOverlay, HookState};
pub use layout::{Corner, Layout};
pub use sprite::{rounded_box_sdf, Sprite};
pub use text::{TextRenderer, INTER_SEMIBOLD};
