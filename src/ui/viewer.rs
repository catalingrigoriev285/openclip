//! Full-window media viewer: a big media area over a transport bar
//!
//! It takes the whole window — toolbar, status strip, navigation and footer are
//! all hidden while it is up — and hands back to the library on Esc or the back
//! button. Video and audio play through [`crate::player`], which needs Media
//! Foundation; elsewhere the file gets a poster and a link to the system player.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, ColorImage, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense,
    TextureHandle, TextureOptions, UiBuilder, Vec2,
};

use crate::player::{load_image, PlaybackState, Player};
use crate::t;
use crate::video::preview::PreviewImage;

use super::library::{open_with_default, reveal_in_folder, LibraryTab};
use super::theme::*;
use super::widgets::*;
use super::{human_bytes, icons, short_duration, App, State};

/// How often a drag along the timeline is allowed to actually seek. Every
/// mouse move would re-decode from a keyframe, which no file survives smoothly.
const SEEK_THROTTLE: Duration = Duration::from_millis(120);

/// Arrow-key jump.
const SKIP: Duration = Duration::from_secs(5);

/// Height of the transport bar's control row.
const CONTROL_H: f32 = 44.0;

/// One open file.
pub(super) struct Viewer {
    pub path: PathBuf,
    name: String,
    kind: LibraryTab,
    /// `None` for still pictures, which need no playback machinery.
    player: Option<Player>,
    /// Why a still picture could not be shown.
    error: Option<String>,
    tex: Option<TextureHandle>,
    dims: (u32, u32),
    size_on_disk: u64,
    /// Position being dragged, and whether playback should resume after.
    scrub: Option<(f32, bool)>,
    last_seek: Option<Instant>,
    /// Pictures: 1:1 instead of fit-to-window.
    actual_size: bool,
}

impl Viewer {
    fn duration(&self) -> Option<Duration> {
        self.player.as_ref().and_then(|p| p.duration())
    }

    fn position(&self) -> Duration {
        self.player.as_ref().map(|p| p.position()).unwrap_or_default()
    }

    /// Where the timeline knob sits, `0..=1`. A drag wins over the clock so the
    /// handle follows the mouse rather than the decoder.
    fn fraction(&self) -> f32 {
        if let Some((f, _)) = self.scrub {
            return f;
        }
        match self.duration() {
            Some(d) if d > Duration::ZERO => {
                (self.position().as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0)
            }
            _ => 0.0,
        }
    }

    /// The time the timeline is pointing at, dragged or played.
    fn shown_position(&self) -> Duration {
        match (self.scrub, self.duration()) {
            (Some((f, _)), Some(d)) => d.mul_f32(f),
            _ => self.position(),
        }
    }

    fn state(&self) -> PlaybackState {
        self.player.as_ref().map(|p| p.state()).unwrap_or(PlaybackState::Paused)
    }

    fn is_playing(&self) -> bool {
        self.player.as_ref().is_some_and(|p| p.is_playing())
    }

    /// True when there is nothing to play here — the wrong platform, an
    /// unreadable file, or a picture that would not decode.
    fn dead(&self) -> bool {
        self.error.is_some() || self.state().is_dead()
    }

    fn message(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(t!(VIEWER_NO_IMAGE, e));
        }
        match self.state() {
            PlaybackState::Unsupported => Some(t!(VIEWER_NO_PLAYBACK).into()),
            PlaybackState::Failed => {
                Some(t!(VIEWER_FAILED, self.player.as_ref().and_then(|p| p.error()).unwrap_or_default()))
            }
            _ => None,
        }
    }

    /// "1920×1080 · 12.4 MiB · 0:42", the same separator the library rows use.
    fn details(&self) -> String {
        let mut parts = Vec::new();
        if self.dims.0 > 0 {
            parts.push(format!("{}×{}", self.dims.0, self.dims.1));
        }
        parts.push(human_bytes(self.size_on_disk));
        match self.duration() {
            Some(d) => parts.push(short_duration(d)),
            None if self.kind != LibraryTab::Images => parts.push(t!(VIEWER_UNKNOWN_LENGTH).into()),
            None => {}
        }
        parts.join("  ·  ")
    }
}

/// What the user asked for while the viewer was being drawn. Collected rather
/// than acted on inline so the borrow of the viewer can end first.
#[derive(Default)]
enum Action {
    #[default]
    None,
    Close,
    Reveal,
    Delete,
    External,
    Snapshot(PreviewImage),
}

impl App {
    /// Opens `path` in the viewer, or hands it to the system player when the
    /// viewer cannot or should not take it.
    pub(super) fn open_viewer(&mut self, ctx: &egui::Context, path: PathBuf) {
        // A modal owns the screen while it is up (the same rule the updater
        // follows), and playing a file back mid-recording would feed the sound
        // into the system-audio track and fight the encoder for the CPU.
        if self.format_dialog.is_open() || self.library.confirm_delete.is_some() || !matches!(self.state, State::Idle)
        {
            open_with_default(&path);
            return;
        }
        let Some(kind) = LibraryTab::for_path(&path) else {
            open_with_default(&path);
            return;
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let size_on_disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut viewer = Viewer {
            path,
            name,
            kind,
            player: None,
            error: None,
            tex: None,
            dims: (0, 0),
            size_on_disk,
            scrub: None,
            last_seek: None,
            actual_size: false,
        };

        if kind == LibraryTab::Images {
            match load_image(&viewer.path) {
                Ok(img) => {
                    viewer.dims = (img.width, img.height);
                    upload(ctx, &mut viewer.tex, &img);
                }
                Err(e) => viewer.error = Some(format!("{e:#}")),
            }
        } else {
            let repaint = {
                let ctx = ctx.clone();
                Arc::new(move || ctx.request_repaint())
            };
            let player = Player::open(&viewer.path, repaint);
            player.set_volume(self.viewer_volume);
            player.set_muted(self.viewer_muted);
            viewer.player = Some(player);
        }
        self.viewer = Some(viewer);
    }

    pub(super) fn close_viewer(&mut self) {
        self.viewer = None;
    }

    pub(super) fn viewer_is_open(&self) -> bool {
        self.viewer.is_some()
    }

    /// Draws the whole window. Called instead of the normal panels.
    pub(super) fn viewer_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(mut v) = self.viewer.take() else { return };
        // Recording takes precedence: hand the window back.
        if !matches!(self.state, State::Idle) {
            return;
        }

        // New frames from the decoder.
        if let Some(p) = &mut v.player
            && p.poll()
            && let Some(frame) = p.frame()
        {
            v.dims = (frame.image.width, frame.image.height);
            upload(ctx, &mut v.tex, &frame.image);
        }

        let mut action = Action::None;
        if !ctx.egui_wants_keyboard_input() {
            action = v.keyboard(ctx);
        }

        v.header(ui, &mut action);
        v.transport(ui, &mut action);
        v.media(ui, &mut action);

        // Repaint on the frame grid while something is moving.
        if v.is_playing() {
            ctx.request_repaint_after(v.player.as_ref().map(|p| p.frame_interval()).unwrap_or(SKIP) / 2);
        }

        let path = v.path.clone();
        let mut keep = true;
        match action {
            Action::None => {}
            Action::Close => keep = false,
            Action::Reveal => reveal_in_folder(&path),
            Action::External => open_with_default(&path),
            Action::Delete => self.library.confirm_delete = Some(path),
            Action::Snapshot(image) => self.viewer_snapshot(image),
        }
        if keep {
            self.viewer_volume = v.player.as_ref().map(|p| p.volume()).unwrap_or(self.viewer_volume);
            self.viewer_muted = v.player.as_ref().map(|p| p.is_muted()).unwrap_or(self.viewer_muted);
            self.viewer = Some(v);
        }
    }

    /// Writes the frame on screen next to the recordings, exactly as the
    /// toolbar's snapshot button does — only the pixels come from the decoder.
    fn viewer_snapshot(&mut self, image: PreviewImage) {
        let path = self.timestamped("png");
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(&self.output_dir).ok();
            let img = image::RgbaImage::from_raw(image.width, image.height, image.rgba)
                .ok_or_else(|| anyhow::anyhow!("bad image buffer"))?;
            img.save(&path).map_err(|e| anyhow::anyhow!("{e}"))
        })();
        match result {
            Ok(()) => self.saved(path, t!(WHAT_SNAPSHOT)),
            Err(e) => self.message = Some((t!(MSG_SNAPSHOT_FAILED, format!("{e:#}")), true)),
        }
    }
}

impl Viewer {
    fn keyboard(&mut self, ctx: &egui::Context) -> Action {
        let (esc, space, left, right, comma, period) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Comma),
                i.key_pressed(egui::Key::Period),
            )
        });
        if esc {
            return Action::Close;
        }
        let Some(p) = &self.player else { return Action::None };
        if space {
            p.toggle();
        }
        if left {
            p.seek(self.position().saturating_sub(SKIP));
        }
        if right {
            p.seek(self.position() + SKIP);
        }
        if comma {
            p.step(-1);
        }
        if period {
            p.step(1);
        }
        Action::None
    }

    fn header(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        egui::Panel::top("viewer-header")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let back = format!("{}  {}", icons::CHEVRON_LEFT, t!(VIEWER_BACK));
                    if tinted_button_small(ui, &back).clicked() {
                        *action = Action::Close;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, icons::XMARK, t!(VIEWER_CLOSE_TIP)).clicked() {
                            *action = Action::Close;
                        }
                        if icon_button(ui, icons::TRASH, t!(DELETE)).clicked() {
                            *action = Action::Delete;
                        }
                        if icon_button(ui, icons::FOLDER, t!(VIEWER_REVEAL_TIP)).clicked() {
                            *action = Action::Reveal;
                        }
                        // Whatever is left in the middle carries the title.
                        let w = ui.available_width().max(40.0);
                        ui.allocate_ui_with_layout(Vec2::new(w, 34.0), Layout::top_down(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add(
                                egui::Label::new(RichText::new(&self.name).font(semibold(15.0)).color(LABEL))
                                    .truncate(),
                            )
                            .on_hover_text(self.path.display().to_string());
                            ui.label(RichText::new(self.details()).size(11.0).color(LABEL_2));
                        });
                    });
                });
            });
    }

    fn media(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PREVIEW_BG))
            .show(ui, |ui| {
                if let Some(msg) = self.message() {
                    self.unavailable(ui, &msg, action);
                    return;
                }
                match self.kind {
                    LibraryTab::Images => self.picture(ui),
                    LibraryTab::Audios => self.artwork(ui),
                    LibraryTab::Videos => self.video(ui),
                }
            });
    }

    fn video(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        match &self.tex {
            Some(tex) if self.dims.0 > 0 => {
                // Fill the window: a viewer that letterboxes a small clip into a
                // postage stamp is not doing its job.
                let size = fit_size(avail, self.dims, 8.0, 8.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(&*tex).fit_to_exact_size(size));
                });
            }
            _ => {
                ui.centered_and_justified(|ui| {
                    ui.label(secondary(t!(VIEWER_OPENING)));
                });
            }
        }
    }

    fn picture(&mut self, ui: &mut egui::Ui) {
        let Some(tex) = self.tex.clone() else { return };
        let avail = ui.available_size();
        if self.actual_size {
            // Panning comes free with a two-axis scroll area.
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                let size = Vec2::new(self.dims.0 as f32, self.dims.1 as f32);
                let resp = ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(size).sense(Sense::click()));
                if resp.clicked() {
                    self.actual_size = false;
                }
            });
            return;
        }
        // Never magnify a small picture past 1:1.
        let size = fit_size(avail, self.dims, 16.0, 1.0);
        ui.centered_and_justified(|ui| {
            let resp = ui.add(
                egui::Image::from_texture(&tex)
                    .fit_to_exact_size(size)
                    .corner_radius(6.0)
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                self.actual_size = true;
            }
        });
    }

    /// Audio files have nothing to show, so they get a Now-Playing style tile.
    fn artwork(&mut self, ui: &mut egui::Ui) {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 110.0).max(0.0));
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(160.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(24), FILL);
                p.text(rect.center(), Align2::CENTER_CENTER, icons::MUSIC, FontId::proportional(56.0), LABEL_2);
                ui.add_space(14.0);
                ui.label(RichText::new(&self.name).font(semibold(15.0)).color(LABEL));
            });
        });
    }

    fn unavailable(&mut self, ui: &mut egui::Ui, msg: &str, action: &mut Action) {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 70.0).max(0.0));
                let glyph = match self.kind {
                    LibraryTab::Videos => icons::FILM,
                    LibraryTab::Images => icons::IMAGE,
                    LibraryTab::Audios => icons::MUSIC,
                };
                ui.label(RichText::new(glyph).size(44.0).color(LABEL_3));
                ui.add_space(12.0);
                ui.add(egui::Label::new(RichText::new(msg).color(LABEL_2)).wrap());
                ui.add_space(14.0);
                if tinted_button(ui, t!(VIEWER_OPEN_EXTERNAL)).clicked() {
                    *action = Action::External;
                }
            });
        });
    }

    fn transport(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        egui::Panel::bottom("viewer-transport")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(16, 12)))
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(CARD)
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        if self.kind == LibraryTab::Images {
                            self.picture_controls(ui);
                        } else {
                            self.timeline(ui);
                            ui.add_space(4.0);
                            self.controls(ui, action);
                        }
                    });
            });
    }

    fn picture_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(CONTROL_H - 12.0);
            let (icon, label) = if self.actual_size {
                (icons::MINIMIZE, t!(VIEWER_FIT))
            } else {
                (icons::EXPAND, t!(VIEWER_ACTUAL_SIZE))
            };
            if !self.dead() && tinted_button_small(ui, &format!("{icon}  {label}")).clicked() {
                self.actual_size = !self.actual_size;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(secondary(self.details()));
            });
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let dur = self.duration();
        let seekable = dur.is_some() && !self.dead();
        // Measured once so the readouts never jitter as the digits change.
        let stamp_w = ui
            .painter()
            .layout_no_wrap("0:00:00".into(), FontId::proportional(11.0), LABEL_2)
            .size()
            .x
            + 6.0;

        ui.horizontal(|ui| {
            let show = |ui: &mut egui::Ui, d: Duration| {
                ui.allocate_ui_with_layout(Vec2::new(stamp_w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(short_duration(d)).size(11.0).color(LABEL_2));
                });
            };
            show(ui, self.shown_position());
            let total = dur.unwrap_or_default();
            let width = (ui.available_width() - stamp_w - 12.0).max(40.0);
            let mut frac = self.fraction();
            let out = ui
                .add_enabled_ui(seekable, |ui| scrubber(ui, "viewer-seek", &mut frac, width))
                .inner;
            if seekable {
                self.on_scrub(out, frac, total);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match dur {
                Some(d) => {
                    ui.label(RichText::new(short_duration(d)).size(11.0).color(LABEL_2));
                }
                None => {
                    ui.label(RichText::new("--:--").size(11.0).color(LABEL_3));
                }
            });
        });
    }

    /// Follows the knob: preview while dragging, seek at a throttled rate so a
    /// long drag does not queue a keyframe re-decode per mouse move.
    fn on_scrub(&mut self, out: ScrubOut, frac: f32, total: Duration) {
        let Some(p) = &self.player else { return };
        if out.dragging && self.scrub.is_none() {
            let was_playing = p.is_playing();
            p.pause();
            self.scrub = Some((frac, was_playing));
        }
        if let Some((f, _)) = &mut self.scrub {
            *f = frac;
        }
        if out.changed
            && self.last_seek.map(|t| t.elapsed() >= SEEK_THROTTLE).unwrap_or(true)
        {
            p.seek(total.mul_f32(frac));
            self.last_seek = Some(Instant::now());
        }
        if out.committed {
            p.seek(total.mul_f32(frac));
            self.last_seek = Some(Instant::now());
            if let Some((_, was_playing)) = self.scrub.take()
                && was_playing
            {
                p.play();
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        let live = !self.dead();
        let video = self.kind == LibraryTab::Videos;
        let has_audio = self.player.as_ref().is_some_and(|p| p.has_audio());

        // The transport is pinned to the middle of the bar, not to the middle
        // third of it: laying the three clusters out in sequence would let the
        // volume slider's width decide where "centre" falls.
        let gap = ui.spacing().item_spacing.x;
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), CONTROL_H), Sense::hover());
        let mid = Rect::from_center_size(row.center(), Vec2::new(transport_width(video, gap), CONTROL_H));
        let left = Rect::from_min_max(row.min, egui::pos2(mid.left() - gap, row.max.y));
        let right = Rect::from_min_max(egui::pos2(mid.right() + gap, row.min.y), row.max);

        // Left: the volume cluster.
        ui.scope_builder(UiBuilder::new().max_rect(left).layout(Layout::left_to_right(Align::Center)), |ui| {
            if has_audio {
                self.volume_controls(ui);
            }
        });

        // Centre: step back, play/pause, step forward.
        ui.scope_builder(UiBuilder::new().max_rect(mid).layout(Layout::left_to_right(Align::Center)), |ui| {
            ui.add_enabled_ui(live, |ui| {
                if video && icon_button(ui, icons::STEP_BACK, t!(VIEWER_STEP_BACK)).clicked()
                    && let Some(p) = &self.player
                {
                    p.step(-1);
                }
                if play_pause_button(ui, self.is_playing(), CONTROL_H)
                    && let Some(p) = &self.player
                {
                    p.toggle();
                }
                if video && icon_button(ui, icons::STEP_FORWARD, t!(VIEWER_STEP_FORWARD)).clicked()
                    && let Some(p) = &self.player
                {
                    p.step(1);
                }
            });
        });

        // Right: snapshot hard against the edge, then anything the player says.
        ui.scope_builder(UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)), |ui| {
            if video && live {
                let frame = self.player.as_ref().and_then(|p| p.frame()).map(|f| f.image.clone());
                ui.add_enabled_ui(frame.is_some(), |ui| {
                    if icon_button(ui, icons::CAMERA, t!(VIEWER_SNAPSHOT_TIP)).clicked()
                        && let Some(image) = frame
                    {
                        *action = Action::Snapshot(image);
                    }
                });
            }
            if self.state() == PlaybackState::Ended {
                ui.label(RichText::new(t!(VIEWER_ENDED)).size(11.0).color(LABEL_2));
            } else if self.player.as_ref().and_then(|p| p.note()).is_some() {
                ui.label(RichText::new(t!(VIEWER_NO_AUDIO_DEVICE)).size(11.0).color(ORANGE));
            }
        });
    }

    fn volume_controls(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.player else { return };
        let muted = p.is_muted();
        let (glyph, tip) =
            if muted { (icons::VOLUME_MUTE, t!(VIEWER_UNMUTE)) } else { (icons::SPEAKER, t!(VIEWER_MUTE)) };
        if icon_button(ui, glyph, tip).clicked() {
            p.set_muted(!muted);
        }
        let mut vol = p.volume();
        let out = scrubber(ui, "viewer-volume", &mut vol, 84.0);
        if out.changed {
            p.set_volume(vol);
            // Nudging the slider is the natural way to undo a mute.
            if muted && vol > 0.0 {
                p.set_muted(false);
            }
        }
        ui.add_space(2.0);
    }
}

fn upload(ctx: &egui::Context, tex: &mut Option<TextureHandle>, img: &PreviewImage) {
    let color = ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
    match tex {
        Some(t) => t.set(color, TextureOptions::LINEAR),
        None => *tex = Some(ctx.load_texture("viewer", color, TextureOptions::LINEAR)),
    }
}

/// Width of the centred transport cluster, from the sizes its buttons allocate:
/// [`icon_button`] is 34 pt square and [`play_pause_button`] is `CONTROL_H`.
/// Audio files have no frame steps, so it is just the one button.
fn transport_width(video: bool, gap: f32) -> f32 {
    if video { 34.0 + gap + CONTROL_H + gap + 34.0 } else { CONTROL_H }
}
