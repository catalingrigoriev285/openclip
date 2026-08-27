//! Persistent on-screen border around the selected recording region, shown
//! while the app is collapsed to the mini bar (like Camtasia / ShareX).
//!
//! Child viewports cannot be transparent with the wgpu backend on Windows
//! (the DX12 swapchain only offers an opaque alpha mode), so the frame is
//! built from four thin, opaque, click-through strip windows placed just
//! *outside* the region. Region capture crops to the exact rect, so the
//! strips are never part of the recording.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Vec2, ViewportBuilder, ViewportCommand, ViewportId};

use super::theme::{ACCENT, REC_RED, WARN_YELLOW};
use super::{App, SourceKind, State};
use crate::capture::monitors::MonitorInfo;
use crate::capture::Rect;

/// Stroke thickness in points (scaled to physical pixels per monitor).
pub(super) const THICKNESS_PT: f32 = 3.0;
/// Physical-pixel gap between the region edge and the stroke, so DPI rounding
/// can never push a strip into the captured rect.
pub(super) const GAP_PX: i32 = 2;
/// Let mouse input fall through the strips to whatever is underneath.
const PASSTHROUGH: bool = true;

#[derive(Clone, Copy)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

/// Rectangle in global physical pixels.
#[derive(Clone, Copy)]
struct PhysRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl App {
    /// The region's monitor and rect, or `None` if no region is selected or its
    /// monitor disappeared after a refresh.
    pub(super) fn region_monitor(&self) -> Option<(&MonitorInfo, Rect)> {
        let (id, rect) = self.region?;
        self.monitors.iter().find(|m| m.id == id).map(|m| (m, rect))
    }

    /// The frame is only shown in mini-bar mode, for a region source, and never
    /// while the picker overlay is open.
    fn region_frame_visible(&self) -> bool {
        self.compact
            && self.source_kind == SourceKind::Region
            && !matches!(self.state, State::Picking(_))
            && self.region_monitor().is_some()
    }

    fn frame_color(&self) -> Color32 {
        match &self.state {
            State::Recording(r) if r.is_paused() => WARN_YELLOW,
            State::Recording(_) => REC_RED,
            _ => ACCENT,
        }
    }

    /// Keeps the region docked to the mini bar: when the user drags the bar,
    /// the region (and its frame) move by the same amount, clamped to the
    /// region's monitor. Ignored while recording (the captured rect is fixed).
    pub(super) fn follow_bar(&mut self, ctx: &egui::Context) {
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else { return };
        // Right after we repositioned the bar ourselves, just track where the
        // OS actually put it; keep repainting so the resync happens even when
        // nothing else triggers a frame.
        if let Some(until) = self.bar_settle_until {
            if Instant::now() < until {
                self.bar_anchor = Some(outer.min);
                ctx.request_repaint_after(Duration::from_millis(50));
                return;
            }
            self.bar_settle_until = None;
        }
        let Some(anchor) = self.bar_anchor else {
            self.bar_anchor = Some(outer.min);
            return;
        };
        if outer.min == anchor {
            return;
        }
        self.bar_anchor = Some(outer.min);
        if self.is_recording() || self.source_kind != SourceKind::Region {
            return;
        }
        let delta = (outer.min - anchor) * ctx.pixels_per_point();
        let Some((m, r)) = self.region_monitor() else { return };
        let max_x = m.width.saturating_sub(r.width) as i32;
        let max_y = m.height.saturating_sub(r.height) as i32;
        let x = (r.x as i32 + delta.x.round() as i32).clamp(0, max_x) as u32;
        let y = (r.y as i32 + delta.y.round() as i32).clamp(0, max_y) as u32;
        if let Some((_, rect)) = &mut self.region {
            rect.x = x;
            rect.y = y;
        }
    }

    /// Shows (or, by not showing, closes) the four strip viewports.
    pub(super) fn region_frame(&mut self, ctx: &egui::Context) {
        if !self.region_frame_visible() {
            self.frame_styled = false;
            return;
        }
        let Some((m, r)) = self.region_monitor() else { return };
        let scale = m.scale_factor.max(0.1);
        let strips = strips(m, r);
        let color = self.frame_color();

        for (side, p) in strips {
            let id = ViewportId::from_hash_of(("openclip-region-frame", side as u8));
            let builder = ViewportBuilder::default()
                .with_title(strip_title(side))
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_always_on_top()
                .with_active(false)
                .with_mouse_passthrough(PASSTHROUGH)
                .with_position(Pos2::new(p.x as f32 / scale, p.y as f32 / scale))
                .with_inner_size(Vec2::new(p.w as f32 / scale, p.h as f32 / scale));
            ctx.show_viewport_immediate(id, builder, move |ui, _class| {
                let vctx = ui.ctx();
                ui.painter().rect_filled(vctx.content_rect(), 0.0, color);
                correct_placement(vctx, id, p);
            });
        }

        // Windows 11 rounds the corners of every top-level window and draws a
        // 1 px border around it, which turns thin strips into grey pills.
        // Square them off once the windows exist (retry until all four do).
        if !self.frame_styled {
            self.frame_styled = [Side::Top, Side::Bottom, Side::Left, Side::Right]
                .iter()
                .all(|&side| square_off(&strip_title(side)));
            if !self.frame_styled {
                ctx.request_repaint();
            }
        }
    }
}

fn strip_title(side: Side) -> String {
    format!("openclip-region-frame-{}", side as u8)
}

/// Disables DWM rounded corners and the window border for the top-level window
/// with the given title. Returns false if no such window exists (yet).
#[cfg(windows)]
pub fn square_off(title: &str) -> bool {
    use std::ffi::c_void;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    let name = HSTRING::from(title);
    // SAFETY: plain Win32 calls with valid, live pointers; failures are ignored.
    unsafe {
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) else { return false };
        if hwnd.is_invalid() {
            return false;
        }
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of_val(&corner) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &DWMWA_COLOR_NONE as *const _ as *const c_void,
            size_of_val(&DWMWA_COLOR_NONE) as u32,
        );
    }
    true
}

#[cfg(not(windows))]
pub fn square_off(_title: &str) -> bool {
    true
}

/// The four strips around the region, in global physical pixels.
fn strips(m: &MonitorInfo, r: Rect) -> [(Side, PhysRect); 4] {
    let scale = m.scale_factor.max(0.1);
    let t = ((THICKNESS_PT * scale).round() as i32).max(1);
    let (rx, ry) = (m.x + r.x as i32, m.y + r.y as i32);
    let (rw, rh) = (r.width as i32, r.height as i32);
    let (ox, oy) = (rx - GAP_PX - t, ry - GAP_PX - t);
    let (ow, oh) = (rw + 2 * (GAP_PX + t), rh + 2 * (GAP_PX + t));
    [
        (Side::Top, PhysRect { x: ox, y: oy, w: ow, h: t }),
        (Side::Bottom, PhysRect { x: ox, y: oy + oh - t, w: ow, h: t }),
        (Side::Left, PhysRect { x: ox, y: ry - GAP_PX, w: t, h: rh + 2 * GAP_PX }),
        (Side::Right, PhysRect { x: ox + ow - t, y: ry - GAP_PX, w: t, h: rh + 2 * GAP_PX }),
    ]
}

/// Mixed-DPI self-correction: the builder position is interpreted with the
/// primary monitor's scale, so on another monitor the strip can land off by the
/// DPI ratio. Compare the strip's real placement (in its own points) with the
/// wanted physical rect and nudge it; a no-op on single-DPI setups.
fn correct_placement(ctx: &egui::Context, id: ViewportId, p: PhysRect) {
    let ppp = ctx.pixels_per_point();
    let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else { return };
    let want_min = Pos2::new(p.x as f32 / ppp, p.y as f32 / ppp);
    let want_size = Vec2::new(p.w as f32 / ppp, p.h as f32 / ppp);
    if ((outer.min - want_min) * ppp).abs().max_elem() > 1.0 {
        ctx.send_viewport_cmd_to(id, ViewportCommand::OuterPosition(want_min));
    }
    if ((outer.size() - want_size) * ppp).abs().max_elem() > 1.0 {
        ctx.send_viewport_cmd_to(id, ViewportCommand::InnerSize(want_size));
    }
}
