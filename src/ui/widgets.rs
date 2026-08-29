use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, Response, RichText, Sense, Shape, Stroke, Ui,
    UiBuilder, Vec2,
};

use super::icons;
use super::theme::*;
use crate::t;

/// Height of a card row.
pub const ROW_H: f32 = 44.0;
/// Horizontal padding inside cards and of section headers.
pub const PAD: f32 = 16.0;

/// Uppercase grey caption above a card ("OUTPUT").
pub fn section_header(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(PAD);
        ui.label(RichText::new(title.to_uppercase()).size(12.0).color(LABEL_2));
    });
    ui.add_space(2.0);
}

/// Small grey note under a card.
pub fn footnote(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(PAD);
        ui.label(RichText::new(text).size(12.0).color(LABEL_2));
    });
}

// ----- cards ------------------------------------------------------------------

/// Inset-grouped list: rounded card whose rows are separated by hairlines
/// inset from the left edge, like iOS Settings.
pub struct Card<'u> {
    ui: &'u mut Ui,
    rows: usize,
}

impl Card<'_> {
    pub fn show(ui: &mut Ui, add: impl FnOnce(&mut Card<'_>)) {
        Self::show_with(ui, CARD, add);
    }

    /// Card with a custom fill (`FILL` for cards placed on a `CARD` sheet).
    pub fn show_with(ui: &mut Ui, fill: Color32, add: impl FnOnce(&mut Card<'_>)) {
        egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut card = Card { ui, rows: 0 };
            add(&mut card);
        });
        ui.add_space(6.0);
    }

    fn separator(&mut self) {
        if self.rows > 0 {
            let r = self.ui.available_rect_before_wrap();
            self.ui.painter().hline((r.left() + PAD)..=r.right(), r.top(), Stroke::new(1.0, SEPARATOR));
        }
        self.rows += 1;
    }

    /// Label on the left, controls right-aligned. The closure lays widgets out
    /// **right to left** (the first widget added ends up rightmost).
    pub fn row(&mut self, label: &str, add: impl FnOnce(&mut Ui)) -> Response {
        self.separator();
        let width = self.ui.available_width();
        self.ui
            .allocate_ui_with_layout(Vec2::new(width, ROW_H), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_height(ROW_H);
                ui.add_space(PAD);
                if !label.is_empty() {
                    ui.label(RichText::new(label).color(LABEL));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(PAD);
                    add(ui);
                });
            })
            .response
    }

    /// Label followed by controls flowing left to right (for rows whose
    /// content reads as a sentence: a value plus a note).
    pub fn row_inline(&mut self, label: &str, add: impl FnOnce(&mut Ui)) -> Response {
        self.separator();
        let width = self.ui.available_width();
        self.ui
            .allocate_ui_with_layout(Vec2::new(width, ROW_H), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_height(ROW_H);
                ui.add_space(PAD);
                if !label.is_empty() {
                    ui.label(RichText::new(label).color(LABEL));
                    ui.add_space(12.0);
                }
                add(ui);
                ui.add_space(PAD);
            })
            .response
    }

    /// Label with a right-aligned secondary value (truncated with an ellipsis).
    pub fn text_row(&mut self, label: &str, value: &str) -> Response {
        let value = value.to_owned();
        self.row(label, |ui| {
            ui.add(egui::Label::new(RichText::new(value).color(LABEL_2)).truncate());
        })
    }

    /// Clickable row with a value and a chevron ("Video    H.264 1080p ›").
    pub fn nav_row(&mut self, label: &str, value: &str) -> Response {
        self.separator();
        let width = self.ui.available_width();
        let (rect, resp) = self.ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
        if resp.hovered() && self.ui.is_enabled() {
            self.ui.painter().rect_filled(rect.shrink2(Vec2::new(6.0, 3.0)), CornerRadius::same(8), FILL_HOVER);
        }
        let inner = rect.shrink2(Vec2::new(PAD, 0.0));
        let mut child = self.ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
        child.label(RichText::new(label).color(LABEL));
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(icons::CHEVRON_RIGHT).size(12.0).color(LABEL_3));
            ui.add(egui::Label::new(RichText::new(value).color(LABEL_2)).truncate());
        });
        resp
    }

    /// Free-form content with 12 pt padding (lists, previews, text blocks).
    pub fn custom(&mut self, add: impl FnOnce(&mut Ui)) {
        self.separator();
        egui::Frame::new().inner_margin(egui::Margin::same(12)).show(self.ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui);
        });
    }

    /// Free-form content with no padding (the file list).
    pub fn flush(&mut self, add: impl FnOnce(&mut Ui)) {
        self.separator();
        add(self.ui);
    }
}

// ----- segmented control ---------------------------------------------------------

pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    id_salt: &str,
    items: &[(T, Option<&str>, &str)],
    selected: &mut T,
) -> bool {
    let font = FontId::proportional(13.0);
    let font_sel = semibold(13.0);
    let texts: Vec<String> = items
        .iter()
        .map(|(_, icon, label)| match icon {
            Some(i) => format!("{i}  {label}"),
            None => (*label).to_owned(),
        })
        .collect();
    let painter = ui.painter().clone();
    let widest =
        texts.iter().map(|t| painter.layout_no_wrap(t.clone(), font_sel.clone(), LABEL).size().x).fold(0.0, f32::max);
    let seg_w = (widest + 28.0).max(72.0);
    let n = items.len().max(1);
    let size = Vec2::new(seg_w * n as f32 + 4.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let enabled = ui.is_enabled();

    let mut changed = false;
    if resp.clicked()
        && enabled
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let i = (((pos.x - rect.left() - 2.0) / seg_w).floor().max(0.0) as usize).min(n - 1);
        if let Some((v, _, _)) = items.get(i)
            && *v != *selected
        {
            *selected = *v;
            changed = true;
        }
    }
    let sel_idx = items.iter().position(|(v, _, _)| *v == *selected).unwrap_or(0);
    let id = ui.id().with(id_salt);
    let knob_x = ui.ctx().animate_value_with_time(id, sel_idx as f32, 0.15);

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(9), if enabled { FILL } else { mix(FILL, BG, 0.4) });
    let knob = Rect::from_min_size(
        rect.min + Vec2::new(2.0 + knob_x * seg_w, 2.0),
        Vec2::new(seg_w, rect.height() - 4.0),
    );
    p.rect_filled(knob, CornerRadius::same(7), if enabled { FILL_STRONG } else { mix(FILL_STRONG, BG, 0.4) });
    for (i, text) in texts.iter().enumerate() {
        let is_sel = i == sel_idx;
        let color = if !enabled {
            LABEL_3
        } else if is_sel {
            LABEL
        } else {
            LABEL_2
        };
        let center = egui::pos2(rect.left() + 2.0 + seg_w * (i as f32 + 0.5), rect.center().y);
        let f = if is_sel { font_sel.clone() } else { font.clone() };
        p.text(center, Align2::CENTER_CENTER, text, f, color);
    }
    changed
}

// ----- switch ----------------------------------------------------------------------

pub fn switch(ui: &mut Ui, value: &mut bool) -> Response {
    let size = Vec2::new(44.0, 26.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    let enabled = ui.is_enabled();
    if resp.clicked() && enabled {
        *value = !*value;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *value, 0.15);
    let mut track = mix(FILL_STRONG, BLUE, t);
    let mut knob = Color32::WHITE;
    if !enabled {
        track = track.gamma_multiply(0.5);
        knob = knob.gamma_multiply(0.6);
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(13), track);
    let x = egui::lerp((rect.left() + 13.0)..=(rect.right() - 13.0), t);
    p.circle_filled(egui::pos2(x, rect.center().y), 11.0, knob);
    resp
}

/// `label` on the left of the row and a switch on the right; returns true when toggled.
pub fn switch_row(card: &mut Card<'_>, label: &str, value: &mut bool) -> bool {
    let mut changed = false;
    card.row(label, |ui| {
        changed = switch(ui, value).changed();
    });
    changed
}

// ----- buttons -------------------------------------------------------------------

fn pill_button(ui: &mut Ui, text: &str, fill: Color32, hover: Color32, fg: Color32, font: FontId, h: f32) -> Response {
    pill_button_min(ui, text, fill, hover, fg, font, h, 0.0)
}

#[allow(clippy::too_many_arguments)]
fn pill_button_min(
    ui: &mut Ui,
    text: &str,
    fill: Color32,
    hover: Color32,
    fg: Color32,
    font: FontId,
    h: f32,
    min_w: f32,
) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = Vec2::new((galley.size().x + h * 0.8).max(min_w), h);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let enabled = ui.is_enabled();
    let mut fill = if resp.hovered() && enabled { hover } else { fill };
    let mut fg = fg;
    if !enabled {
        fill = fill.gamma_multiply(0.45);
        fg = fg.gamma_multiply(0.45);
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((h / 3.4) as u8), fill);
    p.galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp
}

/// Filled blue button (the default action).
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, BLUE, BLUE_HOVER, Color32::WHITE, semibold(13.0), 34.0)
}

/// Primary button at least `min_w` wide (paired dialog buttons).
pub fn primary_button_min(ui: &mut Ui, text: &str, min_w: f32) -> Response {
    pill_button_min(ui, text, BLUE, BLUE_HOVER, Color32::WHITE, semibold(13.0), 34.0, min_w)
}

/// Grey button at least `min_w` wide (paired dialog buttons).
pub fn gray_button_min(ui: &mut Ui, text: &str, min_w: f32) -> Response {
    pill_button_min(ui, text, FILL, FILL_HOVER, LABEL, FontId::proportional(13.0), 34.0, min_w)
}

/// Blue text on a translucent blue fill (secondary actions).
pub fn tinted_button(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, tinted(BLUE, 46), tinted(BLUE, 70), BLUE, FontId::proportional(13.0), 34.0)
}

/// Compact tinted button for rows and strips.
pub fn tinted_button_small(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, tinted(BLUE, 46), tinted(BLUE, 70), BLUE, FontId::proportional(12.0), 26.0)
}

/// Grey filled button (neutral secondary action).
pub fn gray_button(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, FILL, FILL_HOVER, LABEL, FontId::proportional(13.0), 34.0)
}

/// Filled red button.
pub fn destructive_button(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, RED, RED_HOVER, Color32::WHITE, semibold(13.0), 34.0)
}

/// Red text on a translucent red fill.
pub fn destructive_tinted_button(ui: &mut Ui, text: &str) -> Response {
    pill_button(ui, text, tinted(RED, 46), tinted(RED, 70), RED, FontId::proportional(13.0), 34.0)
}

// ----- icon controls ------------------------------------------------------------

/// 40×40 rounded tile; blue with a white glyph when on, grey when off.
pub fn icon_tile(ui: &mut Ui, icon: &str, on: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    let enabled = ui.is_enabled();
    let hovered = resp.hovered() && enabled;
    let (mut fill, mut fg) = match (on, hovered) {
        (true, true) => (BLUE_HOVER, Color32::WHITE),
        (true, false) => (BLUE, Color32::WHITE),
        (false, true) => (FILL_HOVER, LABEL),
        (false, false) => (FILL, LABEL_2),
    };
    if !enabled {
        fill = fill.gamma_multiply(0.5);
        fg = fg.gamma_multiply(0.5);
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(10), fill);
    p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(17.0), fg);
    resp.on_hover_text(tip)
}

/// Icon tile that toggles `value` on click; tooltip shows the state.
pub fn icon_toggle(ui: &mut Ui, icon: &str, name: &str, value: &mut bool) -> Response {
    let tip = format!("{name}: {}", if *value { t!(ON) } else { t!(OFF) });
    let resp = icon_tile(ui, icon, *value, &tip);
    if resp.clicked() && ui.is_enabled() {
        *value = !*value;
    }
    resp
}

/// Round ghost button with an icon (toolbar utilities).
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::click());
    let enabled = ui.is_enabled();
    let p = ui.painter();
    if resp.hovered() && enabled {
        p.circle_filled(rect.center(), 17.0, FILL_HOVER);
    }
    let fg = if enabled { LABEL } else { LABEL_3 };
    p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), fg);
    resp.on_hover_text(tip)
}

/// Pause / resume button next to REC; only active while recording. Returns true on click.
pub fn pause_button(ui: &mut Ui, recording: bool, paused: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    let p = ui.painter();
    let fill = if !recording {
        FILL.gamma_multiply(0.6)
    } else if resp.hovered() {
        FILL_HOVER
    } else {
        FILL
    };
    p.circle_filled(rect.center(), 20.0, fill);
    let color = if !recording {
        LABEL_3
    } else if paused {
        ORANGE
    } else {
        LABEL
    };
    let c = rect.center();
    if paused {
        p.add(Shape::convex_polygon(
            vec![c + Vec2::new(-5.0, -8.0), c + Vec2::new(9.0, 0.0), c + Vec2::new(-5.0, 8.0)],
            color,
            Stroke::NONE,
        ));
    } else {
        p.rect_filled(Rect::from_center_size(c + Vec2::new(-4.0, 0.0), Vec2::new(4.0, 16.0)), CornerRadius::same(1), color);
        p.rect_filled(Rect::from_center_size(c + Vec2::new(4.0, 0.0), Vec2::new(4.0, 16.0)), CornerRadius::same(1), color);
    }
    let resp = resp.on_hover_text(if paused { t!(TIP_RESUME) } else { t!(TIP_PAUSE) });
    recording && resp.clicked()
}

pub enum RecClick {
    None,
    Start,
    Stop,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecMode {
    Idle,
    /// Seconds left before recording starts.
    Countdown(u32),
    Recording,
}

pub fn rec_button(ui: &mut Ui, mode: RecMode, enabled: bool) -> RecClick {
    rec_button_sized(ui, mode, enabled, 56.0)
}

pub fn rec_button_sized(ui: &mut Ui, mode: RecMode, enabled: bool, size: f32) -> RecClick {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let center = rect.center();
    let r = size / 2.0;
    let p = ui.painter();
    let active = mode != RecMode::Idle;
    let usable = enabled || active;
    let hovered = resp.hovered() && usable;
    let ring = match mode {
        RecMode::Countdown(_) => ORANGE,
        _ if usable => LABEL,
        _ => LABEL_3,
    };
    p.circle_stroke(center, r - 1.5, Stroke::new(3.0, ring));
    match mode {
        RecMode::Recording => {
            let side = size * 0.36;
            p.rect_filled(
                Rect::from_center_size(center, Vec2::splat(side)),
                CornerRadius::same((side / 4.0) as u8),
                if hovered { RED_HOVER } else { RED },
            );
        }
        RecMode::Countdown(left) => {
            p.text(center, Align2::CENTER_CENTER, left.max(1).to_string(), semibold(size * 0.42), ORANGE);
        }
        RecMode::Idle => {
            let disc = if !enabled {
                LABEL_3
            } else if hovered {
                RED_HOVER
            } else {
                RED
            };
            let inner = if hovered { r - 6.0 } else { r - 7.0 };
            p.circle_filled(center, inner, disc);
        }
    }
    let resp = resp.on_hover_text(match mode {
        RecMode::Recording => t!(TIP_REC_STOP),
        RecMode::Countdown(_) => t!(TIP_REC_CANCEL),
        RecMode::Idle => t!(TIP_REC_START),
    });
    if !resp.clicked() {
        RecClick::None
    } else {
        match mode {
            RecMode::Recording => RecClick::Stop,
            RecMode::Countdown(_) => RecClick::Cancel,
            RecMode::Idle if enabled => RecClick::Start,
            RecMode::Idle => RecClick::None,
        }
    }
}

// ----- status capsule -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Red,
    Orange,
    Blue,
    Green,
    Gray,
}

impl Tint {
    pub fn color(self) -> Color32 {
        match self {
            Tint::Red => RED,
            Tint::Orange => ORANGE,
            Tint::Blue => BLUE,
            Tint::Green => GREEN,
            Tint::Gray => LABEL_2,
        }
    }
}

const CAPSULE_H: f32 = 30.0;
const CAPSULE_FONT: f32 = 13.0;

/// Width a capsule needs for `sample` (used to give timers a stable width).
pub fn capsule_width_for(ui: &Ui, sample: &str) -> f32 {
    ui.painter().layout_no_wrap(sample.to_owned(), semibold(CAPSULE_FONT), LABEL).size().x + CAPSULE_H
}

/// Tinted pill with a short status text. With `min_width` the text is
/// left-aligned inside a fixed-width capsule (no jitter as digits change);
/// otherwise the capsule hugs the text. Long text is truncated; pass the full
/// text as `tip`.
pub fn status_capsule(
    ui: &mut Ui,
    tint: Tint,
    text: &str,
    min_width: Option<f32>,
    max_width: Option<f32>,
    tip: Option<&str>,
) -> Response {
    let color = tint.color();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), semibold(CAPSULE_FONT), color);
    let natural = galley.size().x + CAPSULE_H;
    let max_w = max_width.unwrap_or(f32::INFINITY).min(ui.available_width()).max(CAPSULE_H * 2.0);
    let w = min_width.map(|m| m.max(natural)).unwrap_or(natural).min(max_w);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, CAPSULE_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((CAPSULE_H / 2.0) as u8), tinted(color, 46));
    let text_rect = rect.shrink2(Vec2::new(CAPSULE_H / 2.0, 0.0));
    let pos = if min_width.is_some() || natural > w {
        text_rect.left_center() - Vec2::new(0.0, galley.size().y / 2.0)
    } else {
        rect.center() - galley.size() / 2.0
    };
    p.with_clip_rect(text_rect).galley(pos, galley, color);
    match tip {
        Some(t) => resp.on_hover_text(t),
        None => resp,
    }
}

/// Clickable tinted capsule (the "Update to vX" chip).
pub fn capsule_button(ui: &mut Ui, tint: Tint, text: &str, tip: &str) -> Response {
    let color = tint.color();
    let resp = pill_button(ui, text, tinted(color, 46), tinted(color, 70), color, semibold(12.0), 26.0);
    resp.on_hover_text(tip)
}

// ----- sidebar ---------------------------------------------------------------------

/// Sidebar entry: tinted icon square + label in a rounded pill, blue when selected.
pub fn nav_entry(ui: &mut Ui, icon: &str, tint: Color32, label: &str, selected: bool) -> Response {
    let width = ui.available_width() - 16.0;
    ui.add_space(0.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click());
    let rect = rect.translate(Vec2::new(8.0, 0.0));
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(8), BLUE);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), FILL);
    }
    let square = Rect::from_center_size(rect.left_center() + Vec2::new(10.0 + 12.0, 0.0), Vec2::splat(24.0));
    let square_fill = if selected { Color32::from_white_alpha(56) } else { tint };
    p.rect_filled(square, CornerRadius::same(6), square_fill);
    p.text(square.center(), Align2::CENTER_CENTER, icon, FontId::proportional(12.0), Color32::WHITE);
    let color = if selected { Color32::WHITE } else { LABEL };
    p.text(rect.left_center() + Vec2::new(44.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
    resp
}

/// Vertical hairline divider for horizontal strips.
pub fn vdivider(ui: &mut Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, height), Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, SEPARATOR));
}

/// Frame for modal sheets: card colour, 14 pt corners, soft shadow.
pub fn sheet_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(20))
        .shadow(egui::Shadow { offset: [0, 8], blur: 32, spread: 0, color: Color32::from_black_alpha(160) })
}
