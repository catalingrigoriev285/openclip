//! Font Awesome 6 Free (Solid) icons. The font is registered as a glyph
//! fallback for every family by [`super::theme::install_fonts`], so the
//! codepoints below can be used inside any egui text.
//!
//! Font: SIL OFL 1.1 — see `assets/fonts/FONT-AWESOME-LICENSE.txt`.

pub(super) const FA_SOLID: &[u8] = include_bytes!("../../assets/fonts/fa-solid-900.ttf");

// Recording modes
pub const REGION: &str = "\u{f5cb}"; // vector-square
pub const MONITOR: &str = "\u{f390}"; // desktop
pub const WINDOW: &str = "\u{f2d0}"; // window-maximize
pub const GAMEPAD: &str = "\u{f11b}"; // gamepad

// Inputs
pub const SPEAKER: &str = "\u{f028}"; // volume-high
pub const MIC: &str = "\u{f130}"; // microphone
pub const CURSOR: &str = "\u{f245}"; // arrow-pointer

// Transport / actions
pub const PLAY: &str = "\u{f04b}";
pub const PAUSE: &str = "\u{f04c}";
pub const STOP: &str = "\u{f04d}";
pub const CAMERA: &str = "\u{f030}";
pub const MINIMIZE: &str = "\u{f066}"; // compress
pub const REFRESH: &str = "\u{f021}"; // arrows-rotate
pub const DOWNLOAD: &str = "\u{f019}"; // download
pub const FOLDER: &str = "\u{f07c}"; // folder-open
pub const TRASH: &str = "\u{f2ed}"; // trash-can
pub const CHECK: &str = "\u{f00c}";
pub const XMARK: &str = "\u{f00d}";
pub const CARET_DOWN: &str = "\u{f0d7}";
pub const CHEVRON_RIGHT: &str = "\u{f054}";
pub const CHEVRON_LEFT: &str = "\u{f053}";
pub const CIRCLE: &str = "\u{f111}";
pub const RECORD: &str = "\u{f192}"; // circle-dot

// Media viewer transport
pub const STEP_BACK: &str = "\u{f048}"; // backward-step
pub const STEP_FORWARD: &str = "\u{f051}"; // forward-step
pub const VOLUME_MUTE: &str = "\u{f6a9}"; // volume-xmark
pub const EXPAND: &str = "\u{f065}"; // expand (image at 1:1)

// Navigation
pub const HOME: &str = "\u{f015}"; // house
pub const GEAR: &str = "\u{f013}";
pub const FILM: &str = "\u{f008}";
pub const IMAGE: &str = "\u{f03e}";
pub const MUSIC: &str = "\u{f001}"; // music
pub const INFO: &str = "\u{f05a}"; // circle-info
pub const MOUSE: &str = "\u{f8cc}"; // computer-mouse
