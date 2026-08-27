//! Dark navy colour scheme and egui visuals.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const TOOLBAR_BG: Color32 = Color32::from_rgb(0x1b, 0x22, 0x2e);
pub const STATUS_BG: Color32 = Color32::from_rgb(0x24, 0x2d, 0x3b);
pub const NAV_BG: Color32 = Color32::from_rgb(0x20, 0x28, 0x35);
pub const NAV_SELECTED: Color32 = Color32::from_rgb(0x2c, 0x37, 0x48);
pub const PAGE_BG: Color32 = Color32::from_rgb(0x2b, 0x35, 0x44);
pub const PREVIEW_BG: Color32 = Color32::from_rgb(0x12, 0x16, 0x1c);
pub const BUTTON_BG: Color32 = Color32::from_rgb(0x34, 0x40, 0x52);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x40, 0x4e, 0x63);
pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x7b, 0xe0);
pub const SEPARATOR: Color32 = Color32::from_rgb(0x46, 0x53, 0x66);
pub const TEXT_BRIGHT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xf7);
pub const TEXT_NORMAL: Color32 = Color32::from_rgb(0xc9, 0xd0, 0xda);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8d, 0x98, 0xa8);
pub const REC_RED: Color32 = Color32::from_rgb(0xe6, 0x28, 0x28);
pub const REC_RED_HOVER: Color32 = Color32::from_rgb(0xff, 0x44, 0x44);
pub const ERR_RED: Color32 = Color32::from_rgb(0xf0, 0x60, 0x60);
pub const OK_GREEN: Color32 = Color32::from_rgb(0x5f, 0xd0, 0x80);
pub const WARN_YELLOW: Color32 = Color32::from_rgb(0xf0, 0xc0, 0x40);

pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PAGE_BG;
    visuals.window_fill = PAGE_BG;
    visuals.extreme_bg_color = Color32::from_rgb(0x1e, 0x25, 0x31);
    visuals.faint_bg_color = Color32::from_rgb(0x30, 0x3b, 0x4c);
    visuals.override_text_color = Some(TEXT_NORMAL);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, TEXT_BRIGHT);
    visuals.hyperlink_color = Color32::from_rgb(0x7f, 0xb3, 0xff);

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = PAGE_BG;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEPARATOR);
    widgets.inactive.bg_fill = BUTTON_BG;
    widgets.inactive.weak_bg_fill = BUTTON_BG;
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.hovered.bg_fill = BUTTON_HOVER;
    widgets.hovered.weak_bg_fill = BUTTON_HOVER;
    widgets.hovered.bg_stroke = Stroke::new(1.0, SEPARATOR);
    widgets.active.bg_fill = ACCENT;
    widgets.active.weak_bg_fill = ACCENT;
    widgets.open.bg_fill = BUTTON_HOVER;
    widgets.open.weak_bg_fill = BUTTON_HOVER;
    for w in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(3);
    }
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    ctx.set_style_of(egui::Theme::Dark, style);
}
