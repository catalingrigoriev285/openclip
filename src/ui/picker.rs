//! Full-screen region picker: one undecorated, always-on-top viewport per
//! monitor showing a fresh screenshot (opaque, so no compositor transparency
//! is needed); the user drags a rectangle, Esc cancels.

use anyhow::Result;
use eframe::egui::{
    self, Color32, ColorImage, CursorIcon, Key, Pos2, Sense, Stroke, TextureHandle, TextureOptions,
    ViewportBuilder, ViewportId,
};

use crate::capture::monitors::{screenshot_monitor, MonitorInfo};
use crate::capture::Rect;
use crate::video::preview::make_preview;

pub enum PickerOutcome {
    Pending,
    Selected(u32, Rect),
    Cancelled,
}

struct MonitorView {
    info: MonitorInfo,
    image: Option<ColorImage>,
    tex: Option<TextureHandle>,
}

pub struct Picker {
    views: Vec<MonitorView>,
    /// (monitor index, drag start in that viewport's points)
    drag: Option<(usize, Pos2)>,
    current: Option<Pos2>,
    outcome: Option<PickerOutcome>,
}

impl Picker {
    /// Screenshots every monitor up front so the overlays look like the desktop.
    pub fn new(monitors: &[MonitorInfo]) -> Result<Self> {
        if monitors.is_empty() {
            anyhow::bail!("no monitors");
        }
        let mut views = Vec::new();
        for m in monitors {
            let image = screenshot_monitor(m.id).ok().map(|frame| {
                // Keep it reasonably small; it is only a backdrop.
                let p = make_preview(&frame, 2560);
                ColorImage::from_rgba_unmultiplied([p.width as usize, p.height as usize], &p.rgba)
            });
            views.push(MonitorView { info: m.clone(), image, tex: None });
        }
        Ok(Self { views, drag: None, current: None, outcome: None })
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PickerOutcome {
        for i in 0..self.views.len() {
            let info = self.views[i].info.clone();
            let scale = info.scale_factor.max(0.1);
            let id = ViewportId::from_hash_of(("openclip-picker", info.id));
            let builder = ViewportBuilder::default()
                .with_title("Select region — drag to select, Esc to cancel")
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_always_on_top()
                .with_active(true)
                .with_position(Pos2::new(info.x as f32 / scale, info.y as f32 / scale))
                .with_inner_size(egui::vec2(info.width as f32 / scale, info.height as f32 / scale));
            ctx.show_viewport_immediate(id, builder, |ui, _class| {
                self.draw_monitor(ui, i);
            });
        }
        match self.outcome.take() {
            Some(o) => o,
            None => PickerOutcome::Pending,
        }
    }

    fn draw_monitor(&mut self, ui: &mut egui::Ui, i: usize) {
        let ctx = ui.ctx().clone();
        let full = ctx.content_rect();
        let ppp = ctx.pixels_per_point();
        ctx.set_cursor_icon(CursorIcon::Crosshair);

        if self.views[i].tex.is_none()
            && let Some(img) = self.views[i].image.take()
        {
            self.views[i].tex =
                Some(ctx.load_texture(format!("picker-{i}"), img, TextureOptions::LINEAR));
        }
        let painter = ui.painter().clone();
        let uv_full = egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        match &self.views[i].tex {
            Some(tex) => {
                painter.image(tex.id(), full, uv_full, Color32::WHITE);
            }
            None => {
                painter.rect_filled(full, 0.0, Color32::from_gray(40));
            }
        }
        painter.rect_filled(full, 0.0, Color32::from_black_alpha(110));

        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        if resp.drag_started() {
            // `drag_started` fires once the pointer has moved past the drag
            // threshold; anchor on the original press position instead.
            let origin = ctx.input(|inp| inp.pointer.press_origin());
            if let Some(p) = origin.or_else(|| resp.interact_pointer_pos()) {
                self.drag = Some((i, p));
                self.current = resp.interact_pointer_pos().or(Some(p));
            }
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
            && matches!(self.drag, Some((m, _)) if m == i)
        {
            self.current = Some(p);
        }

        if let (Some((m, start)), Some(cur)) = (self.drag, self.current) {
            if m == i {
                let sel = egui::Rect::from_two_pos(start, cur).intersect(full);
                if let Some(tex) = &self.views[i].tex {
                    let uv = egui::Rect::from_min_max(
                        Pos2::new(sel.min.x / full.width(), sel.min.y / full.height()),
                        Pos2::new(sel.max.x / full.width(), sel.max.y / full.height()),
                    );
                    painter.image(tex.id(), sel, uv, Color32::WHITE);
                }
                painter.rect_stroke(sel, 0.0, Stroke::new(2.0, Color32::from_rgb(255, 80, 80)), egui::StrokeKind::Outside);
                let phys = to_physical(sel, full, ppp, &self.views[i].info);
                let label = format!("{}×{}", phys.width, phys.height);
                let pos = Pos2::new(sel.min.x, (sel.min.y - 22.0).max(full.min.y));
                painter.rect_filled(
                    egui::Rect::from_min_size(pos, egui::vec2(label.len() as f32 * 8.0 + 12.0, 20.0)),
                    3.0,
                    Color32::from_black_alpha(200),
                );
                painter.text(
                    pos + egui::vec2(6.0, 2.0),
                    egui::Align2::LEFT_TOP,
                    label,
                    egui::FontId::monospace(14.0),
                    Color32::WHITE,
                );
                if resp.drag_stopped() {
                    if phys.width >= 16 && phys.height >= 16 {
                        self.outcome = Some(PickerOutcome::Selected(self.views[i].info.id, phys));
                    } else {
                        self.drag = None;
                        self.current = None;
                    }
                }
            }
        } else {
            let hint = "Drag to select a region · Esc to cancel";
            painter.text(
                full.center(),
                egui::Align2::CENTER_CENTER,
                hint,
                egui::FontId::proportional(22.0),
                Color32::from_white_alpha(220),
            );
        }

        let esc = ctx.input(|inp| inp.key_pressed(Key::Escape) || inp.viewport().close_requested());
        if esc {
            self.outcome = Some(PickerOutcome::Cancelled);
        }
        ctx.request_repaint();
    }
}

/// Converts a selection in viewport points to monitor-local physical pixels
/// (even-sized, clamped to the monitor).
fn to_physical(sel: egui::Rect, full: egui::Rect, ppp: f32, info: &MonitorInfo) -> Rect {
    let sx = info.width as f32 / (full.width() * ppp).max(1.0) * ppp;
    let sy = info.height as f32 / (full.height() * ppp).max(1.0) * ppp;
    let x0 = ((sel.min.x - full.min.x) * sx).round().max(0.0) as u32;
    let y0 = ((sel.min.y - full.min.y) * sy).round().max(0.0) as u32;
    let x1 = ((sel.max.x - full.min.x) * sx).round().max(0.0) as u32;
    let y1 = ((sel.max.y - full.min.y) * sy).round().max(0.0) as u32;
    let x0 = x0.min(info.width.saturating_sub(2));
    let y0 = y0.min(info.height.saturating_sub(2));
    let w = (x1.min(info.width).saturating_sub(x0)) & !1;
    let h = (y1.min(info.height).saturating_sub(y0)) & !1;
    Rect { x: x0, y: y0, width: w, height: h }
}
