//! Font Awesome 6 Free (Solid) icons, embedded and registered as a fallback
//! font so the codepoints below can be used inside any egui text.
//!
//! Font: SIL OFL 1.1 — see `assets/fonts/FONT-AWESOME-LICENSE.txt`.

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

const FA_SOLID: &[u8] = include_bytes!("../../assets/fonts/fa-solid-900.ttf");

/// Registers the icon font as a fallback for both default families and as
/// the named family `"fa"`.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("fa-solid".to_owned(), Arc::new(FontData::from_static(FA_SOLID)));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("fa-solid".to_owned());
    }
    fonts.families.insert(FontFamily::Name("fa".into()), vec!["fa-solid".to_owned()]);
    ctx.set_fonts(fonts);
}

// Recording modes
pub const REGION: &str = "\u{f5cb}"; // vector-square
pub const MONITOR: &str = "\u{f390}"; // desktop
pub const WINDOW: &str = "\u{f2d0}"; // window-maximize

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
pub const EXPAND: &str = "\u{f065}"; // expand
pub const REFRESH: &str = "\u{f021}"; // arrows-rotate
pub const FOLDER: &str = "\u{f07c}"; // folder-open
pub const TRASH: &str = "\u{f2ed}"; // trash-can
pub const CHECK: &str = "\u{f00c}";
pub const XMARK: &str = "\u{f00d}";
pub const CARET_DOWN: &str = "\u{f0d7}";
pub const RECORD: &str = "\u{f192}"; // circle-dot

// Navigation
pub const HOME: &str = "\u{f015}"; // house
pub const GEAR: &str = "\u{f013}";
pub const FILM: &str = "\u{f008}";
pub const IMAGE: &str = "\u{f03e}";
pub const INFO: &str = "\u{f05a}"; // circle-info
pub const MOUSE: &str = "\u{f8cc}"; // computer-mouse
