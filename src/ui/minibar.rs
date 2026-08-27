//! Compact floating recorder bar (Camtasia-style): recording area, recorded
//! inputs, pause / rec, and an expand button back to the full window.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, CornerRadius, FontId, Layout, RichText, Sense, Stroke, Vec2, ViewportCommand, WindowLevel};

use super::icons;
use super::theme::*;
use super::{format_duration, human_bytes, pause_button, rec_button, App, RecClick, SourceKind, State};

/// Inner size of the bar window, in points.
pub const BAR_SIZE: Vec2 = Vec2::new(780.0, 104.0);
const GROUP_H: f32 = 86.0;
const TOAST_TTL: Duration = Duration::from_secs(5);

impl App {
    /// Collapses the main window into the floating bar (always on top, bottom-right).
    pub(super) fn enter_compact(&mut self, ctx: &egui::Context) {
        let (outer, monitor) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
        self.saved_rect = outer;
        self.live.stop();
        self.compact = true;
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
        let (w, h) = match self.source_kind {
            SourceKind::Monitor => self.monitors.get(self.monitor_idx).map(|m| (m.width, m.height)).unwrap_or((0, 0)),
            SourceKind::Window => self.windows.get(self.window_idx).map(|w| (w.width, w.height)).unwrap_or((0, 0)),
            SourceKind::Region => self.region.map(|(_, r)| (r.width, r.height)).unwrap_or((0, 0)),
        };
        if w == 0 {
            "—".into()
        } else if self.half_resolution {
            format!("{}×{}", w / 2, h / 2)
        } else {
            format!("{w}×{h}")
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
            group_box(ui, "Recorded inputs", 300.0, |ui| {
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
                    });
                });
            });

            // ----- Controls -----
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if expand_button(ui).clicked() {
                    self.exit_compact(ctx);
                }
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

fn expand_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(30.0, 40.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(3), BUTTON_HOVER);
    }
    p.text(rect.center(), Align2::CENTER_CENTER, icons::EXPAND, FontId::proportional(16.0), TEXT_BRIGHT);
    resp.on_hover_text("Back to the full window")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}
