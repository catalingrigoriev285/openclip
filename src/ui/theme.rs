use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Shadow, Stroke, TextStyle,
};

use super::icons;

/// Window / page / sidebar background (systemBackground).
pub const BG: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
/// Inset-grouped cards and modal sheets (secondarySystemGroupedBackground).
pub const CARD: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1e);
/// Buttons, fields, tiles, segmented track (tertiarySystemFill).
pub const FILL: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2e);
pub const FILL_HOVER: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3c);
/// Segmented knob, switch-off track, pressed state.
pub const FILL_STRONG: Color32 = Color32::from_rgb(0x48, 0x48, 0x4a);
pub const SEPARATOR: Color32 = Color32::from_rgb(0x38, 0x38, 0x3a);
pub const LABEL: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// Secondary text (systemGray).
pub const LABEL_2: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x93);
/// Tertiary / disabled text.
pub const LABEL_3: Color32 = Color32::from_rgb(0x63, 0x63, 0x66);
pub const BLUE: Color32 = Color32::from_rgb(0x0a, 0x84, 0xff);
pub const BLUE_HOVER: Color32 = Color32::from_rgb(0x33, 0x96, 0xff);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
pub const RED_HOVER: Color32 = Color32::from_rgb(0xff, 0x69, 0x61);
pub const GREEN: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
pub const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9f, 0x0a);
/// Preview well behind captured frames.
pub const PREVIEW_BG: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const CHECKER_LIGHT: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);
pub const CHECKER_DARK: Color32 = Color32::from_rgb(0xb4, 0xb4, 0xb4);

/// `color` at the given opacity (iOS "tinted" fills are the accent at ~18 %).
pub fn tinted(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Linear blend of two opaque colours.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()), ch(a.a(), b.a()))
}

// ----- typography --------------------------------------------------------------

const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// Font family carrying Inter SemiBold (titles, selected segments, primary buttons).
pub fn semibold_family() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, semibold_family())
}

/// Page / dialog heading (20 pt semibold).
pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text).font(semibold(20.0)).color(LABEL)
}

/// Secondary 13 pt text.
pub fn secondary(text: impl Into<String>) -> RichText {
    RichText::new(text).color(LABEL_2)
}

/// Registers Inter (regular as the default proportional face, semibold as the
/// `"semibold"` family) and Font Awesome as a glyph fallback for every family.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".to_owned(), Arc::new(FontData::from_static(INTER_REGULAR)));
    fonts.font_data.insert("inter-semibold".to_owned(), Arc::new(FontData::from_static(INTER_SEMIBOLD)));
    fonts.font_data.insert("fa-solid".to_owned(), Arc::new(FontData::from_static(icons::FA_SOLID)));

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "inter".to_owned());
    proportional.push("fa-solid".to_owned());
    let mut semibold = vec!["inter-semibold".to_owned()];
    semibold.extend(proportional.iter().cloned());
    fonts.families.entry(FontFamily::Monospace).or_default().push("fa-solid".to_owned());
    fonts.families.insert(semibold_family(), semibold);
    ctx.set_fonts(fonts);
}

// ----- visuals -------------------------------------------------------------------

pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = FILL;
    visuals.faint_bg_color = FILL;
    visuals.code_bg_color = FILL;
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(LABEL_2);
    visuals.selection.bg_fill = BLUE;
    visuals.selection.stroke = Stroke::new(1.0, LABEL);
    visuals.hyperlink_color = BLUE;
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.window_stroke = Stroke::NONE;
    visuals.window_shadow = Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(150) };
    visuals.menu_corner_radius = CornerRadius::same(12);
    visuals.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(120) };
    visuals.slider_trailing_fill = true;
    visuals.text_cursor.stroke.color = BLUE;

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = CARD;
    widgets.noninteractive.weak_bg_fill = CARD;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEPARATOR);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, LABEL);
    widgets.inactive.bg_fill = FILL;
    widgets.inactive.weak_bg_fill = FILL;
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.inactive.fg_stroke = Stroke::new(1.0, LABEL);
    widgets.hovered.bg_fill = FILL_HOVER;
    widgets.hovered.weak_bg_fill = FILL_HOVER;
    widgets.hovered.bg_stroke = Stroke::NONE;
    widgets.hovered.fg_stroke = Stroke::new(1.0, LABEL);
    widgets.active.bg_fill = FILL_STRONG;
    widgets.active.weak_bg_fill = FILL_STRONG;
    widgets.active.bg_stroke = Stroke::NONE;
    widgets.active.fg_stroke = Stroke::new(1.0, LABEL);
    widgets.open.bg_fill = FILL_HOVER;
    widgets.open.weak_bg_fill = FILL_HOVER;
    widgets.open.bg_stroke = Stroke::NONE;
    widgets.open.fg_stroke = Stroke::new(1.0, LABEL);
    for w in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(8);
        w.expansion = 0.0;
    }
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 6.0);
    style.spacing.interact_size = egui::vec2(40.0, 30.0);
    style.spacing.combo_width = 200.0;
    style.spacing.slider_width = 160.0;
    style.spacing.icon_width = 20.0;
    style.spacing.menu_margin = egui::Margin::same(8);
    style.interaction.selectable_labels = false;
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Name("title".into()), semibold(15.0)),
    ]
    .into();
    ctx.set_style_of(egui::Theme::Dark, style);
}
