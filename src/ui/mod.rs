//! egui application, laid out in the style of classic recorder tools: a top
//! toolbar (recording modes, audio toggles, pause, REC), a left navigation
//! with settings pages, and a file browser (+ optional preview tab) on Home.

mod library;
pub mod picker;
mod theme;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Layout, Margin, PointerButton,
    RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};

use crate::audio::capture::list_input_devices;
use crate::capture::monitors::{
    list_monitors, list_windows, screenshot_source, source_origin, MonitorInfo, WindowInfo,
};
use crate::capture::{self as cap, CaptureConfig, CaptureHandle, Rect, Source};
use crate::pipeline::{RecordConfig, Recorder};
use crate::video::mouse_fx::{MouseFx, MouseSampler, ARROW, CLICK_DURATION};
use crate::video::preview::{make_preview, PreviewImage};

use library::{open_with_default, reveal_in_folder, Library, LibraryTab};
use picker::{Picker, PickerOutcome};
use theme::*;

type SharedFx = Arc<RwLock<MouseFx>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Region,
    Monitor,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    General,
    Video,
    Image,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeTab {
    Videos,
    Images,
    Audios,
    Preview,
}

impl HomeTab {
    fn library(self) -> Option<LibraryTab> {
        match self {
            HomeTab::Videos => Some(LibraryTab::Videos),
            HomeTab::Images => Some(LibraryTab::Images),
            HomeTab::Audios => Some(LibraryTab::Audios),
            HomeTab::Preview => None,
        }
    }

    fn from_library(tab: LibraryTab) -> Self {
        match tab {
            LibraryTab::Videos => HomeTab::Videos,
            LibraryTab::Images => HomeTab::Images,
            LibraryTab::Audios => HomeTab::Audios,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoTab {
    Record,
    Format,
    Mouse,
}

enum State {
    Idle,
    Picking(Picker),
    Recording(Recorder),
}

/// Live low-frame-rate capture of the selected source shown on the Preview
/// tab, so the preview (cursor and mouse effects included) matches what will
/// be recorded. Only runs while the tab is visible.
struct LivePreview {
    handle: Option<CaptureHandle>,
    source: Option<Source>,
    native_cursor: bool,
    slot: Arc<Mutex<Option<PreviewImage>>>,
    error: Option<String>,
    last_attempt: Option<Instant>,
}

impl LivePreview {
    const FPS: u32 = 12;

    fn new() -> Self {
        Self {
            handle: None,
            source: None,
            native_cursor: true,
            slot: Default::default(),
            error: None,
            last_attempt: None,
        }
    }

    /// Starts/restarts the preview capture if the source or cursor mode changed.
    fn ensure(&mut self, source: Option<Source>, fx: &SharedFx, ctx: &egui::Context) {
        let native_cursor = fx.read().unwrap().native_cursor();
        let changed = source != self.source || native_cursor != self.native_cursor;
        let retry = self.handle.is_none()
            && self.error.is_some()
            && self.last_attempt.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true);
        if !changed && !retry {
            return;
        }
        self.stop();
        self.source = source.clone();
        self.native_cursor = native_cursor;
        self.error = None;
        let Some(source) = source else { return };
        self.last_attempt = Some(Instant::now());
        let slot = self.slot.clone();
        let ctx = ctx.clone();
        let fx = fx.clone();
        let is_window = matches!(source, Source::Window { .. });
        let src = source.clone();
        let mut origin = source_origin(&source).unwrap_or((0, 0));
        let mut sampler: Option<MouseSampler> = None;
        let mut n = 0u32;
        let sink: cap::FrameSink = Box::new(move |mut frame| {
            let fx = fx.read().unwrap().clone();
            if fx.any_overlay() {
                let s = sampler.get_or_insert_with(MouseSampler::new);
                s.sample();
                n += 1;
                if is_window
                    && n.is_multiple_of(30)
                    && let Ok(o) = source_origin(&src)
                {
                    origin = o;
                }
                let (cursor, clicks) = s.mapped(origin, 1.0);
                fx.apply(&mut frame, cursor, &clicks, 1.0);
            }
            *slot.lock().unwrap() = Some(make_preview(&frame, 720));
            ctx.request_repaint();
            true
        });
        let cfg = CaptureConfig { source, fps: Self::FPS, show_cursor: native_cursor };
        match cap::start(cfg, Instant::now(), sink) {
            Ok(h) => self.handle = Some(h),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.stop();
        }
        self.source = None;
    }

    fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    fn take(&self) -> Option<PreviewImage> {
        self.slot.lock().unwrap().take()
    }
}

pub struct App {
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    mics: Vec<String>,
    source_kind: SourceKind,
    monitor_idx: usize,
    window_idx: usize,
    region: Option<(u32, Rect)>,
    fps: u32,
    bitrate_kbps: u32,
    half_resolution: bool,
    mouse_fx: SharedFx,
    fx_demo_clicks: Vec<(Instant, bool)>,
    system_audio: bool,
    mic_enabled: bool,
    mic_idx: usize,
    output_dir: PathBuf,
    file_prefix: String,
    tab: Tab,
    home_tab: HomeTab,
    video_tab: VideoTab,
    state: State,
    live: LivePreview,
    library: Library,
    preview_tex: Option<TextureHandle>,
    preview_dims: (u32, u32),
    message: Option<(String, bool)>, // (text, is_error)
    last_file: Option<PathBuf>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let monitors = list_monitors().unwrap_or_default();
        let windows = list_windows().unwrap_or_default();
        let mics = list_input_devices();
        let output_dir = default_output_dir();
        let mut library = Library::new();
        library.refresh(&output_dir, true);
        Self {
            monitors,
            windows,
            mics,
            source_kind: SourceKind::Monitor,
            monitor_idx: 0,
            window_idx: 0,
            region: None,
            fps: 30,
            bitrate_kbps: 6000,
            half_resolution: false,
            mouse_fx: Arc::new(RwLock::new(MouseFx::default())),
            fx_demo_clicks: Vec::new(),
            system_audio: true,
            mic_enabled: false,
            mic_idx: 0,
            output_dir,
            file_prefix: "openclip".into(),
            tab: Tab::Home,
            home_tab: HomeTab::Videos,
            video_tab: VideoTab::Record,
            state: State::Idle,
            live: LivePreview::new(),
            library,
            preview_tex: None,
            preview_dims: (0, 0),
            message: None,
            last_file: None,
        }
    }

    fn selected_source(&self) -> Option<Source> {
        match self.source_kind {
            SourceKind::Monitor => self.monitors.get(self.monitor_idx).map(|m| Source::Monitor { id: m.id }),
            SourceKind::Window => self.windows.get(self.window_idx).map(|w| Source::Window { id: w.id }),
            SourceKind::Region => self.region.map(|(monitor_id, rect)| Source::Region { monitor_id, rect }),
        }
    }

    fn source_label(&self) -> String {
        match self.source_kind {
            SourceKind::Monitor => {
                self.monitors.get(self.monitor_idx).map(|m| m.label()).unwrap_or("No monitor".into())
            }
            SourceKind::Window => {
                self.windows.get(self.window_idx).map(|w| w.label()).unwrap_or("No window selected".into())
            }
            SourceKind::Region => match self.region {
                Some((mid, r)) => {
                    let mon =
                        self.monitors.iter().find(|m| m.id == mid).map(|m| m.name.clone()).unwrap_or_default();
                    format!("Region {}×{} at ({}, {}) on {}", r.width, r.height, r.x, r.y, mon)
                }
                None => "No region selected".into(),
            },
        }
    }

    fn refresh_sources(&mut self) {
        self.monitors = list_monitors().unwrap_or_default();
        self.windows = list_windows().unwrap_or_default();
        self.mics = list_input_devices();
        self.monitor_idx = self.monitor_idx.min(self.monitors.len().saturating_sub(1));
        self.window_idx = self.window_idx.min(self.windows.len().saturating_sub(1));
        self.mic_idx = self.mic_idx.min(self.mics.len().saturating_sub(1));
    }

    fn is_recording(&self) -> bool {
        matches!(self.state, State::Recording(_))
    }

    fn show_preview_tab(&mut self) {
        self.tab = Tab::Home;
        self.home_tab = HomeTab::Preview;
    }

    fn timestamped(&self, ext: &str) -> PathBuf {
        let prefix = if self.file_prefix.trim().is_empty() { "openclip" } else { self.file_prefix.trim() };
        self.output_dir.join(format!("{prefix}-{}.{ext}", timestamp()))
    }

    fn saved(&mut self, path: PathBuf, what: &str) {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.message = Some((format!("{what} saved: {} ({})", path.display(), human_bytes(size)), false));
        self.library.select_path(&path, &self.output_dir);
        if self.tab == Tab::Home && self.home_tab != HomeTab::Preview {
            self.home_tab = HomeTab::from_library(self.library.tab);
        }
        self.last_file = Some(path);
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let Some(source) = self.selected_source() else {
            self.message = Some(("Select something to record first.".into(), true));
            return;
        };
        // Release the preview capture before opening the recording session.
        self.live.stop();
        let config = RecordConfig {
            source,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            half_resolution: self.half_resolution,
            mouse_fx: self.mouse_fx.read().unwrap().clone(),
            system_audio: self.system_audio,
            microphone: self.mic_enabled.then(|| self.mics.get(self.mic_idx).cloned()),
            output: self.timestamped("mp4"),
        };
        let ctx2 = ctx.clone();
        let cb: crate::pipeline::PreviewCallback = Arc::new(move || ctx2.request_repaint());
        match Recorder::start(config, Some(cb)) {
            Ok(rec) => {
                self.message = None;
                self.state = State::Recording(rec);
            }
            Err(e) => self.message = Some((format!("Could not start recording: {e:#}"), true)),
        }
    }

    fn stop_recording(&mut self) {
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            match rec.stop() {
                Ok(path) => self.saved(path, "Recording"),
                Err(e) => self.message = Some((format!("Recording failed: {e:#}"), true)),
            }
        }
    }

    fn toggle_pause(&mut self) {
        if let State::Recording(rec) = &mut self.state {
            if rec.is_paused() {
                rec.resume();
            } else {
                rec.pause();
            }
        }
    }

    fn take_snapshot(&mut self) {
        let Some(source) = self.selected_source() else {
            self.message = Some(("Select something to capture first.".into(), true));
            return;
        };
        let path = self.timestamped("png");
        let result = screenshot_source(&source).and_then(|frame| {
            std::fs::create_dir_all(&self.output_dir).ok();
            let img = xcap::image::RgbaImage::from_raw(frame.width, frame.height, frame.data)
                .ok_or_else(|| anyhow::anyhow!("bad image buffer"))?;
            img.save(&path).map_err(|e| anyhow::anyhow!("{e}"))
        });
        match result {
            Ok(()) => self.saved(path, "Snapshot"),
            Err(e) => self.message = Some((format!("Snapshot failed: {e:#}"), true)),
        }
    }

    fn upload_preview(&mut self, ctx: &egui::Context, img: &PreviewImage) {
        let color = ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
        self.preview_dims = (img.width, img.height);
        match &mut self.preview_tex {
            Some(tex) => tex.set(color, TextureOptions::LINEAR),
            None => self.preview_tex = Some(ctx.load_texture("preview", color, TextureOptions::LINEAR)),
        }
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let preview_visible = self.tab == Tab::Home && self.home_tab == HomeTab::Preview;
        match &self.state {
            State::Recording(rec) => {
                if let Some(img) = rec.preview().take() {
                    self.upload_preview(ctx, &img);
                }
            }
            State::Idle => {
                if preview_visible {
                    self.live.ensure(self.selected_source(), &self.mouse_fx, ctx);
                    if let Some(img) = self.live.take() {
                        self.upload_preview(ctx, &img);
                    }
                } else if self.live.is_running() {
                    self.live.stop();
                }
            }
            State::Picking(_) => {}
        }
    }

    // ----- toolbar -----------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let recording = self.is_recording();
        let paused = matches!(&self.state, State::Recording(r) if r.is_paused());
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.add_sized(
                Vec2::new(100.0, 54.0),
                egui::Label::new(RichText::new("OPENCLIP").strong().size(20.0).color(TEXT_BRIGHT)),
            );
            ui.add_space(4.0);

            ui.add_enabled_ui(!recording, |ui| {
                for (kind, icon, label) in [
                    (SourceKind::Region, "▭", "Region"),
                    (SourceKind::Monitor, "🖥", "Monitor"),
                    (SourceKind::Window, "▣", "Window"),
                ] {
                    if mode_button(ui, icon, label, self.source_kind == kind).clicked() {
                        self.select_mode(kind);
                    }
                }
                ui.add_space(6.0);
                toggle_button(ui, "🔊", "System audio", &mut self.system_audio);
                toggle_button(ui, "🎤", "Microphone", &mut self.mic_enabled);
                let mut show_cursor = self.mouse_fx.read().unwrap().show_cursor;
                let before = show_cursor;
                toggle_button(ui, "🖱", "Show cursor", &mut show_cursor);
                if show_cursor != before {
                    self.mouse_fx.write().unwrap().show_cursor = show_cursor;
                }
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if icon_button(ui, "📷", "Take a snapshot (PNG)").clicked() {
                    self.take_snapshot();
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
            });
        });
    }

    /// Switches recording mode and opens the Preview tab when a choice is needed.
    fn select_mode(&mut self, kind: SourceKind) {
        self.source_kind = kind;
        let needs_choice = match kind {
            SourceKind::Window => true,
            SourceKind::Monitor => self.monitors.len() > 1,
            SourceKind::Region => false,
        };
        if kind == SourceKind::Region && self.region.is_none() {
            self.open_picker();
        } else if needs_choice {
            self.show_preview_tab();
        }
    }

    fn open_picker(&mut self) {
        self.live.stop();
        match Picker::new(&self.monitors) {
            Ok(p) => self.state = State::Picking(p),
            Err(e) => self.message = Some((format!("Region picker: {e:#}"), true)),
        }
    }

    fn status_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            match &self.state {
                State::Recording(rec) => {
                    let s = rec.stats();
                    let elapsed = rec.elapsed();
                    let encoded = s.frames_encoded.load(Ordering::Relaxed);
                    let dropped = s.frames_dropped.load(Ordering::Relaxed);
                    let bytes = s.bytes_written.load(Ordering::Relaxed);
                    let (w, h) = (s.width.load(Ordering::Relaxed), s.height.load(Ordering::Relaxed));
                    let fps = if elapsed.as_secs_f64() > 0.5 { encoded as f64 / elapsed.as_secs_f64() } else { 0.0 };
                    if rec.is_paused() {
                        ui.label(RichText::new("‖").color(WARN_YELLOW).size(16.0));
                        ui.label(
                            RichText::new(format!("PAUSED  {}", format_duration(elapsed))).strong().color(WARN_YELLOW),
                        );
                    } else {
                        ui.label(RichText::new("●").color(REC_RED).size(16.0));
                        ui.label(RichText::new(format!("REC  {}", format_duration(elapsed))).strong().color(TEXT_BRIGHT));
                    }
                    ui.separator();
                    ui.label(format!("{w}×{h}   {fps:.1} fps   {dropped} dropped   {}", human_bytes(bytes)));
                    if let Some(n) = s.audio_note.lock().unwrap().as_ref() {
                        ui.separator();
                        ui.label(RichText::new(n).color(WARN_YELLOW));
                    }
                    if s.error().is_some() || rec.is_finished() {
                        self.stop_recording();
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(250));
                    }
                }
                State::Picking(_) => {
                    ui.label(RichText::new("⬚").color(ACCENT));
                    ui.label("Drag a rectangle on the screen to select the recording region (Esc to cancel)");
                }
                State::Idle => {
                    ui.label(RichText::new("▶").color(ACCENT));
                    let text = if self.selected_source().is_some() {
                        format!("Ready to record: {}", self.source_label())
                    } else {
                        "Please select a recording source".to_string()
                    };
                    ui.label(text);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(6.0);
                        if ui.small_button("Change…").on_hover_text("Choose the monitor, window or region").clicked() {
                            self.show_preview_tab();
                        }
                    });
                }
            }
        });
    }

    // ----- navigation + pages --------------------------------------------------

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        for (tab, icon, label) in [
            (Tab::Home, "🏠", "Home"),
            (Tab::General, "⚙", "General"),
            (Tab::Video, "🎞", "Video"),
            (Tab::Image, "🖼", "Image"),
            (Tab::About, "ℹ", "About"),
        ] {
            if nav_entry(ui, icon, label, self.tab == tab).clicked() {
                self.tab = tab;
            }
        }
    }

    fn page(&mut self, ui: &mut egui::Ui) {
        match self.tab {
            Tab::Home => self.page_home(ui),
            Tab::General => self.page_general(ui),
            Tab::Video => self.page_video(ui),
            Tab::Image => self.page_image(ui),
            Tab::About => self.page_about(ui),
        }
    }

    // ----- Home: Videos | Images | Audios | Preview -----------------------------

    fn page_home(&mut self, ui: &mut egui::Ui) {
        let mut home_tab = self.home_tab;
        tab_strip(
            ui,
            &[
                (HomeTab::Videos, "Videos"),
                (HomeTab::Images, "Images"),
                (HomeTab::Audios, "Audios"),
                (HomeTab::Preview, "Preview"),
            ],
            &mut home_tab,
        );
        if home_tab != self.home_tab {
            self.home_tab = home_tab;
            if let Some(lib) = home_tab.library() {
                self.library.set_tab(lib, &self.output_dir);
            }
        }
        ui.add_space(4.0);
        match self.home_tab {
            HomeTab::Preview => {
                let recording = self.is_recording();
                self.source_row(ui, recording);
                ui.add_space(6.0);
                self.preview_panel(ui);
            }
            _ => self.library_panel(ui),
        }
    }

    fn library_panel(&mut self, ui: &mut egui::Ui) {
        self.library.refresh(&self.output_dir, false);
        // Folder row.
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(self.output_dir.display().to_string()).color(TEXT_DIM).small())
                .on_hover_text("Output folder (change it under General)");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("⟳").on_hover_text("Refresh list").clicked() {
                    self.library.refresh(&self.output_dir, true);
                }
                if ui.small_button("📂").on_hover_text("Open folder").clicked() {
                    open_folder(&self.output_dir);
                }
            });
        });
        ui.painter().hline(
            ui.available_rect_before_wrap().x_range(),
            ui.cursor().top() + 2.0,
            Stroke::new(1.0, SEPARATOR),
        );
        ui.add_space(6.0);

        // File list.
        let list_h = (ui.available_height() - 48.0).max(80.0);
        let mut clicked: Option<usize> = None;
        let mut activated: Option<usize> = None;
        egui::Frame::new().fill(PREVIEW_BG).corner_radius(CornerRadius::same(3)).show(ui, |ui| {
            ui.set_min_height(list_h);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                if self.library.entries.is_empty() {
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(format!("No {} yet", self.library.tab.label().to_lowercase())).color(TEXT_DIM));
                    });
                }
                for (i, e) in self.library.entries.iter().enumerate() {
                    let selected = self.library.selected == Some(i);
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
                    let p = ui.painter();
                    if selected {
                        p.rect_filled(rect, CornerRadius::ZERO, ACCENT.gamma_multiply(0.55));
                    } else if resp.hovered() {
                        p.rect_filled(rect, CornerRadius::ZERO, BUTTON_HOVER);
                    }
                    let color = if selected { TEXT_BRIGHT } else { TEXT_NORMAL };
                    let name_rect = egui::Rect::from_min_max(rect.min + Vec2::new(8.0, 0.0), rect.max - Vec2::new(90.0, 0.0));
                    p.with_clip_rect(name_rect).text(
                        name_rect.left_center(),
                        Align2::LEFT_CENTER,
                        &e.name,
                        FontId::proportional(14.0),
                        color,
                    );
                    p.text(
                        rect.right_center() - Vec2::new(8.0, 0.0),
                        Align2::RIGHT_CENTER,
                        human_bytes(e.size),
                        FontId::proportional(13.0),
                        color,
                    );
                    if resp.double_clicked() {
                        activated = Some(i);
                    } else if resp.clicked() {
                        clicked = Some(i);
                    }
                    resp.on_hover_text(e.path.display().to_string());
                }
            });
        });
        if let Some(i) = clicked {
            self.library.selected = Some(i);
        }
        if let Some(i) = activated {
            self.library.selected = Some(i);
            if let Some(e) = self.library.entries.get(i) {
                open_with_default(&e.path);
            }
        }

        // Actions.
        ui.add_space(6.0);
        let selected = self.library.selected_entry().map(|e| e.path.clone());
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 16.0) / 3.0;
            let has = selected.is_some();
            if ui.add_enabled(has, egui::Button::new("▶  Play").min_size(Vec2::new(w, 30.0))).clicked()
                && let Some(p) = &selected
            {
                open_with_default(p);
            }
            if ui.add_enabled(has, egui::Button::new("📂  Folder").min_size(Vec2::new(w, 30.0))).clicked()
                && let Some(p) = &selected
            {
                reveal_in_folder(p);
            }
            if ui.add_enabled(has, egui::Button::new("🗑  Delete").min_size(Vec2::new(w, 30.0))).clicked() {
                self.library.confirm_delete = selected.clone();
            }
        });
    }

    fn source_row(&mut self, ui: &mut egui::Ui, recording: bool) {
        ui.add_enabled_ui(!recording, |ui| {
            ui.horizontal(|ui| {
                let combo_w = (ui.available_width() - 120.0).max(160.0);
                match self.source_kind {
                    SourceKind::Monitor => {
                        ui.label("Monitor");
                        let label =
                            self.monitors.get(self.monitor_idx).map(|m| m.label()).unwrap_or("No monitors".into());
                        egui::ComboBox::from_id_salt("monitor").width(combo_w).selected_text(label).show_ui(ui, |ui| {
                            for (i, m) in self.monitors.iter().enumerate() {
                                ui.selectable_value(&mut self.monitor_idx, i, m.label());
                            }
                        });
                    }
                    SourceKind::Window => {
                        ui.label("Window");
                        let label =
                            self.windows.get(self.window_idx).map(|w| w.label()).unwrap_or("No windows".into());
                        egui::ComboBox::from_id_salt("window").width(combo_w).selected_text(label).show_ui(ui, |ui| {
                            for (i, w) in self.windows.iter().enumerate() {
                                ui.selectable_value(&mut self.window_idx, i, w.label());
                            }
                        });
                    }
                    SourceKind::Region => {
                        ui.label(self.source_label());
                        if ui.button("Select region…").clicked() {
                            self.open_picker();
                        }
                    }
                }
                if ui.button("⟳").on_hover_text("Refresh monitors, windows and audio devices").clicked() {
                    self.refresh_sources();
                }
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        egui::Frame::new().fill(PREVIEW_BG).corner_radius(CornerRadius::same(4)).show(ui, |ui| {
            ui.set_min_size(avail);
            match &self.preview_tex {
                Some(tex) if self.preview_dims.0 > 0 => {
                    let (w, h) = (self.preview_dims.0 as f32, self.preview_dims.1 as f32);
                    let scale = ((avail.x - 8.0) / w).min((avail.y - 8.0) / h).min(3.0);
                    let size = egui::vec2(w * scale, h * scale);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::from_texture(&*tex).fit_to_exact_size(size));
                    });
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        let text = match (&self.live.error, self.selected_source()) {
                            (Some(e), _) => format!("Preview unavailable: {e}"),
                            (None, None) => "Select a monitor, window or region to preview".into(),
                            (None, Some(_)) => "Starting preview…".into(),
                        };
                        ui.label(RichText::new(text).color(TEXT_DIM).size(16.0));
                    });
                }
            }
        });
    }

    // ----- settings pages ---------------------------------------------------------

    fn page_general(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Output");
        settings_row(ui, "Save to", |ui| {
            ui.label(RichText::new(self.output_dir.display().to_string()).color(TEXT_BRIGHT));
        });
        settings_row(ui, "", |ui| {
            if ui.button("Choose folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(&self.output_dir).pick_folder()
            {
                self.output_dir = dir;
                self.library.refresh(&self.output_dir, true);
            }
            if ui.button("Open").clicked() {
                open_folder(&self.output_dir);
            }
        });
        settings_row(ui, "File name prefix", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.file_prefix).desired_width(160.0));
            ui.label(RichText::new(format!("→ {}-YYYYMMDD-HHMMSS.mp4", self.file_prefix.trim())).color(TEXT_DIM).small());
        });
        ui.add_space(14.0);
        section_title(ui, "Sources");
        settings_row(ui, "Devices", |ui| {
            if ui.button("Refresh monitors, windows and devices").clicked() {
                self.refresh_sources();
            }
        });
    }

    fn page_video(&mut self, ui: &mut egui::Ui) {
        let mut vt = self.video_tab;
        tab_strip(ui, &[(VideoTab::Record, "Record"), (VideoTab::Format, "Format"), (VideoTab::Mouse, "Mouse")], &mut vt);
        self.video_tab = vt;
        ui.add_space(6.0);
        match self.video_tab {
            VideoTab::Record => self.video_record_tab(ui),
            VideoTab::Format => self.video_format_tab(ui),
            VideoTab::Mouse => self.video_mouse_tab(ui),
        }
    }

    fn video_record_tab(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Record");
        let current = self.mouse_fx.read().unwrap().clone();
        let mut fx = current.clone();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.checkbox(&mut fx.show_cursor, "Show mouse cursor");
                ui.checkbox(&mut fx.click_effect, "Add mouse click effects");
                ui.checkbox(&mut fx.highlight, "Add mouse highlight effect");
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if ui.add(egui::Button::new("Settings").min_size(Vec2::new(150.0, 26.0))).clicked() {
                    self.video_tab = VideoTab::Mouse;
                }
            });
        });
        if fx != current {
            *self.mouse_fx.write().unwrap() = fx;
        }
        ui.add_space(6.0);
        ui.add_enabled_ui(!self.is_recording(), |ui| {
            settings_row(ui, "Size", |ui| {
                ui.selectable_value(&mut self.half_resolution, false, "Full size");
                ui.selectable_value(&mut self.half_resolution, true, "Half size");
            });
        });
        ui.add_space(14.0);
        section_title(ui, "Format – MP4");
        let size = if self.half_resolution { "Half size" } else { "Full size" };
        format_box(ui, "Video", "H264 – OpenH264 (CBR)", &format!("{size}, {}fps, {} kbps", self.fps, self.bitrate_kbps));
        format_box(ui, "Audio", "MP3 – MPEG-1 Layer III", &format!("48.0KHz, stereo, 160kbps – {}", self.audio_sources_label()));
        ui.horizontal(|ui| {
            ui.add_space(134.0);
            if ui.add(egui::Button::new("Settings").min_size(Vec2::new(150.0, 26.0))).clicked() {
                self.video_tab = VideoTab::Format;
            }
        });
    }

    fn audio_sources_label(&self) -> &'static str {
        match (self.system_audio, self.mic_enabled) {
            (true, true) => "system audio + microphone",
            (true, false) => "system audio",
            (false, true) => "microphone",
            (false, false) => "no audio",
        }
    }

    fn video_format_tab(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Video");
        ui.add_enabled_ui(!self.is_recording(), |ui| {
            settings_row(ui, "Codec", |ui| {
                ui.label(RichText::new("H264 – OpenH264 (CBR), MP4 container").color(TEXT_BRIGHT));
            });
            settings_row(ui, "Frame rate", |ui| {
                for f in [15u32, 24, 30, 60] {
                    ui.selectable_value(&mut self.fps, f, format!("{f} fps"));
                }
            });
            settings_row(ui, "Bitrate", |ui| {
                ui.add(egui::Slider::new(&mut self.bitrate_kbps, 500..=20_000).suffix(" kbps").logarithmic(true));
            });
            settings_row(ui, "Size", |ui| {
                ui.selectable_value(&mut self.half_resolution, false, "Full size");
                ui.selectable_value(&mut self.half_resolution, true, "Half size");
            });
            ui.label(
                RichText::new(
                    "Frames are timestamped, so dropped or skipped frames never desynchronise audio. \
                     If the status bar reports drops at 1080p, choose Half size or a lower frame rate.",
                )
                .color(TEXT_DIM)
                .small(),
            );
            ui.add_space(14.0);
            section_title(ui, "Audio");
            settings_row(ui, "Codec", |ui| {
                ui.label(RichText::new("MP3 – 48.0KHz, stereo, 160kbps").color(TEXT_BRIGHT));
            });
            settings_row(ui, "System audio", |ui| {
                ui.checkbox(&mut self.system_audio, "Record what you hear (speakers / headphones)");
            });
            settings_row(ui, "Microphone", |ui| {
                ui.checkbox(&mut self.mic_enabled, "Record microphone");
            });
            settings_row(ui, "Device", |ui| {
                ui.add_enabled_ui(self.mic_enabled && !self.mics.is_empty(), |ui| {
                    let label = self.mics.get(self.mic_idx).cloned().unwrap_or("No input devices".into());
                    let w = ui.available_width().min(360.0);
                    egui::ComboBox::from_id_salt("mic").width(w).selected_text(label).show_ui(ui, |ui| {
                        for (i, m) in self.mics.iter().enumerate() {
                            ui.selectable_value(&mut self.mic_idx, i, m);
                        }
                    });
                });
            });
        });
    }

    fn video_mouse_tab(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Mouse effects");
        let current = self.mouse_fx.read().unwrap().clone();
        let mut fx = current.clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(350.0);
                ui.checkbox(&mut fx.show_cursor, "Show mouse cursor");
                ui.add_enabled_ui(fx.show_cursor, |ui| {
                    settings_row(ui, "      Size", |ui| {
                        size_combo(ui, "cursor_size", &mut fx.cursor_size);
                        if fx.cursor_size != 100 {
                            ui.label(RichText::new("(app-drawn)").color(TEXT_DIM).small());
                        }
                    });
                });
                ui.add_space(6.0);
                ui.checkbox(&mut fx.click_effect, "Add mouse click effect");
                ui.add_enabled_ui(fx.click_effect, |ui| {
                    settings_row(ui, "      Size", |ui| size_combo(ui, "click_size", &mut fx.click_size));
                    settings_row(ui, "      Left click color", |ui| color_swatch(ui, &mut fx.left_color));
                    settings_row(ui, "      Right click color", |ui| color_swatch(ui, &mut fx.right_color));
                });
                ui.add_space(6.0);
                ui.checkbox(&mut fx.highlight, "Add mouse highlight effect");
                ui.add_enabled_ui(fx.highlight, |ui| {
                    settings_row(ui, "      Size", |ui| size_combo(ui, "highlight_size", &mut fx.highlight_size));
                    settings_row(ui, "      Highlight color", |ui| color_swatch(ui, &mut fx.highlight_color));
                    settings_row(ui, "      Opacity", |ui| {
                        ui.add(egui::DragValue::new(&mut fx.highlight_opacity).range(0..=100).suffix(" %"));
                        ui.add(egui::Slider::new(&mut fx.highlight_opacity, 0..=100).show_value(false));
                    });
                });
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Preview").color(TEXT_NORMAL));
                ui.add_space(4.0);
                self.fx_preview(ui, &fx);
                ui.add_space(6.0);
                ui.label(RichText::new("Click inside to test the click effect.").color(TEXT_DIM).small());
            });
        });
        if fx != current {
            *self.mouse_fx.write().unwrap() = fx;
        }
    }

    /// Checkerboard square showing the cursor, halo and (on click) ripples.
    fn fx_preview(&mut self, ui: &mut egui::Ui, fx: &MouseFx) {
        let size = Vec2::splat(200.0);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let p = ui.painter_at(rect);
        let cell = 10.0;
        let cols = (size.x / cell).ceil() as i32;
        for cy in 0..cols {
            for cx in 0..cols {
                let c = if (cx + cy) % 2 == 0 { CHECKER_LIGHT } else { CHECKER_DARK };
                let r = egui::Rect::from_min_size(rect.min + Vec2::new(cx as f32 * cell, cy as f32 * cell), Vec2::splat(cell))
                    .intersect(rect);
                p.rect_filled(r, CornerRadius::ZERO, c);
            }
        }
        p.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, SEPARATOR), StrokeKind::Inside);
        let center = rect.center();
        if resp.clicked_by(PointerButton::Primary) {
            self.fx_demo_clicks.push((Instant::now(), false));
        }
        if resp.clicked_by(PointerButton::Secondary) {
            self.fx_demo_clicks.push((Instant::now(), true));
        }
        let now = Instant::now();
        self.fx_demo_clicks.retain(|(t, _)| now.duration_since(*t) < CLICK_DURATION);
        if fx.highlight && fx.highlight_opacity > 0 {
            let [r, g, b] = fx.highlight_color;
            let a = (fx.highlight_opacity.min(100) * 255 / 100) as u8;
            p.circle_filled(center, 32.0 * fx.highlight_size as f32 / 100.0, Color32::from_rgba_unmultiplied(r, g, b, a));
        }
        if fx.click_effect {
            for (t, right) in &self.fx_demo_clicks {
                let age = now.duration_since(*t).as_secs_f32() / CLICK_DURATION.as_secs_f32();
                let k = fx.click_size as f32 / 100.0;
                let radius = (10.0 + 32.0 * age) * k;
                let [r, g, b] = if *right { fx.right_color } else { fx.left_color };
                let a = (230.0 * (1.0 - age)) as u8;
                p.circle_stroke(center, radius, Stroke::new(3.0 * k.max(0.5), Color32::from_rgba_unmultiplied(r, g, b, a)));
            }
            if !self.fx_demo_clicks.is_empty() {
                ui.ctx().request_repaint();
            }
        }
        if fx.show_cursor {
            let s = fx.cursor_size as f32 / 100.0;
            for (y, row) in ARROW.iter().enumerate() {
                for (x, ch) in row.bytes().enumerate() {
                    let color = match ch {
                        b'X' => Color32::BLACK,
                        b'W' => Color32::WHITE,
                        _ => continue,
                    };
                    let r = egui::Rect::from_min_size(
                        center + Vec2::new(x as f32 * s, y as f32 * s),
                        Vec2::splat(s + 0.2),
                    );
                    p.rect_filled(r, CornerRadius::ZERO, color);
                }
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::Crosshair);
    }

    fn page_image(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Snapshot");
        format_box(ui, "Image", "PNG", "Full size, saved next to your recordings");
        ui.add_space(10.0);
        settings_row(ui, "Capture", |ui| {
            if ui.button("📷  Take snapshot now").clicked() {
                self.take_snapshot();
            }
        });
        settings_row(ui, "Source", |ui| {
            ui.label(RichText::new(self.source_label()).color(TEXT_DIM));
        });
    }

    fn page_about(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "openclip");
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(6.0);
        ui.label("A self-contained screen recorder: no ffmpeg, no system codecs.");
        ui.label("Video: H.264 via bundled OpenH264 · Audio: MP3 via bundled LAME · Container: in-house MP4 muxer");
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Licensed under Apache-2.0. OpenH264 is BSD-2-Clause (source build, no Cisco patent coverage); LAME is LGPL.",
            )
            .color(TEXT_DIM),
        );
        ui.add_space(6.0);
        ui.hyperlink_to("github.com/catalingrigoriev285/openclip", "https://github.com/catalingrigoriev285/openclip");
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            match &self.message {
                Some((msg, is_err)) => {
                    let color = if *is_err { ERR_RED } else { OK_GREEN };
                    ui.label(RichText::new(msg).color(color).small());
                    if !*is_err
                        && let Some(path) = self.last_file.clone()
                        && ui.small_button("Open folder").clicked()
                    {
                        reveal_in_folder(&path);
                    }
                }
                None => {
                    ui.label(RichText::new("openclip – screen recorder").color(TEXT_DIM));
                }
            }
        });
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = self.library.confirm_delete.clone() else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-delete")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Delete file?");
            ui.add_space(6.0);
            ui.label(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            ui.label(RichText::new("This permanently deletes the file from disk.").color(TEXT_DIM));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let del = egui::Button::new(RichText::new("Delete").color(TEXT_BRIGHT)).fill(REC_RED);
                if ui.add(del).clicked() {
                    match self.library.delete(&path, &self.output_dir) {
                        Ok(()) => self.message = Some((format!("Deleted {}", path.display()), false)),
                        Err(e) => self.message = Some((format!("Could not delete: {e}"), true)),
                    }
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.library.confirm_delete = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_preview(&ctx);

        if let State::Picking(picker) = &mut self.state {
            match picker.show(&ctx) {
                PickerOutcome::Pending => {}
                PickerOutcome::Selected(monitor_id, rect) => {
                    self.region = Some((monitor_id, rect));
                    self.source_kind = SourceKind::Region;
                    self.state = State::Idle;
                }
                PickerOutcome::Cancelled => self.state = State::Idle,
            }
        }

        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(TOOLBAR_BG).inner_margin(Margin::symmetric(4, 6)))
            .show(ui, |ui| self.toolbar(ui, &ctx));
        egui::Panel::top("status")
            .frame(egui::Frame::new().fill(STATUS_BG).inner_margin(Margin::symmetric(4, 5)))
            .show(ui, |ui| self.status_strip(ui, &ctx));
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(TOOLBAR_BG).inner_margin(Margin::symmetric(4, 5)))
            .show(ui, |ui| self.footer(ui));
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(160.0)
            .frame(egui::Frame::new().fill(NAV_BG))
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAGE_BG).inner_margin(Margin::same(14)))
            .show(ui, |ui| self.page(ui));

        self.delete_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        self.live.stop();
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            let _ = rec.stop();
        }
    }
}

// ----- widgets -------------------------------------------------------------------

/// Horizontal tab strip (Videos | Images | …). Returns true when the selection changed.
fn tab_strip<T: PartialEq + Copy>(ui: &mut egui::Ui, items: &[(T, &str)], selected: &mut T) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        for (value, label) in items {
            let is_sel = *selected == *value;
            let text = RichText::new(*label).size(15.0);
            let text = if is_sel { text.strong().color(TEXT_BRIGHT) } else { text.color(TEXT_NORMAL) };
            if ui.selectable_label(is_sel, text).clicked() && !is_sel {
                *selected = *value;
                changed = true;
            }
            ui.add_space(4.0);
        }
    });
    let rect = ui.available_rect_before_wrap();
    ui.painter().hline(rect.x_range(), rect.top() + 1.0, Stroke::new(1.0, SEPARATOR));
    ui.add_space(4.0);
    changed
}

/// Large recording-mode button (icon over label), highlighted when selected.
fn mode_button(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let size = Vec2::new(84.0, 54.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        ACCENT
    } else if resp.hovered() {
        BUTTON_HOVER
    } else {
        BUTTON_BG
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), fill);
    let text_color = if ui.is_enabled() { TEXT_BRIGHT } else { TEXT_DIM };
    p.text(rect.center() - Vec2::new(0.0, 8.0), Align2::CENTER_CENTER, icon, FontId::proportional(20.0), text_color);
    p.text(rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, label, FontId::proportional(12.0), text_color);
    resp.on_hover_text(format!("{label} recording mode"))
}

/// Square toggle with an icon; a small ✕ marks the "off" state.
fn toggle_button(ui: &mut egui::Ui, icon: &str, tip: &str, value: &mut bool) {
    let size = Vec2::new(50.0, 54.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *value = !*value;
    }
    let fill = if resp.hovered() { BUTTON_HOVER } else { BUTTON_BG };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), fill);
    let color = if *value { TEXT_BRIGHT } else { TEXT_DIM };
    p.text(rect.center() - Vec2::new(0.0, 6.0), Align2::CENTER_CENTER, icon, FontId::proportional(20.0), color);
    if *value {
        p.text(rect.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, "on", FontId::proportional(11.0), OK_GREEN);
    } else {
        p.text(rect.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, "✕", FontId::proportional(12.0), ERR_RED);
    }
    resp.on_hover_text(format!("{tip}: {}", if *value { "on" } else { "off" }));
}

fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 44.0), Sense::click());
    let fill = if resp.hovered() { BUTTON_HOVER } else { Color32::TRANSPARENT };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), fill);
    p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(22.0), TEXT_BRIGHT);
    resp.on_hover_text(tip)
}

/// Pause / resume button next to REC; only active while recording. Returns true on click.
fn pause_button(ui: &mut egui::Ui, recording: bool, paused: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(44.0, 44.0), Sense::click());
    let p = ui.painter();
    let fill = if resp.hovered() && recording { BUTTON_HOVER } else { Color32::TRANSPARENT };
    p.rect_filled(rect, CornerRadius::same(3), fill);
    let color = if !recording {
        TEXT_DIM
    } else if paused {
        WARN_YELLOW
    } else {
        TEXT_BRIGHT
    };
    let c = rect.center();
    if paused {
        // Play triangle.
        p.add(egui::Shape::convex_polygon(
            vec![c + Vec2::new(-7.0, -10.0), c + Vec2::new(10.0, 0.0), c + Vec2::new(-7.0, 10.0)],
            color,
            Stroke::NONE,
        ));
    } else {
        p.rect_filled(egui::Rect::from_center_size(c + Vec2::new(-5.0, 0.0), Vec2::new(5.0, 20.0)), CornerRadius::same(1), color);
        p.rect_filled(egui::Rect::from_center_size(c + Vec2::new(5.0, 0.0), Vec2::new(5.0, 20.0)), CornerRadius::same(1), color);
    }
    let resp = resp.on_hover_text(if paused { "Resume recording" } else { "Pause recording" });
    recording && resp.clicked()
}

enum RecClick {
    None,
    Start,
    Stop,
}

/// The big round REC button; shows a stop square while recording.
fn rec_button(ui: &mut egui::Ui, recording: bool, enabled: bool) -> RecClick {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(60.0, 60.0), Sense::click());
    let center = rect.center();
    let p = ui.painter();
    let color = if !enabled && !recording { TEXT_DIM } else { REC_RED };
    let hovered = resp.hovered() && (enabled || recording);
    if recording {
        p.circle_filled(center, 27.0, if hovered { REC_RED_HOVER } else { REC_RED });
        p.rect_filled(egui::Rect::from_center_size(center, Vec2::splat(18.0)), CornerRadius::same(2), TEXT_BRIGHT);
    } else {
        p.circle_stroke(center, 27.0, Stroke::new(3.0, color));
        if hovered {
            p.circle_filled(center, 24.0, Color32::from_rgba_unmultiplied(230, 40, 40, 40));
        }
        p.text(center, Align2::CENTER_CENTER, "REC", FontId::proportional(17.0), color);
    }
    let resp = resp.on_hover_text(if recording { "Stop recording" } else { "Start recording" });
    if !resp.clicked() {
        RecClick::None
    } else if recording {
        RecClick::Stop
    } else if enabled {
        RecClick::Start
    } else {
        RecClick::None
    }
}

fn nav_entry(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::ZERO, NAV_SELECTED);
        p.rect_filled(egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())), CornerRadius::ZERO, ACCENT);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::ZERO, BUTTON_HOVER);
    }
    let color = if selected { TEXT_BRIGHT } else { TEXT_NORMAL };
    p.text(rect.left_center() + Vec2::new(22.0, 0.0), Align2::LEFT_CENTER, icon, FontId::proportional(18.0), color);
    p.text(rect.left_center() + Vec2::new(54.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(15.0), color);
    resp
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).strong().size(16.0).color(TEXT_BRIGHT));
    let rect = ui.available_rect_before_wrap();
    let y = rect.top() + 2.0;
    ui.painter().hline(rect.left()..=rect.right(), y, Stroke::new(1.0, SEPARATOR));
    ui.add_space(10.0);
}

fn settings_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(Vec2::new(130.0, 24.0), egui::Label::new(RichText::new(label).color(TEXT_NORMAL)));
        add(ui);
    });
    ui.add_space(4.0);
}

/// Percent size selector used by the mouse-effect settings.
fn size_combo(ui: &mut egui::Ui, id: &str, value: &mut u32) {
    egui::ComboBox::from_id_salt(id).width(90.0).selected_text(format!("{value} %")).show_ui(ui, |ui| {
        for v in [50u32, 75, 100, 125, 150, 200, 300] {
            ui.selectable_value(value, v, format!("{v} %"));
        }
    });
}

/// Wide colour swatch that opens egui's colour picker.
fn color_swatch(ui: &mut egui::Ui, rgb: &mut [u8; 3]) {
    let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    ui.spacing_mut().interact_size = Vec2::new(64.0, 22.0);
    egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque);
    *rgb = [c.r(), c.g(), c.b()];
}

/// Dark "format summary" box like the Video/Audio boxes in classic recorders.
fn format_box(ui: &mut egui::Ui, label: &str, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(Vec2::new(130.0, 24.0), egui::Label::new(RichText::new(label).color(TEXT_NORMAL)));
        let width = ui.available_width().min(520.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(3), BUTTON_BG);
        p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.0, SEPARATOR), StrokeKind::Inside);
        p.text(rect.min + Vec2::new(12.0, 12.0), Align2::LEFT_TOP, title, FontId::proportional(14.0), TEXT_BRIGHT);
        p.text(rect.min + Vec2::new(12.0, 34.0), Align2::LEFT_TOP, detail, FontId::proportional(13.0), TEXT_DIM);
    });
    ui.add_space(4.0);
}

// ----- helpers -------------------------------------------------------------------

fn default_output_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    if let Some(home) = home {
        let videos = home.join("Videos");
        if videos.is_dir() {
            return videos;
        }
        return home;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil date from days since epoch (Howard Hinnant's algorithm), UTC.
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn open_folder(path: &std::path::Path) {
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    #[cfg(windows)]
    let cmd = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = std::process::Command::new("xdg-open").arg(dir).spawn();
    if let Err(e) = cmd {
        log::warn!("open folder: {e}");
    }
}
