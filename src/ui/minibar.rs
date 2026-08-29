//! Compact floating recorder bar: one flat strip with the source picker, the
//! input toggles, the format button, a status capsule and pause / REC.
//! Closing the bar window (title-bar X) does not quit the app — it restores
//! the full window instead.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Layout, RichText, Sense, Vec2, ViewportCommand, WindowLevel};

use super::format_dialog::FormatSection;
use super::icons;
use super::region_frame::{BAND_PT, GAP_PX};
use super::theme::*;
use super::widgets::*;
use super::{format_duration, human_bytes, App, SourceKind, State};
use crate::t;

/// Initial inner size of the bar window, in points. The bar grows once (see
/// `App::bar_size`) when localized labels need more room than this.
pub const BAR_SIZE: Vec2 = Vec2::new(900.0, 72.0);
const MAX_BAR_WIDTH: f32 = 1400.0;
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
        let bar = self.bar_size;
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(400.0, 60.0)));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(bar));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        if let Some(mon) = monitor {
            // Bottom-right corner, above the taskbar.
            let pos = egui::pos2((mon.x - bar.x - 24.0).max(0.0), (mon.y - bar.y - 110.0).max(0.0));
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
        let (bw, bh) = (self.bar_size.x * ppp, self.bar_size.y * ppp + deco);
        // Clearance from the region edge: frame stroke + gap + breathing room.
        let band = (BAND_PT * scale).round() + GAP_PX as f32 + 16.0;
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
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(820.0, 600.0)));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(880.0, 660.0)));
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

        // Where the left-to-right content ends and the right cluster begins;
        // used to grow the window when the two would collide.
        let mut left_end = full.left();
        let mut right_start = full.right();

        ui.allocate_ui_with_layout(ui.available_size(), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;

            // ----- source -----
            ui.add_enabled_ui(!recording, |ui| {
                let (icon, label) = match self.source_kind {
                    SourceKind::Region => (icons::REGION, t!(MODE_REGION)),
                    SourceKind::Monitor => (icons::MONITOR, t!(MODE_MONITOR)),
                    SourceKind::Window => (icons::WINDOW, t!(MODE_WINDOW)),
                };
                let mut picked: Option<SourceKind> = None;
                ui.style_mut().spacing.button_padding = Vec2::new(12.0, 7.0);
                ui.menu_button(RichText::new(format!("{icon}  {label}  {}", icons::CARET_DOWN)), |ui| {
                    ui.style_mut().spacing.button_padding = Vec2::new(10.0, 6.0);
                    for (kind, icon, label) in [
                        (SourceKind::Region, icons::REGION, t!(MODE_REGION)),
                        (SourceKind::Monitor, icons::MONITOR, t!(MODE_MONITOR)),
                        (SourceKind::Window, icons::WINDOW, t!(MODE_WINDOW)),
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
                        egui::ComboBox::from_id_salt("mini-monitor").width(130.0).selected_text(truncate(&label, 14)).show_ui(
                            ui,
                            |ui| {
                                for (i, m) in self.monitors.iter().enumerate() {
                                    ui.selectable_value(&mut self.monitor_idx, i, m.label());
                                }
                            },
                        );
                    }
                    SourceKind::Window => {
                        let label =
                            self.windows.get(self.window_idx).map(|w| w.title.clone()).unwrap_or_else(|| t!(BAR_PICK).into());
                        egui::ComboBox::from_id_salt("mini-window").width(130.0).selected_text(truncate(&label, 14)).show_ui(
                            ui,
                            |ui| {
                                for (i, w) in self.windows.iter().enumerate() {
                                    ui.selectable_value(&mut self.window_idx, i, w.label());
                                }
                            },
                        );
                    }
                    SourceKind::Region
                        if tinted_button_small(ui, t!(BAR_SELECT)).on_hover_text(t!(BAR_DRAG_NEW_REGION)).clicked() =>
                    {
                        self.open_picker();
                    }
                    _ => {}
                }
                ui.label(secondary(self.source_dimensions()));
                if icon_button(ui, icons::REFRESH, t!(BAR_REFRESH_TIP)).clicked() {
                    self.refresh_sources();
                }
            });

            vdivider(ui, 28.0);

            // ----- inputs -----
            ui.add_enabled_ui(!recording, |ui| {
                let mic_name = match self.mics.get(self.mic_idx) {
                    Some(dev) => format!("{} ({dev})", t!(MICROPHONE)),
                    None => t!(MICROPHONE).to_string(),
                };
                icon_toggle(ui, icons::MIC, &mic_name, &mut self.mic_enabled);
                icon_toggle(ui, icons::SPEAKER, t!(SYSTEM_AUDIO), &mut self.system_audio);
                let mut show_cursor = self.mouse_fx.read().unwrap().show_cursor;
                let before = show_cursor;
                icon_toggle(ui, icons::CURSOR, t!(CURSOR), &mut show_cursor);
                if show_cursor != before {
                    self.mouse_fx.write().unwrap().show_cursor = show_cursor;
                }
                vdivider(ui, 28.0);
                // ⚙ opens a small menu: Video format… / Audio format… (separate dialogs).
                let (title, _) = self.format.video_summary(&self.encoders, self.source_size());
                let tip = t!(BAR_FORMAT_TIP, self.format.container.label(), title);
                let mut section: Option<FormatSection> = None;
                {
                    let v = &mut ui.style_mut().visuals.widgets;
                    v.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.inactive.bg_fill = egui::Color32::TRANSPARENT;
                    v.hovered.weak_bg_fill = FILL_HOVER;
                    v.open.weak_bg_fill = FILL_HOVER;
                    ui.style_mut().spacing.button_padding = Vec2::new(9.0, 9.0);
                }
                ui.menu_button(RichText::new(icons::GEAR).size(15.0), |ui| {
                    ui.style_mut().spacing.button_padding = Vec2::new(10.0, 6.0);
                    ui.style_mut().visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    if ui.button(format!("{}  {}", icons::FILM, t!(BOX_VIDEO))).clicked() {
                        section = Some(FormatSection::Video);
                        ui.close();
                    }
                    if ui.button(format!("{}  {}", icons::SPEAKER, t!(BOX_AUDIO))).clicked() {
                        section = Some(FormatSection::Audio);
                        ui.close();
                    }
                })
                .response
                .on_hover_text(tip);
                if let Some(section) = section {
                    self.open_format_dialog(section);
                }
            });
            left_end = ui.min_rect().right();

            // ----- status + controls -----
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_record = self.selected_source().is_some();
                match rec_button_sized(ui, self.rec_mode(), can_record, 52.0) {
                    RecClick::Start => self.start_recording(ctx),
                    RecClick::Stop => self.stop_recording(),
                    RecClick::Cancel => self.cancel_countdown(),
                    RecClick::None => {}
                }
                if pause_button(ui, recording, paused) {
                    self.toggle_pause();
                }
                ui.add_space(4.0);
                let timer_w = capsule_width_for(ui, "‖  00:00:00");
                let toast_max = (ui.min_rect().left() - left_end - 24.0).max(timer_w * 2.0);
                match &self.state {
                    State::Recording(rec) => {
                        let s = rec.stats();
                        let elapsed = rec.elapsed();
                        let bytes = s.bytes_written.load(std::sync::atomic::Ordering::Relaxed);
                        let (tint, glyph) = if paused { (Tint::Orange, "‖") } else { (Tint::Red, "●") };
                        let text = format!("{glyph}  {}", format_duration(elapsed));
                        status_capsule(ui, tint, &text, Some(timer_w), None, Some(&human_bytes(bytes)));
                        if s.error().is_some() || rec.is_finished() {
                            self.stop_recording();
                        } else {
                            ctx.request_repaint_after(Duration::from_millis(250));
                        }
                    }
                    State::Picking(_) => {
                        status_capsule(ui, Tint::Blue, &format!("{}  {}", icons::REGION, t!(SELECT_REGION)), None, None, None);
                    }
                    State::Countdown { started } => {
                        let left = (self.countdown_secs as f32 - started.elapsed().as_secs_f32()).ceil().max(1.0);
                        status_capsule(ui, Tint::Orange, &t!(BAR_STARTING_IN, left), None, None, Some(t!(BAR_ESC_CANCELS)));
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                    State::Idle => {
                        // The latest message (saved file / error) takes over the capsule for a few seconds.
                        let toast = match (&self.message, self.message_at) {
                            (Some((msg, is_err)), Some(at)) if at.elapsed() < TOAST_TTL => Some((msg.clone(), *is_err)),
                            _ => None,
                        };
                        match toast {
                            Some((msg, is_err)) => {
                                let tint = if is_err { Tint::Red } else { Tint::Green };
                                status_capsule(ui, tint, &msg, None, Some(toast_max), Some(&msg));
                                ctx.request_repaint_after(Duration::from_millis(500));
                            }
                            None if can_record => {
                                status_capsule(ui, Tint::Green, t!(BAR_READY), None, None, None);
                            }
                            None => {
                                status_capsule(ui, Tint::Gray, t!(BAR_PICK_SOURCE), None, None, None);
                            }
                        }
                    }
                }
                right_start = ui.min_rect().left();
            });
        });

        // Long localized labels: grow the window once instead of overlapping.
        let overlap = left_end + 16.0 - right_start;
        if overlap > 0.5 && self.bar_size.x < MAX_BAR_WIDTH {
            self.bar_size.x = (self.bar_size.x + overlap.ceil()).min(MAX_BAR_WIDTH);
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(self.bar_size));
            self.bar_moved_by_us();
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}
