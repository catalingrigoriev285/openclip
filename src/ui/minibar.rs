//! Compact floating recorder bar (Camtasia-style): recording area, recorded
//! inputs and pause / rec. Closing the bar window (title-bar X) does not quit
//! the app — it restores the full window instead.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, CornerRadius, FontId, Layout, RichText, Sense, Stroke, Vec2, ViewportCommand, WindowLevel};

use super::icons;
use super::region_frame::{GAP_PX, THICKNESS_PT};
use super::theme::*;
use super::{format_duration, human_bytes, icon_button, pause_button, rec_button, App, RecClick, SourceKind, State};

/// Inner size of the bar window, in points.
pub const BAR_SIZE: Vec2 = Vec2::new(800.0, 104.0);
const GROUP_H: f32 = 86.0;
const TOAST_TTL: Duration = Duration::from_secs(5);
/// How long after one of our own position commands the bar is left to settle
/// before user drags start moving the docked region.
const BAR_SETTLE: Duration = Duration::from_millis(400);

impl App {
    /// Collapses the main window into the floating bar (always on top, bottom-right).
    pub(super) fn enter_compact(&mut self, ctx: &egui::Context) {
        let (outer, monitor) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
        self.saved_rect = outer;
        self.live.stop();
        self.compact = true;
        self.bar_moved_by_us();
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(400.0, 80.0)));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(BAR_SIZE));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        if let Some(mon) = monitor {
            // Bottom-right corner, above the taskbar.
            let pos = egui::pos2((mon.x - BAR_SIZE.x - 24.0).max(0.0), (mon.y - BAR_SIZE.y - 110.0).max(0.0));
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
        }
    }

    /// Parks the bar outside the selected region on the region's monitor:
    /// centred below it, else above, else to the right, else to the left. If
    /// the region leaves no room on any side, the bar goes to the bottom
    /// centre of the screen (it will overlap; the user can drag it).
    pub(super) fn place_bar_near_region(&mut self, ctx: &egui::Context) {
        self.bar_moved_by_us();
        let Some((m, r)) = self.region_monitor() else { return };
        let ppp = ctx.pixels_per_point();
        let scale = m.scale_factor.max(0.1);
        // Title-bar height in physical px (outer minus inner of the current window).
        let deco = ctx
            .input(|i| Some((i.viewport().outer_rect?.height() - i.viewport().inner_rect?.height()) * ppp))
            .unwrap_or(32.0 * ppp)
            .max(0.0);
        let (bw, bh) = (BAR_SIZE.x * ppp, BAR_SIZE.y * ppp + deco);
        // Clearance from the region edge: frame stroke + gap + breathing room.
        let band = (THICKNESS_PT * scale).round() + GAP_PX as f32 + 16.0;
        let (rx, ry) = ((m.x + r.x as i32) as f32, (m.y + r.y as i32) as f32);
        let (rw, rh) = (r.width as f32, r.height as f32);
        // Usable screen area (taskbar allowance at the bottom, as in `enter_compact`).
        let (x0, y0) = (m.x as f32, m.y as f32);
        let (x1, y1) = (x0 + m.width as f32, y0 + m.height as f32 - 110.0 * scale);
        let clamp_x = |x: f32| x.clamp(x0, (x1 - bw).max(x0));
        let clamp_y = |y: f32| y.clamp(y0, (y1 - bh).max(y0));
        let cx = rx + rw / 2.0 - bw / 2.0;
        let cy = ry + rh / 2.0 - bh / 2.0;

        let candidates = [
            (clamp_x(cx), ry + rh + band),  // below
            (clamp_x(cx), ry - band - bh),  // above
            (rx + rw + band, clamp_y(cy)),  // right
            (rx - band - bw, clamp_y(cy)),  // left
        ];
        let fits = |(x, y): (f32, f32)| x >= x0 && y >= y0 && x + bw <= x1 && y + bh <= y1;
        let (x, y) = candidates
            .into_iter()
            .find(|&c| fits(c))
            .unwrap_or((clamp_x(cx), clamp_y(y1 - bh)));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2(x / ppp, y / ppp)));
    }

    /// Marks that the bar's position is about to change because of our own
    /// viewport command, so `follow_bar` resyncs instead of moving the region.
    fn bar_moved_by_us(&mut self) {
        self.bar_anchor = None;
        self.bar_settle_until = Some(Instant::now() + BAR_SETTLE);
    }

    /// In compact mode the title-bar X restores the full window instead of
    /// quitting. Returns true when a close was intercepted this frame.
    pub(super) fn intercept_close(&mut self, ctx: &egui::Context) -> bool {
        if !self.compact || !ctx.input(|i| i.viewport().close_requested()) {
            return false;
        }
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        self.exit_compact(ctx);
        true
    }

    /// Restores the full window at its previous position.
    pub(super) fn exit_compact(&mut self, ctx: &egui::Context) {
        self.compact = false;
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(760.0, 600.0)));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(800.0, 640.0)));
        if let Some(r) = self.saved_rect {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(r.min));
        }
    }

    /// Stamps `message_at` whenever the footer message text changes (for toasts).
    pub(super) fn track_message(&mut self) {
        let current = self.message.as_ref().map(|m| m.0.clone());
        if current != self.last_message {
            self.last_message = current;
            self.message_at = Some(Instant::now());
        }
    }

    fn source_dimensions(&self) -> String {
        match self.source_size() {
            Some((w, h)) => {
                let (ow, oh) = self.format.size.resolve(w, h);
                format!("{ow}×{oh}")
            }
            None => "—".into(),
        }
    }

    pub(super) fn minibar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Drag the window by its empty background.
        let full = ui.max_rect();
        let bg = ui.interact(full, ui.id().with("bar-drag"), Sense::drag());
        if bg.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        let recording = self.is_recording();
        let paused = matches!(&self.state, State::Recording(r) if r.is_paused());

        ui.horizontal(|ui| {
            // ----- Recording Area -----
            group_box(ui, "Recording Area", 262.0, |ui| {
                ui.add_enabled_ui(!recording, |ui| {
                    ui.horizontal(|ui| {
                        let (icon, label) = match self.source_kind {
                            SourceKind::Region => (icons::REGION, "Region"),
                            SourceKind::Monitor => (icons::MONITOR, "Monitor"),
                            SourceKind::Window => (icons::WINDOW, "Window"),
                        };
                        let mut picked: Option<SourceKind> = None;
                        ui.menu_button(RichText::new(format!("{icon}  {label}  {}", icons::CARET_DOWN)).size(14.0), |ui| {
                            for (kind, icon, label) in [
                                (SourceKind::Region, icons::REGION, "Region"),
                                (SourceKind::Monitor, icons::MONITOR, "Monitor"),
                                (SourceKind::Window, icons::WINDOW, "Window"),
                            ] {
                                if ui.button(format!("{icon}  {label}")).clicked() {
                                    picked = Some(kind);
                                    ui.close();
                                }
                            }
                        });
                        if let Some(kind) = picked {
                            self.source_kind = kind;
                            if kind == SourceKind::Region && self.region.is_none() {
                                self.open_picker();
                            }
                        }
                        // Concrete target.
                        match self.source_kind {
                            SourceKind::Monitor if self.monitors.len() > 1 => {
                                let label = self.monitors.get(self.monitor_idx).map(|m| m.name.clone()).unwrap_or_default();
                                egui::ComboBox::from_id_salt("mini-monitor").width(96.0).selected_text(label).show_ui(ui, |ui| {
                                    for (i, m) in self.monitors.iter().enumerate() {
                                        ui.selectable_value(&mut self.monitor_idx, i, m.label());
                                    }
                                });
                            }
                            SourceKind::Window => {
                                let label = self.windows.get(self.window_idx).map(|w| w.title.clone()).unwrap_or("Pick…".into());
                                egui::ComboBox::from_id_salt("mini-window").width(96.0).selected_text(truncate(&label, 12)).show_ui(ui, |ui| {
                                    for (i, w) in self.windows.iter().enumerate() {
                                        ui.selectable_value(&mut self.window_idx, i, w.label());
                                    }
                                });
                            }
                            SourceKind::Region
                                if ui.small_button("Select…").on_hover_text("Drag a new region").clicked() =>
                            {
                                self.open_picker();
                            }
                            _ => {}
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Dimensions").color(TEXT_DIM).small());
                    ui.label(RichText::new(self.source_dimensions()).color(TEXT_BRIGHT).small());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button(icons::REFRESH).on_hover_text("Refresh monitors and windows").clicked() {
                            self.refresh_sources();
                        }
                    });
                });
            });

            // ----- Recorded inputs -----
            group_box(ui, "Recorded inputs", 350.0, |ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!recording, |ui| {
                        input_toggle(ui, icons::MIC, "Microphone", &mut self.mic_enabled, self.mics.get(self.mic_idx).cloned());
                        input_toggle(ui, icons::SPEAKER, "System audio", &mut self.system_audio, None);
                        let mut show_cursor = self.mouse_fx.read().unwrap().show_cursor;
                        let before = show_cursor;
                        input_toggle(ui, icons::CURSOR, "Cursor", &mut show_cursor, None);
                        if show_cursor != before {
                            self.mouse_fx.write().unwrap().show_cursor = show_cursor;
                        }
                        let (title, _) = self.format.video_summary(&self.encoders, self.source_size());
                        let tip = format!("Format settings – {} / {}", self.format.container.label(), title);
                        if icon_button(ui, icons::GEAR, &tip).clicked() {
                            self.open_format_dialog();
                        }
                    });
                });
            });

            // ----- Controls -----
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                let can_record = self.selected_source().is_some();
                match rec_button(ui, recording, can_record) {
                    RecClick::Start => self.start_recording(ctx),
                    RecClick::Stop => self.stop_recording(),
                    RecClick::None => {}
                }
                if pause_button(ui, recording, paused) {
                    self.toggle_pause();
                }
                // Status column (fixed width so the timer never wraps).
                ui.allocate_ui_with_layout(Vec2::new(104.0, GROUP_H), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(104.0);
                    ui.add_space(14.0);
                    match &self.state {
                        State::Recording(rec) => {
                            let s = rec.stats();
                            let elapsed = rec.elapsed();
                            let bytes = s.bytes_written.load(std::sync::atomic::Ordering::Relaxed);
                            if paused {
                                ui.label(RichText::new(format!("‖ {}", format_duration(elapsed))).strong().color(WARN_YELLOW).size(15.0));
                            } else {
                                ui.label(RichText::new(format!("● {}", format_duration(elapsed))).strong().color(REC_RED).size(15.0));
                            }
                            ui.label(RichText::new(human_bytes(bytes)).color(TEXT_DIM).small());
                            if s.error().is_some() || rec.is_finished() {
                                self.stop_recording();
                            } else {
                                ctx.request_repaint_after(Duration::from_millis(250));
                            }
                        }
                        State::Picking(_) => {
                            ui.label(RichText::new("Select region…").color(ACCENT).small());
                        }
                        State::Idle => {
                            let ready = self.selected_source().is_some();
                            let text = if ready { "Ready" } else { "Pick a source" };
                            ui.label(RichText::new(text).color(TEXT_DIM).small());
                        }
                    }
                });
            });
        });

        // Toast with the latest message (saved file / error).
        if let (Some((msg, is_err)), Some(at)) = (&self.message, self.message_at)
            && at.elapsed() < TOAST_TTL
        {
            let color = if *is_err { ERR_RED } else { OK_GREEN };
            let pos = egui::pos2(full.left() + 6.0, full.bottom() - 2.0);
            ui.painter().text(pos, Align2::LEFT_BOTTOM, msg, FontId::proportional(11.0), color);
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// Bordered group with a small centred title, like Camtasia's "Recording Area".
fn group_box(ui: &mut egui::Ui, title: &str, width: f32, content: impl FnOnce(&mut egui::Ui)) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, GROUP_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), STATUS_BG);
    p.rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(width, 18.0)),
        CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 },
        BUTTON_BG,
    );
    p.text(rect.min + Vec2::new(width / 2.0, 9.0), Align2::CENTER_CENTER, title, FontId::proportional(12.0), TEXT_NORMAL);
    p.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, SEPARATOR), egui::StrokeKind::Inside);
    let inner = egui::Rect::from_min_max(rect.min + Vec2::new(8.0, 22.0), rect.max - Vec2::new(8.0, 4.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
    content(&mut child);
}

/// Icon toggle with a caption underneath ("Microphone on").
fn input_toggle(ui: &mut egui::Ui, icon: &str, name: &str, value: &mut bool, tip: Option<String>) {
    ui.vertical(|ui| {
        ui.set_width(92.0);
        ui.vertical_centered(|ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 34.0), Sense::click());
            if resp.clicked() {
                *value = !*value;
            }
            let p = ui.painter();
            let fill = if resp.hovered() { BUTTON_HOVER } else { BUTTON_BG };
            p.rect_filled(rect, CornerRadius::same(3), fill);
            let color = if *value { TEXT_BRIGHT } else { TEXT_DIM };
            p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(20.0), color);
            let badge = if *value { (icons::CHECK, OK_GREEN) } else { (icons::XMARK, ERR_RED) };
            p.text(rect.right_bottom() - Vec2::new(6.0, 6.0), Align2::CENTER_CENTER, badge.0, FontId::proportional(10.0), badge.1);
            let caption = format!("{name} {}", if *value { "on" } else { "off" });
            ui.label(RichText::new(caption).size(11.0).color(if *value { TEXT_NORMAL } else { TEXT_DIM }));
            if let Some(t) = tip {
                resp.on_hover_text(t);
            }
        });
    });
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}
