//! Persistent on-screen border around the selected recording region, a thin
//! yellow-green rectangle with eight white drag handles and a crosshair in the
//! middle. Dragging the border moves the region, dragging a handle resizes it;
//! while recording only the move is offered, because the encoder's frame size
//! is fixed at the first frame.
//!
//! Child viewports cannot be transparent with the wgpu backend on Windows (the
//! DX12 swapchain only offers an opaque alpha mode), so the frame is built from
//! four strip windows placed just *outside* the region and each strip *is* the
//! line: filled edge to edge with the border colour, so an opaque window has no
//! matte band to give away. Region capture crops to the exact rect, so the
//! strips are never part of the recording; the crosshair sits *inside* the
//! region and relies on `WDA_EXCLUDEFROMCAPTURE` instead.

use device_query::{DeviceQuery, DeviceState};
use eframe::egui::{
    self, Color32, CursorIcon, Pos2, Stroke, Vec2, ViewportBuilder, ViewportCommand, ViewportId,
};

use super::theme::{BG, ORANGE, RED, REGION};
use super::{App, SourceKind, State};
use crate::capture::monitors::MonitorInfo;
use crate::capture::Rect;

/// Thickness of the border line around the region, in points (scaled to
/// physical pixels per monitor). The strip window *is* the line, so this is
/// both how thick the border looks and how wide the grab area is.
pub(super) const BAND_PT: f32 = 5.0;
/// Physical-pixel gap between the region edge and the line, so DPI rounding
/// can never push a strip into the captured rect.
pub(super) const GAP_PX: i32 = 2;
/// Length of a drag handle along its strip, in points; across it the handle
/// spans the full line thickness.
const HANDLE_PT: f32 = 16.0;
/// Colour of the drag handles, against the coloured line.
const HANDLE_COLOR: Color32 = Color32::WHITE;
/// Side of the centre crosshair window, in points.
const CROSS_PT: f32 = 22.0;
/// Smallest region a resize may produce; matches the picker's threshold, and is
/// even so the encoder-friendly rounding below never has to shrink past it.
const MIN_REGION_PX: i32 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Top,
    Bottom,
    Left,
    Right,
    /// The crosshair inside the region; a move target.
    Centre,
}

impl Part {
    /// Strips whose long axis is horizontal carry the corner and N/S handles.
    fn horizontal(self) -> bool {
        matches!(self, Part::Top | Part::Bottom)
    }
}

/// Rectangle in global physical pixels.
#[derive(Clone, Copy)]
struct PhysRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// The bits of [`MonitorInfo`] the geometry needs, copied out so the borrow on
/// `App` ends before the frame is drawn.
#[derive(Clone, Copy)]
struct Mon {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f32,
}

/// What a press on the border does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Grab {
    Move,
    Resize { left: bool, right: bool, top: bool, bottom: bool },
}

/// A border drag in progress. Tracked against the pointer's *global* position
/// rather than per-frame deltas, because the strip windows move and resize
/// under the cursor while the drag runs.
pub(super) struct FrameDrag {
    grab: Grab,
    /// Pointer position when the drag started, in global physical pixels.
    start_pointer: (i32, i32),
    /// Region rect when the drag started, in monitor-local physical pixels.
    start_rect: Rect,
}

/// Global pointer position and primary-button state, read straight from the OS.
/// `device_query` is already a dependency (see [`crate::video::mouse_fx`]) and
/// reports the same global physical pixels the region geometry uses.
pub(super) struct GlobalPointer(DeviceState);

impl GlobalPointer {
    fn read(&self) -> ((i32, i32), bool) {
        let m = self.0.get_mouse();
        // `device_query` indexes buttons from 1; 1 is the primary button.
        (m.coords, m.button_pressed.get(1).copied().unwrap_or(false))
    }
}

impl Default for GlobalPointer {
    fn default() -> Self {
        Self(DeviceState::new())
    }
}

impl App {
    /// The region's monitor and rect, or `None` if no region is selected or its
    /// monitor disappeared after a refresh.
    pub(super) fn region_monitor(&self) -> Option<(&MonitorInfo, Rect)> {
        let (id, rect) = self.region?;
        self.monitors.iter().find(|m| m.id == id).map(|m| (m, rect))
    }

    /// The frame belongs to the mini bar: it is shown while compact for a region
    /// source, and closing the bar hides it along with the bar. Never while the
    /// picker overlay is open.
    fn region_frame_visible(&self) -> bool {
        self.compact
            && self.source_kind == SourceKind::Region
            && !matches!(self.state, State::Picking(_))
            && self.region_monitor().is_some()
    }

    fn frame_color(&self) -> Color32 {
        match &self.state {
            State::Recording(r) if r.is_paused() => ORANGE,
            State::Recording(_) => RED,
            _ => REGION,
        }
    }

    /// Stores a new region rect and, while recording, moves the live capture
    /// crop with it. Only the origin reaches the recorder: the encoder and the
    /// container header are fixed to the first frame's size.
    pub(super) fn apply_region(&mut self, rect: Rect) {
        match &mut self.region {
            Some((_, r)) if *r == rect => return,
            Some((_, r)) => *r = rect,
            None => return,
        }
        if let State::Recording(rec) = &self.state {
            rec.set_region(rect);
        }
    }

    /// Shows (or, by not showing, closes) the border viewports and drives the
    /// drag they start.
    pub(super) fn region_frame(&mut self, ctx: &egui::Context) {
        if !self.region_frame_visible() {
            self.frame_styled = false;
            self.frame_excluded = false;
            self.frame_parts = 0;
            self.frame_drag = None;
            return;
        }
        let Some((info, rect)) = self.region_monitor() else { return };
        let mon = Mon {
            x: info.x,
            y: info.y,
            width: info.width,
            height: info.height,
            scale: info.scale_factor.max(0.1),
        };
        let color = self.frame_color();
        // Resizing changes the frame size, which the encoder cannot follow, so
        // only offer the handles when no recording is under way.
        let handles = matches!(self.state, State::Idle);
        // The crosshair sits inside the captured rect. It is only safe while
        // recording if Windows agreed to hide it from screen capture.
        let crosshair = handles || self.frame_excluded;
        let parts = parts(mon, rect, crosshair);
        // Once a drag is running the cursor may be anywhere; keep showing its
        // shape rather than whatever the strip under the pointer would pick.
        let active = self.frame_drag.as_ref().map(|d| d.grab);

        let mut pressed = None;
        for (part, p) in &parts {
            let (part, p) = (*part, *p);
            let id = ViewportId::from_hash_of(("openclip-region-frame", part as u8));
            let builder = ViewportBuilder::default()
                .with_title(part_title(part))
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_always_on_top()
                .with_active(false)
                .with_mouse_passthrough(false)
                .with_position(Pos2::new(p.x as f32 / mon.scale, p.y as f32 / mon.scale))
                .with_inner_size(Vec2::new(p.w as f32 / mon.scale, p.h as f32 / mon.scale));
            let hit = ctx.show_viewport_immediate(id, builder, move |ui, _class| {
                let vctx = ui.ctx().clone();
                let area = vctx.content_rect();
                paint(ui, part, area, color, handles);
                correct_placement(&vctx, id, p);
                let over = vctx
                    .input(|i| i.pointer.latest_pos())
                    .filter(|q| area.contains(*q))
                    .map(|q| grab_at(part, area, q, handles));
                if let Some(g) = active.or(over) {
                    vctx.set_cursor_icon(cursor_for(g));
                }
                over.filter(|_| vctx.input(|i| i.pointer.primary_pressed()))
            });
            pressed = pressed.or(hit);
        }

        self.drive_drag(ctx, pressed, rect, (mon.width, mon.height));

        // Windows 11 rounds the corners of every top-level window and draws a
        // 1 px border around it, which turns thin strips into grey pills. Square
        // them off (and hide them from screen capture) once they exist.
        // Restyle whenever the set changes: the crosshair comes and goes with the
        // region's size, and a window that appears later needs the same treatment.
        if self.frame_parts != parts.len() {
            self.frame_parts = parts.len();
            self.frame_styled = false;
        }
        if !self.frame_styled {
            let styled: Vec<_> = parts.iter().map(|(part, _)| style_overlay(&part_title(*part))).collect();
            self.frame_styled = styled.iter().all(Option::is_some);
            self.frame_excluded = self.frame_styled && styled.iter().all(|s| *s == Some(true));
            if !self.frame_styled {
                ctx.request_repaint();
            }
        }
    }

    /// Starts, advances and ends a border drag. Position and button state come
    /// from the OS, so the drag is unaffected by the strips moving away from
    /// under the pointer mid-drag.
    fn drive_drag(&mut self, ctx: &egui::Context, pressed: Option<Grab>, rect: Rect, mon: (u32, u32)) {
        let pointer = self.frame_pointer.get_or_insert_with(GlobalPointer::default);
        let (pos, down) = pointer.read();
        if let Some(grab) = pressed
            && self.frame_drag.is_none()
            && down
        {
            self.frame_drag = Some(FrameDrag { grab, start_pointer: pos, start_rect: rect });
        }
        let Some(drag) = &self.frame_drag else { return };
        if !down {
            self.frame_drag = None;
            return;
        }
        let delta = (pos.0 - drag.start_pointer.0, pos.1 - drag.start_pointer.1);
        self.apply_region(dragged_rect(drag.start_rect, drag.grab, delta, mon));
        // The strips only repaint when something asks them to; a drag has to.
        ctx.request_repaint();
    }
}

fn part_title(part: Part) -> String {
    format!("openclip-region-frame-{}", part as u8)
}

// ----- painting ----------------------------------------------------------------

fn paint(ui: &mut egui::Ui, part: Part, area: egui::Rect, color: Color32, handles: bool) {
    let p = ui.painter();
    if part == Part::Centre {
        // The one solid shape of the frame; its arms are cut out in the app's
        // background colour so it reads as a target rather than a blob.
        let c = area.center();
        let arm = area.width() * 0.30;
        let gap = area.width() * 0.13;
        let stroke = Stroke::new(2.0, BG);
        p.rect_filled(area, 0.0, color);
        p.line_segment([Pos2::new(c.x - gap - arm, c.y), Pos2::new(c.x - gap, c.y)], stroke);
        p.line_segment([Pos2::new(c.x + gap, c.y), Pos2::new(c.x + gap + arm, c.y)], stroke);
        p.line_segment([Pos2::new(c.x, c.y - gap - arm), Pos2::new(c.x, c.y - gap)], stroke);
        p.line_segment([Pos2::new(c.x, c.y + gap), Pos2::new(c.x, c.y + gap + arm)], stroke);
        return;
    }
    // The strip is the line: filled edge to edge, so the opaque window shows
    // nothing but the border itself.
    p.rect_filled(area, 0.0, color);
    if handles {
        let size = handle_size(part, area);
        for c in handle_centres(part, area) {
            p.rect_filled(egui::Rect::from_center_size(c, size), 0.0, HANDLE_COLOR);
        }
    }
}

/// Thickness of the band across its short axis, in the viewport's own points.
fn band_thickness(part: Part, area: egui::Rect) -> f32 {
    if part.horizontal() { area.height() } else { area.width() }
}

/// Length of a handle along its strip, in the viewport's own points.
fn handle_len(part: Part, area: egui::Rect) -> f32 {
    band_thickness(part, area) * (HANDLE_PT / BAND_PT)
}

/// A handle spans the line across and [`HANDLE_PT`] along it.
fn handle_size(part: Part, area: egui::Rect) -> Vec2 {
    let (t, len) = (band_thickness(part, area), handle_len(part, area));
    if part.horizontal() { Vec2::new(len, t) } else { Vec2::new(t, len) }
}

/// Centres of the handles this strip owns. The horizontal strips span the full
/// outer width, so they carry the corners as well as the N/S midpoints.
fn handle_centres(part: Part, area: egui::Rect) -> Vec<Pos2> {
    let len = handle_len(part, area);
    if part.horizontal() {
        let y = area.center().y;
        vec![
            Pos2::new(area.left() + len / 2.0, y),
            Pos2::new(area.center().x, y),
            Pos2::new(area.right() - len / 2.0, y),
        ]
    } else {
        vec![Pos2::new(area.center().x, area.center().y)]
    }
}

// ----- interaction -------------------------------------------------------------

/// Which grab a point on a strip maps to. Everything that is not a handle moves
/// the whole region, so the band is always actionable.
fn grab_at(part: Part, area: egui::Rect, q: Pos2, handles: bool) -> Grab {
    if !handles || part == Part::Centre {
        return Grab::Move;
    }
    // Exactly the drawn handle, which is already a generous target.
    let reach = (handle_len(part, area) / 2.0).max(4.0);
    if part.horizontal() {
        let (top, bottom) = (part == Part::Top, part == Part::Bottom);
        let near = |x: f32| (q.x - x).abs() <= reach;
        if near(area.left() + reach) {
            return Grab::Resize { left: true, right: false, top, bottom };
        }
        if near(area.right() - reach) {
            return Grab::Resize { left: false, right: true, top, bottom };
        }
        if near(area.center().x) {
            return Grab::Resize { left: false, right: false, top, bottom };
        }
    } else if (q.y - area.center().y).abs() <= reach {
        return Grab::Resize { left: part == Part::Left, right: part == Part::Right, top: false, bottom: false };
    }
    Grab::Move
}

fn cursor_for(grab: Grab) -> CursorIcon {
    match grab {
        Grab::Move => CursorIcon::Move,
        Grab::Resize { left, right, top, bottom } => match (left || right, top || bottom) {
            // `left == top` picks out the NW/SE diagonal from the NE/SW one.
            (true, true) => {
                if left == top {
                    CursorIcon::ResizeNwSe
                } else {
                    CursorIcon::ResizeNeSw
                }
            }
            (true, false) => CursorIcon::ResizeHorizontal,
            (false, true) => CursorIcon::ResizeVertical,
            (false, false) => CursorIcon::Move,
        },
    }
}

fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi.max(lo))
}

/// Applies a pointer delta (global physical pixels) to the rect the drag started
/// from. The result stays on the monitor, keeps even dimensions for the encoders
/// and never shrinks below [`MIN_REGION_PX`].
fn dragged_rect(start: Rect, grab: Grab, delta: (i32, i32), mon: (u32, u32)) -> Rect {
    let (mw, mh) = (mon.0 as i32, mon.1 as i32);
    let (w, h) = (start.width as i32, start.height as i32);
    let Grab::Resize { left, right, top, bottom } = grab else {
        return Rect {
            x: clamp(start.x as i32 + delta.0, 0, mw - w) as u32,
            y: clamp(start.y as i32 + delta.1, 0, mh - h) as u32,
            ..start
        };
    };
    let (mut x0, mut y0) = (start.x as i32, start.y as i32);
    let (mut x1, mut y1) = (x0 + w, y0 + h);
    if left {
        x0 = clamp(x0 + delta.0, 0, x1 - MIN_REGION_PX);
    }
    if right {
        x1 = clamp(x1 + delta.0, x0 + MIN_REGION_PX, mw);
    }
    if top {
        y0 = clamp(y0 + delta.1, 0, y1 - MIN_REGION_PX);
    }
    if bottom {
        y1 = clamp(y1 + delta.1, y0 + MIN_REGION_PX, mh);
    }
    // Even dimensions, corrected on the edge being dragged so the fixed edge
    // stays put. Growing is preferred; at the screen edge we shrink instead.
    if (x1 - x0) % 2 != 0 {
        if left {
            if x0 > 0 { x0 -= 1 } else { x1 -= 1 }
        } else if x1 < mw {
            x1 += 1
        } else {
            x0 += 1
        }
    }
    if (y1 - y0) % 2 != 0 {
        if top {
            if y0 > 0 { y0 -= 1 } else { y1 -= 1 }
        } else if y1 < mh {
            y1 += 1
        } else {
            y0 += 1
        }
    }
    Rect { x: x0 as u32, y: y0 as u32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 }
}

// ----- geometry ----------------------------------------------------------------

/// The four strips around the region plus, when it fits, the centre crosshair —
/// all in global physical pixels.
fn parts(m: Mon, r: Rect, crosshair: bool) -> Vec<(Part, PhysRect)> {
    let t = ((BAND_PT * m.scale).round() as i32).max(1);
    let (rx, ry) = (m.x + r.x as i32, m.y + r.y as i32);
    let (rw, rh) = (r.width as i32, r.height as i32);
    let (ox, oy) = (rx - GAP_PX - t, ry - GAP_PX - t);
    let (ow, oh) = (rw + 2 * (GAP_PX + t), rh + 2 * (GAP_PX + t));
    let mut out = vec![
        (Part::Top, PhysRect { x: ox, y: oy, w: ow, h: t }),
        (Part::Bottom, PhysRect { x: ox, y: oy + oh - t, w: ow, h: t }),
        (Part::Left, PhysRect { x: ox, y: ry - GAP_PX, w: t, h: rh + 2 * GAP_PX }),
        (Part::Right, PhysRect { x: ox + ow - t, y: ry - GAP_PX, w: t, h: rh + 2 * GAP_PX }),
    ];
    let c = ((CROSS_PT * m.scale).round() as i32).max(8);
    // Skip it on a region small enough for the crosshair to dominate.
    if crosshair && rw >= c * 3 && rh >= c * 3 {
        out.push((Part::Centre, PhysRect { x: rx + (rw - c) / 2, y: ry + (rh - c) / 2, w: c, h: c }));
    }
    out
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

// ----- platform styling ---------------------------------------------------------

/// Squares off the DWM rounded corners/border of the overlay window with the
/// given title, drops the drop shadow it would otherwise cast, and asks Windows
/// to keep it out of screen captures. Returns `None` if no such window exists
/// (yet), else whether the capture exclusion took effect.
#[cfg(windows)]
pub fn style_overlay(title: &str) -> Option<bool> {
    use std::ffi::c_void;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowDisplayAffinity, SetWindowLongPtrW, FindWindowW, GWL_EXSTYLE,
        WDA_EXCLUDEFROMCAPTURE, WS_EX_NOACTIVATE,
    };

    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    let name = HSTRING::from(title);
    // SAFETY: plain Win32 calls with valid, live pointers; failures are ignored.
    unsafe {
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) else { return None };
        if hwnd.is_invalid() {
            return None;
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
        // winit keeps `WS_CAPTION` on undecorated windows (it cuts the frame
        // away in `WM_NCCALCSIZE`), so DWM still draws a drop shadow around
        // each strip — a grey smudge along the region edge, and four of them
        // overlapping at the corners. Turning off non-client rendering removes
        // it; the strips paint their whole client area themselves.
        let policy = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const _ as *const c_void,
            size_of_val(&policy) as u32,
        );
        // Grabbing the border must not pull focus away from whatever is being
        // recorded; mouse messages still arrive.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
        // Windows 10 2004+: the window stays on screen but is not captured, so
        // the border can never bleed into the recording.
        Some(SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok())
    }
}

#[cfg(not(windows))]
pub fn style_overlay(_title: &str) -> Option<bool> {
    // No equivalent of WDA_EXCLUDEFROMCAPTURE, so the crosshair stays hidden
    // while recording.
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: (u32, u32) = (1920, 1080);

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    fn resize(left: bool, right: bool, top: bool, bottom: bool) -> Grab {
        Grab::Resize { left, right, top, bottom }
    }

    #[test]
    fn move_keeps_size_and_stays_on_the_monitor() {
        let start = rect(100, 100, 640, 480);
        assert_eq!(dragged_rect(start, Grab::Move, (30, -40), MON), rect(130, 60, 640, 480));
        // Off the top-left corner.
        assert_eq!(dragged_rect(start, Grab::Move, (-500, -500), MON), rect(0, 0, 640, 480));
        // Off the bottom-right corner: the far edge stops at the monitor edge.
        assert_eq!(dragged_rect(start, Grab::Move, (5000, 5000), MON), rect(1280, 600, 640, 480));
    }

    #[test]
    fn resize_moves_only_the_dragged_edges() {
        let start = rect(100, 100, 640, 480);
        // Bottom-right corner grows both dimensions, origin untouched.
        assert_eq!(dragged_rect(start, resize(false, true, false, true), (60, 20), MON), rect(100, 100, 700, 500));
        // Top-left corner moves the origin and shrinks by the same amount.
        assert_eq!(dragged_rect(start, resize(true, false, true, false), (40, 40), MON), rect(140, 140, 600, 440));
        // A side handle leaves the other axis alone.
        assert_eq!(dragged_rect(start, resize(false, true, false, false), (-100, 999), MON), rect(100, 100, 540, 480));
    }

    #[test]
    fn resize_keeps_even_dimensions() {
        let start = rect(100, 100, 640, 480);
        for d in [1, 3, 7, -5, -11] {
            let r = dragged_rect(start, resize(false, true, false, true), (d, d), MON);
            assert_eq!(r.width % 2, 0, "width for delta {d}");
            assert_eq!(r.height % 2, 0, "height for delta {d}");
            // Only the dragged edges move.
            assert_eq!((r.x, r.y), (100, 100));
            let r = dragged_rect(start, resize(true, false, true, false), (d, d), MON);
            assert_eq!(r.width % 2, 0, "width for delta {d} (origin edge)");
            assert_eq!(r.height % 2, 0, "height for delta {d} (origin edge)");
            assert_eq!((r.x + r.width, r.y + r.height), (740, 580));
        }
    }

    #[test]
    fn resize_respects_the_minimum_and_the_monitor() {
        let start = rect(100, 100, 640, 480);
        // Dragging an edge past its opposite stops at the minimum.
        let r = dragged_rect(start, resize(false, true, false, true), (-5000, -5000), MON);
        assert_eq!(r, rect(100, 100, MIN_REGION_PX as u32, MIN_REGION_PX as u32));
        let r = dragged_rect(start, resize(true, false, true, false), (5000, 5000), MON);
        assert_eq!(r, rect(724, 564, MIN_REGION_PX as u32, MIN_REGION_PX as u32));
        // Growing past the screen stops at the screen.
        let r = dragged_rect(start, resize(false, true, false, true), (5000, 5000), MON);
        assert_eq!(r, rect(100, 100, 1820, 980));
        let r = dragged_rect(start, resize(true, false, true, false), (-5000, -5000), MON);
        assert_eq!(r, rect(0, 0, 740, 580));
    }

    #[test]
    fn grab_at_maps_handles_then_falls_back_to_move() {
        // A 900-point-wide top strip, one line thick.
        let mid = BAND_PT / 2.0;
        let area = egui::Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(900.0, BAND_PT));
        let g = |x: f32| grab_at(Part::Top, area, Pos2::new(x, mid), true);
        assert_eq!(g(0.0), resize(true, false, true, false));
        assert_eq!(g(900.0), resize(false, true, true, false));
        assert_eq!(g(450.0), resize(false, false, true, false));
        assert_eq!(g(200.0), Grab::Move);
        // Handles off (recording): the whole line moves.
        assert_eq!(grab_at(Part::Top, area, Pos2::new(0.0, mid), false), Grab::Move);
        // The vertical strips carry a single midpoint handle.
        let side = egui::Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(BAND_PT, 600.0));
        assert_eq!(grab_at(Part::Left, side, Pos2::new(mid, 300.0), true), resize(true, false, false, false));
        assert_eq!(grab_at(Part::Left, side, Pos2::new(mid, 100.0), true), Grab::Move);
        assert_eq!(grab_at(Part::Centre, side, Pos2::new(mid, 300.0), true), Grab::Move);
    }

    #[test]
    fn cursor_follows_the_dragged_corner() {
        assert_eq!(cursor_for(resize(true, false, true, false)), CursorIcon::ResizeNwSe);
        assert_eq!(cursor_for(resize(false, true, false, true)), CursorIcon::ResizeNwSe);
        assert_eq!(cursor_for(resize(false, true, true, false)), CursorIcon::ResizeNeSw);
        assert_eq!(cursor_for(resize(true, false, false, true)), CursorIcon::ResizeNeSw);
        assert_eq!(cursor_for(resize(true, false, false, false)), CursorIcon::ResizeHorizontal);
        assert_eq!(cursor_for(resize(false, false, true, false)), CursorIcon::ResizeVertical);
        assert_eq!(cursor_for(Grab::Move), CursorIcon::Move);
    }

    #[test]
    fn strips_stay_outside_the_region_and_the_crosshair_inside() {
        let m = Mon { x: 0, y: 0, width: 1920, height: 1080, scale: 1.0 };
        let r = rect(200, 150, 640, 480);
        let laid_out = parts(m, r, true);
        assert_eq!(laid_out.len(), 5);
        let t = BAND_PT as i32;
        for (part, p) in &laid_out {
            if *part == Part::Centre {
                // Inside the captured rect (hidden from capture by the OS).
                assert!(p.x >= 200 && p.y >= 150);
                assert!(p.x + p.w <= 840 && p.y + p.h <= 630);
                continue;
            }
            // Never overlaps the captured rect, on any side.
            let outside = p.x + p.w <= 200 - GAP_PX
                || p.x >= 840 + GAP_PX
                || p.y + p.h <= 150 - GAP_PX
                || p.y >= 630 + GAP_PX;
            assert!(outside, "strip overlaps the region");
        }
        assert_eq!(laid_out[0].1.w, 640 + 2 * (GAP_PX + t));
        // A region too small for the crosshair simply does not get one.
        assert_eq!(parts(m, rect(0, 0, 40, 40), true).len(), 4);
        assert_eq!(parts(m, r, false).len(), 4);
    }
}
