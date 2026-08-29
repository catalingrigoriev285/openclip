//! egui application, laid out in the style of classic recorder tools: a top
//! toolbar (recording modes, audio toggles, pause, REC), a left navigation
//! with settings pages, and a file browser (+ optional preview tab) on Home.

mod format_dialog;
pub mod icons;
mod library;
mod minibar;
pub mod picker;
pub mod region_frame;
mod theme;
mod updater;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Layout, Margin, PointerButton,
    RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};

use crate::audio::capture::list_input_devices;
use crate::capture::monitors::{
    list_monitors, list_windows, screenshot_source, source_origin, MonitorInfo, WindowInfo,
};
use crate::capture::{self as cap, CaptureConfig, CaptureHandle, Rect, Source};
use crate::i18n::{self, Lang};
use crate::pipeline::{RecordConfig, Recorder, Stats};
use crate::settings::{FormatSettings, Settings};
use crate::t;
use crate::video::encoder::{available_encoders, refresh_encoders, EncoderInfo};
use crate::video::mouse_fx::{MouseFx, MouseSampler, ARROW, CLICK_DURATION};
use crate::video::preview::{make_preview, PreviewImage};

use format_dialog::{DialogOutcome, FormatDialog, FormatSection};
use library::{open_with_default, reveal_in_folder, Library, LibraryTab};
use picker::{Picker, PickerOutcome};
use theme::*;
use widgets::*;

type SharedFx = Arc<RwLock<MouseFx>>;

/// Application icon shown on the About page (same artwork as the window icon).
const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/android-chrome-192x192.png");

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
    Mouse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneralTab {
    Output,
    Recording,
    Appearance,
    Sources,
}

enum State {
    Idle,
    Picking(Picker),
    /// REC was pressed; recording starts when the countdown ends.
    Countdown { started: Instant },
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
    const FPS: u32 = 20;

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
                let (cursor, clicks) = s.mapped(origin, (1.0, 1.0));
                fx.apply(&mut frame, cursor, &clicks, 1.0);
            }
            *slot.lock().unwrap() = Some(make_preview(&frame, 720));
            ctx.request_repaint();
            true
        });
        let cfg = CaptureConfig { source, fps: Self::FPS, show_cursor: native_cursor, pool: None, live_region: None };
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
    /// Container / codec / size / quality settings (the Format dialog).
    format: FormatSettings,
    format_dialog: FormatDialog,
    /// Encoders found on this machine (Media Foundation); empty until the scan finishes.
    encoders: Vec<EncoderInfo>,
    encoder_rx: Option<mpsc::Receiver<Vec<EncoderInfo>>>,
    mouse_fx: SharedFx,
    fx_demo_clicks: Vec<(Instant, bool)>,
    system_audio: bool,
    mic_enabled: bool,
    mic_idx: usize,
    output_dir: PathBuf,
    file_prefix: String,
    countdown_enabled: bool,
    countdown_secs: u32,
    language: Lang,
    /// Look for a newer release on GitHub at start-up.
    check_updates: bool,
    update: updater::UpdateState,
    update_modal: bool,
    tab: Tab,
    home_tab: HomeTab,
    video_tab: VideoTab,
    general_tab: GeneralTab,
    state: State,
    live: LivePreview,
    library: Library,
    preview_tex: Option<TextureHandle>,
    preview_dims: (u32, u32),
    message: Option<(String, bool)>, // (text, is_error)
    message_at: Option<Instant>,
    last_message: Option<String>,
    last_file: Option<PathBuf>,
    /// Compact "mini bar" mode (Camtasia-style floating recorder bar).
    compact: bool,
    /// Outer rect of the full window, restored when leaving compact mode.
    saved_rect: Option<egui::Rect>,
    /// Last known outer position of the mini bar; the region is docked to it
    /// and follows when the user drags the bar.
    bar_anchor: Option<egui::Pos2>,
    /// After we move the bar ourselves, ignore position changes until then.
    bar_settle_until: Option<Instant>,
    /// Whether the region-frame overlay windows have had their DWM styling applied.
    frame_styled: bool,
    /// Whether Windows agreed to keep those windows out of screen captures; the
    /// centre crosshair is only shown while recording when it did.
    frame_excluded: bool,
    /// How many overlay windows the frame last showed; a change means one was
    /// created and still needs styling.
    frame_parts: usize,
    /// Border drag in progress (move or resize of the selected region).
    frame_drag: Option<region_frame::FrameDrag>,
    /// Global pointer used to drive border drags; created on the first one.
    frame_pointer: Option<region_frame::GlobalPointer>,
    /// Inner size of the mini bar window; grows once when localized labels need more room.
    bar_size: Vec2,
    /// App icon texture for the About page (decoded on first use).
    about_icon: Option<TextureHandle>,
    /// Debug aid: `OPENCLIP_START_COMPACT=1` opens straight into the mini bar.
    start_compact: bool,
}

/// `x,y,w,h` in monitor-local physical pixels, for `OPENCLIP_START_REGION`.
fn parse_region(spec: &str) -> Option<Rect> {
    let n: Vec<u32> = spec.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let [x, y, width, height] = n[..] else { return None };
    (width >= 16 && height >= 16).then_some(Rect { x, y, width: width & !1, height: height & !1 })
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        install_fonts(&cc.egui_ctx);
        let monitors = list_monitors().unwrap_or_default();
        let windows = list_windows().unwrap_or_default();
        let mics = list_input_devices();
        let settings = Settings::load();
        let output_dir = settings.output_dir.clone().filter(|d| d.is_dir()).unwrap_or_else(default_output_dir);
        let mic_idx = settings.mic_name.as_ref().and_then(|n| mics.iter().position(|m| m == n)).unwrap_or(0);
        let mut library = Library::new();
        library.refresh(&output_dir, true);
        // Enumerating Media Foundation encoders takes a moment; do it off the GUI thread.
        let (tx, encoder_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("openclip-encoders".into())
            .spawn(move || {
                let _ = tx.send(available_encoders());
            })
            .ok();
        // Debug aid: `OPENCLIP_START_REGION=x,y,w,h` opens with that region already
        // selected on the first monitor, so the border can be screenshotted
        // without driving the picker.
        let start_region = cfg!(debug_assertions)
            .then(|| std::env::var("OPENCLIP_START_REGION").ok())
            .flatten()
            .and_then(|s| parse_region(&s))
            .and_then(|r| Some((monitors.first()?.id, r)));
        let mut app = Self {
            monitors,
            windows,
            mics,
            source_kind: if start_region.is_some() { SourceKind::Region } else { SourceKind::Monitor },
            monitor_idx: 0,
            window_idx: 0,
            region: start_region,
            format: settings.format,
            format_dialog: FormatDialog::new(),
            encoders: Vec::new(),
            encoder_rx: Some(encoder_rx),
            mouse_fx: Arc::new(RwLock::new(settings.mouse_fx)),
            fx_demo_clicks: Vec::new(),
            system_audio: settings.system_audio,
            mic_enabled: settings.mic_enabled,
            mic_idx,
            output_dir,
            file_prefix: settings.file_prefix,
            countdown_enabled: settings.countdown_enabled,
            countdown_secs: settings.countdown_secs.clamp(1, 10),
            language: settings.language,
            check_updates: settings.check_updates,
            update: updater::UpdateState::Idle,
            update_modal: false,
            tab: Tab::Home,
            home_tab: HomeTab::Videos,
            video_tab: VideoTab::Record,
            general_tab: GeneralTab::Output,
            state: State::Idle,
            live: LivePreview::new(),
            library,
            preview_tex: None,
            preview_dims: (0, 0),
            message: None,
            message_at: None,
            last_message: None,
            last_file: None,
            compact: false,
            saved_rect: None,
            bar_anchor: None,
            bar_settle_until: None,
            frame_styled: false,
            frame_excluded: false,
            frame_parts: 0,
            frame_drag: None,
            frame_pointer: None,
            bar_size: minibar::BAR_SIZE,
            about_icon: None,
            start_compact: cfg!(debug_assertions) && std::env::var_os("OPENCLIP_START_COMPACT").is_some(),
        };
        if app.check_updates {
            app.start_update_check(&cc.egui_ctx);
        }
        app
    }

    fn selected_source(&self) -> Option<Source> {
        match self.source_kind {
            SourceKind::Monitor => self.monitors.get(self.monitor_idx).map(|m| Source::Monitor { id: m.id }),
            SourceKind::Window => self.windows.get(self.window_idx).map(|w| Source::Window { id: w.id }),
            SourceKind::Region => self.region.map(|(monitor_id, rect)| Source::Region { monitor_id, rect }),
        }
    }

    /// Pixel size of the selected source, if known.
    pub(super) fn source_size(&self) -> Option<(u32, u32)> {
        let (w, h) = match self.source_kind {
            SourceKind::Monitor => self.monitors.get(self.monitor_idx).map(|m| (m.width, m.height)).unwrap_or((0, 0)),
            SourceKind::Window => self.windows.get(self.window_idx).map(|w| (w.width, w.height)).unwrap_or((0, 0)),
            SourceKind::Region => self.region.map(|(_, r)| (r.width, r.height)).unwrap_or((0, 0)),
        };
        (w > 0 && h > 0).then_some((w, h))
    }

    /// Persists everything the user can change (called on dialog OK, folder change and exit).
    pub(super) fn save_settings(&self) {
        let settings = Settings {
            format: self.format.clone(),
            output_dir: Some(self.output_dir.clone()),
            file_prefix: self.file_prefix.clone(),
            system_audio: self.system_audio,
            mic_enabled: self.mic_enabled,
            mic_name: self.mics.get(self.mic_idx).cloned(),
            mouse_fx: self.mouse_fx.read().unwrap().clone(),
            countdown_enabled: self.countdown_enabled,
            countdown_secs: self.countdown_secs,
            language: self.language,
            check_updates: self.check_updates,
        };
        if let Err(e) = settings.save() {
            log::warn!("could not save settings: {e:#}");
        }
    }

    pub(super) fn open_format_dialog(&mut self, section: FormatSection) {
        self.wait_for_encoders(Duration::from_millis(1500));
        self.format_dialog.open(&self.format, &self.encoders, section);
    }

    /// Runs the Format dialog (both layouts call this last, like the delete dialog).
    fn show_format_dialog(&mut self, ctx: &egui::Context) {
        let recording = self.is_recording();
        match self.format_dialog.show(ctx, &self.encoders, self.source_size(), recording, self.compact) {
            DialogOutcome::Ok(f) => {
                self.format = f;
                self.save_settings();
            }
            DialogOutcome::Cancel | DialogOutcome::None => {}
        }
        if self.format_dialog.take_rescan() {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(refresh_encoders());
            });
            self.encoder_rx = Some(rx);
        }
    }

    /// Picks up the encoder list once the background scan is done.
    fn poll_encoders(&mut self) {
        let Some(rx) = &self.encoder_rx else { return };
        if let Ok(list) = rx.try_recv() {
            self.encoders = list;
            self.encoder_rx = None;
            self.apply_encoder_list();
        }
    }

    /// Blocks briefly for a pending encoder scan (before recording / opening the dialog).
    fn wait_for_encoders(&mut self, timeout: Duration) {
        if let Some(rx) = self.encoder_rx.take() {
            if let Ok(list) = rx.recv_timeout(timeout) {
                self.encoders = list;
                self.apply_encoder_list();
            } else {
                log::warn!("encoder scan did not finish in time");
            }
        }
    }

    fn apply_encoder_list(&mut self) {
        log::info!("encoders: {:?}", self.encoders.iter().map(|e| e.label.as_str()).collect::<Vec<_>>());
        let notes = self.format.normalize(&self.encoders);
        if !notes.is_empty() {
            self.message = Some((notes.join(" "), true));
        }
    }

    fn source_label(&self) -> String {
        match self.source_kind {
            SourceKind::Monitor => {
                self.monitors.get(self.monitor_idx).map(|m| m.label()).unwrap_or_else(|| t!(NO_MONITOR_SELECTED).into())
            }
            SourceKind::Window => {
                self.windows.get(self.window_idx).map(|w| w.label()).unwrap_or_else(|| t!(NO_WINDOW_SELECTED).into())
            }
            SourceKind::Region => match self.region {
                Some((mid, r)) => {
                    let mon =
                        self.monitors.iter().find(|m| m.id == mid).map(|m| m.name.clone()).unwrap_or_default();
                    t!(REGION_LABEL, r.width, r.height, r.x, r.y, mon)
                }
                None => t!(NO_REGION_SELECTED).into(),
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
        self.message = Some((t!(MSG_SAVED, what, path.display(), human_bytes(size)), false));
        self.library.select_path(&path, &self.output_dir);
        if self.tab == Tab::Home && self.home_tab != HomeTab::Preview {
            self.home_tab = HomeTab::from_library(self.library.tab);
        }
        self.last_file = Some(path);
    }

    /// REC pressed: count down first (if enabled), then start.
    fn start_recording(&mut self, ctx: &egui::Context) {
        if self.selected_source().is_none() {
            self.message = Some((t!(MSG_SELECT_SOURCE_FIRST).into(), true));
            return;
        }
        if self.countdown_enabled && self.countdown_secs > 0 && !self.is_recording() {
            self.state = State::Countdown { started: Instant::now() };
            self.message = None;
            ctx.request_repaint();
            return;
        }
        self.start_recording_now(ctx);
    }

    fn cancel_countdown(&mut self) {
        if matches!(self.state, State::Countdown { .. }) {
            self.state = State::Idle;
        }
    }

    /// Seconds still to count down (rounded up), if counting.
    fn countdown_remaining(&self) -> Option<u32> {
        match &self.state {
            State::Countdown { started } => {
                let left = self.countdown_secs as f32 - started.elapsed().as_secs_f32();
                Some(left.ceil().max(0.0) as u32)
            }
            _ => None,
        }
    }

    /// Advances the countdown; starts recording when it reaches zero.
    fn tick_countdown(&mut self, ctx: &egui::Context) {
        let State::Countdown { started } = &self.state else { return };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_countdown();
            self.message = Some((t!(MSG_CANCELLED).into(), false));
            return;
        }
        if started.elapsed().as_secs_f32() >= self.countdown_secs as f32 {
            // Start before this frame is drawn so the overlay never reaches the file.
            self.start_recording_now(ctx);
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn start_recording_now(&mut self, ctx: &egui::Context) {
        let Some(source) = self.selected_source() else {
            self.state = State::Idle;
            self.message = Some((t!(MSG_SELECT_SOURCE_FIRST).into(), true));
            return;
        };
        self.state = State::Idle;
        // Release the preview capture before opening the recording session.
        self.live.stop();
        self.wait_for_encoders(Duration::from_secs(2));
        let mut format = self.format.clone();
        let notes = format.normalize(&self.encoders);
        if !notes.is_empty() {
            log::warn!("format settings adjusted: {}", notes.join(" "));
        }
        let output = self.timestamped(format.container.extension());
        let config = RecordConfig {
            source,
            format,
            mouse_fx: self.mouse_fx.read().unwrap().clone(),
            system_audio: self.system_audio,
            microphone: self.mic_enabled.then(|| self.mics.get(self.mic_idx).cloned()),
            output,
        };
        let ctx2 = ctx.clone();
        let cb: crate::pipeline::PreviewCallback = Arc::new(move || ctx2.request_repaint());
        match Recorder::start(config, Some(cb)) {
            Ok(rec) => {
                self.message = None;
                self.state = State::Recording(rec);
            }
            Err(e) => self.message = Some((t!(MSG_START_FAILED, format!("{e:#}")), true)),
        }
    }

    fn stop_recording(&mut self) {
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            match rec.stop() {
                Ok(path) => self.saved(path, t!(WHAT_RECORDING)),
                Err(e) => self.message = Some((t!(MSG_RECORDING_FAILED, format!("{e:#}")), true)),
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
            self.message = Some((t!(MSG_SELECT_CAPTURE_FIRST).into(), true));
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
            Ok(()) => self.saved(path, t!(WHAT_SNAPSHOT)),
            Err(e) => self.message = Some((t!(MSG_SNAPSHOT_FAILED, format!("{e:#}")), true)),
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
        let preview_visible = self.tab == Tab::Home && self.home_tab == HomeTab::Preview && !self.compact;
        match &self.state {
            State::Recording(rec) => {
                rec.set_preview_visible(preview_visible);
                if let Some(img) = rec.preview().take() {
                    self.upload_preview(ctx, &img);
                }
            }
            State::Idle | State::Countdown { .. } => {
                if preview_visible {
                    // Restarting the backend on every mouse-move of a border
                    // drag would thrash WGC; pick the new rect up on release.
                    if self.frame_drag.is_none() {
                        self.live.ensure(self.selected_source(), &self.mouse_fx, ctx);
                    }
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

    // ----- header ------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let recording = self.is_recording();
        let paused = matches!(&self.state, State::Recording(r) if r.is_paused());
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.add_enabled_ui(!recording, |ui| {
                let mut kind = self.source_kind;
                let items = [
                    (SourceKind::Region, Some(icons::REGION), t!(MODE_REGION)),
                    (SourceKind::Monitor, Some(icons::MONITOR), t!(MODE_MONITOR)),
                    (SourceKind::Window, Some(icons::WINDOW), t!(MODE_WINDOW)),
                ];
                if segmented(ui, "mode", &items, &mut kind) {
                    self.select_mode(kind);
                }
                ui.add_space(12.0);
                icon_toggle(ui, icons::SPEAKER, t!(SYSTEM_AUDIO), &mut self.system_audio);
                icon_toggle(ui, icons::MIC, t!(MICROPHONE), &mut self.mic_enabled);
                let mut show_cursor = self.mouse_fx.read().unwrap().show_cursor;
                let before = show_cursor;
                icon_toggle(ui, icons::CURSOR, t!(SHOW_CURSOR), &mut show_cursor);
                if show_cursor != before {
                    self.mouse_fx.write().unwrap().show_cursor = show_cursor;
                }
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if icon_button(ui, icons::CAMERA, t!(TIP_SNAPSHOT)).clicked() {
                    self.take_snapshot();
                }
                if icon_button(ui, icons::MINIMIZE, t!(TIP_MINIBAR)).clicked() {
                    self.enter_compact(ctx);
                }
                ui.add_space(8.0);
                let can_record = self.selected_source().is_some();
                match rec_button(ui, self.rec_mode(), can_record) {
                    RecClick::Start => self.start_recording(ctx),
                    RecClick::Stop => self.stop_recording(),
                    RecClick::Cancel => self.cancel_countdown(),
                    RecClick::None => {}
                }
                if pause_button(ui, recording, paused) {
                    self.toggle_pause();
                }
            });
        });
    }

    pub(super) fn rec_mode(&self) -> RecMode {
        match &self.state {
            State::Recording(_) => RecMode::Recording,
            State::Countdown { .. } => RecMode::Countdown(self.countdown_remaining().unwrap_or(0)),
            _ => RecMode::Idle,
        }
    }

    /// Big centred "3 … 2 … 1" while the countdown runs (full-window layout).
    fn countdown_overlay(&mut self, ctx: &egui::Context) {
        let Some(left) = self.countdown_remaining() else { return };
        let mut cancel = false;
        egui::Area::new(egui::Id::new("countdown-overlay"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                sheet_frame().corner_radius(CornerRadius::same(20)).inner_margin(Margin::symmetric(44, 28)).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(secondary(t!(COUNTDOWN_TITLE)).size(15.0));
                        ui.label(RichText::new(left.max(1).to_string()).font(semibold(96.0)).color(LABEL));
                        ui.add_space(8.0);
                        if tinted_button(ui, t!(COUNTDOWN_CANCEL)).clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if cancel {
            self.cancel_countdown();
        }
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
            Err(e) => self.message = Some((t!(MSG_PICKER_FAILED, format!("{e:#}")), true)),
        }
    }

    fn status_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            match &self.state {
                State::Recording(rec) => {
                    let s = rec.stats();
                    let elapsed = rec.elapsed();
                    let encoded = s.frames_encoded.load(Ordering::Relaxed);
                    let dropped = s.frames_dropped.load(Ordering::Relaxed);
                    let bytes = s.bytes_written.load(Ordering::Relaxed);
                    let (w, h) = (s.width.load(Ordering::Relaxed), s.height.load(Ordering::Relaxed));
                    let fps = if elapsed.as_secs_f64() > 0.5 { encoded as f64 / elapsed.as_secs_f64() } else { 0.0 };
                    let timer_w = capsule_width_for(ui, "‖  00:00:00");
                    if rec.is_paused() {
                        status_capsule(ui, Tint::Orange, &format!("‖  {}", format_duration(elapsed)), Some(timer_w), None, None);
                    } else {
                        status_capsule(ui, Tint::Red, &format!("●  {}", format_duration(elapsed)), Some(timer_w), None, None);
                    }
                    ui.add_space(4.0);
                    ui.label(secondary(t!(STATUS_COUNTERS, w, h, format!("{fps:.1}"), dropped, human_bytes(bytes))))
                        .on_hover_ui(|ui| counters_tooltip(ui, s, elapsed));
                    // A slot costing more than its budget is why the frame rate
                    // is low; show it while it lasts rather than as a sticky note.
                    let budget_us = 1_000_000 / s.target_fps.load(Ordering::Relaxed).max(1);
                    let slot_us = s.slot_us.load(Ordering::Relaxed);
                    if elapsed.as_secs_f64() > 2.0 && slot_us > budget_us {
                        vdivider(ui, 16.0);
                        let behind = t!(STATUS_ENCODER_BEHIND, format!("{:.1}", slot_us as f64 / 1e3), budget_us / 1000);
                        ui.label(RichText::new(behind).color(ORANGE));
                    }
                    for note in [s.note(), s.audio_note.lock().unwrap().clone()].into_iter().flatten() {
                        vdivider(ui, 16.0);
                        ui.label(RichText::new(note).color(ORANGE));
                    }
                    if s.error().is_some() || rec.is_finished() {
                        self.stop_recording();
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(250));
                    }
                }
                State::Picking(_) => {
                    status_capsule(ui, Tint::Blue, &format!("{}  {}", icons::REGION, t!(MODE_REGION)), None, None, None);
                    ui.label(secondary(t!(STATUS_PICKING)));
                }
                State::Countdown { .. } => {
                    let left = self.countdown_remaining().unwrap_or(0).max(1);
                    status_capsule(ui, Tint::Orange, &t!(BAR_STARTING_IN, left), None, None, None);
                    ui.label(secondary(t!(COUNTDOWN_ESC)));
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                State::Idle => {
                    let ready = self.selected_source().is_some();
                    if ready {
                        status_capsule(ui, Tint::Green, t!(BAR_READY), None, None, None);
                        ui.label(secondary(self.source_label()));
                    } else {
                        status_capsule(ui, Tint::Gray, t!(BAR_PICK_SOURCE), None, None, None);
                        ui.label(secondary(t!(STATUS_NO_SOURCE)));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(4.0);
                        if tinted_button_small(ui, t!(STATUS_CHANGE)).on_hover_text(t!(STATUS_CHANGE_TIP)).clicked() {
                            self.show_preview_tab();
                        }
                        self.update_chip(ui);
                    });
                }
            }
        });
    }

    // ----- navigation + pages --------------------------------------------------

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        for (tab, icon, tint, label) in [
            (Tab::Home, icons::HOME, BLUE, t!(NAV_HOME)),
            (Tab::General, icons::GEAR, LABEL_2, t!(NAV_GENERAL)),
            (Tab::Video, icons::FILM, ORANGE, t!(NAV_VIDEO)),
            (Tab::Image, icons::IMAGE, GREEN, t!(NAV_IMAGE)),
            (Tab::About, icons::INFO, BLUE, t!(NAV_ABOUT)),
        ] {
            if nav_entry(ui, icon, tint, label, self.tab == tab).clicked() {
                self.tab = tab;
            }
        }
    }

    fn page(&mut self, ui: &mut egui::Ui) {
        match self.tab {
            Tab::Home => self.page_home(ui),
            _ => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(760.0));
                    match self.tab {
                        Tab::Home => {}
                        Tab::General => self.page_general(ui),
                        Tab::Video => self.page_video(ui),
                        Tab::Image => self.page_image(ui),
                        Tab::About => self.page_about(ui),
                    }
                    ui.add_space(12.0);
                });
            }
        }
    }

    // ----- Home: Videos | Images | Audios | Preview -----------------------------

    fn page_home(&mut self, ui: &mut egui::Ui) {
        let mut home_tab = self.home_tab;
        segmented(
            ui,
            "home",
            &[
                (HomeTab::Videos, None, t!(TAB_VIDEOS)),
                (HomeTab::Images, None, t!(TAB_IMAGES)),
                (HomeTab::Audios, None, t!(TAB_AUDIOS)),
                (HomeTab::Preview, None, t!(TAB_PREVIEW)),
            ],
            &mut home_tab,
        );
        if home_tab != self.home_tab {
            self.home_tab = home_tab;
            if let Some(lib) = home_tab.library() {
                self.library.set_tab(lib, &self.output_dir);
            }
        }
        ui.add_space(8.0);
        match self.home_tab {
            HomeTab::Preview => {
                let recording = self.is_recording();
                self.source_row(ui, recording);
                self.preview_panel(ui);
            }
            _ => self.library_panel(ui),
        }
    }

    fn library_panel(&mut self, ui: &mut egui::Ui) {
        self.library.refresh(&self.output_dir, false);
        // Folder row.
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            let w = (ui.available_width() - 96.0).max(60.0);
            ui.allocate_ui_with_layout(Vec2::new(w, 34.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(secondary(self.output_dir.display().to_string())).truncate())
                    .on_hover_text(t!(OUTPUT_FOLDER_TIP));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icons::REFRESH, t!(REFRESH_LIST)).clicked() {
                    self.library.refresh(&self.output_dir, true);
                }
                if icon_button(ui, icons::FOLDER, t!(OPEN_FOLDER)).clicked() {
                    open_folder(&self.output_dir);
                }
            });
        });

        // File list.
        let list_h = (ui.available_height() - 60.0).max(80.0);
        let mut clicked: Option<usize> = None;
        let mut activated: Option<usize> = None;
        let entries = &self.library.entries;
        let selected_idx = self.library.selected;
        let empty_label = self.library.tab.empty_label();
        Card::show(ui, |card| {
            card.flush(|ui| {
                ui.set_min_height(list_h);
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(4.0);
                    if entries.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.label(secondary(empty_label));
                        });
                    }
                    for (i, e) in entries.iter().enumerate() {
                        let selected = selected_idx == Some(i);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
                        let p = ui.painter();
                        if i > 0 {
                            p.hline((rect.left() + PAD)..=rect.right(), rect.top(), Stroke::new(1.0, SEPARATOR));
                        }
                        let pill = rect.shrink2(Vec2::new(6.0, 2.0));
                        if selected {
                            p.rect_filled(pill, CornerRadius::same(8), BLUE);
                        } else if resp.hovered() {
                            p.rect_filled(pill, CornerRadius::same(8), FILL_HOVER);
                        }
                        let (name_color, size_color) = if selected { (Color32::WHITE, Color32::WHITE) } else { (LABEL, LABEL_2) };
                        let name_rect = egui::Rect::from_min_max(rect.min + Vec2::new(PAD, 0.0), rect.max - Vec2::new(96.0, 0.0));
                        p.with_clip_rect(name_rect).text(
                            name_rect.left_center(),
                            Align2::LEFT_CENTER,
                            &e.name,
                            FontId::proportional(13.0),
                            name_color,
                        );
                        p.text(
                            rect.right_center() - Vec2::new(PAD, 0.0),
                            Align2::RIGHT_CENTER,
                            human_bytes(e.size),
                            FontId::proportional(12.0),
                            size_color,
                        );
                        if resp.double_clicked() {
                            activated = Some(i);
                        } else if resp.clicked() {
                            clicked = Some(i);
                        }
                        resp.on_hover_text(e.path.display().to_string());
                    }
                    ui.add_space(4.0);
                });
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
        let selected = self.library.selected_entry().map(|e| e.path.clone());
        ui.horizontal(|ui| {
            let has = selected.is_some();
            ui.add_enabled_ui(has, |ui| {
                if tinted_button(ui, &format!("{}  {}", icons::PLAY, t!(PLAY))).clicked()
                    && let Some(p) = &selected
                {
                    open_with_default(p);
                }
                if tinted_button(ui, &format!("{}  {}", icons::FOLDER, t!(FOLDER))).clicked()
                    && let Some(p) = &selected
                {
                    reveal_in_folder(p);
                }
                if destructive_tinted_button(ui, &format!("{}  {}", icons::TRASH, t!(DELETE))).clicked() {
                    self.library.confirm_delete = selected.clone();
                }
            });
        });
    }

    fn source_row(&mut self, ui: &mut egui::Ui, recording: bool) {
        ui.add_enabled_ui(!recording, |ui| {
            Card::show(ui, |card| {
                let label = match self.source_kind {
                    SourceKind::Monitor => t!(MODE_MONITOR),
                    SourceKind::Window => t!(MODE_WINDOW),
                    SourceKind::Region => t!(MODE_REGION),
                };
                card.row(label, |ui| {
                    if icon_button(ui, icons::REFRESH, t!(REFRESH_SOURCES_TIP)).clicked() {
                        self.refresh_sources();
                    }
                    let combo_w = (ui.available_width() - 40.0).clamp(160.0, 360.0);
                    match self.source_kind {
                        SourceKind::Monitor => {
                            let label = self
                                .monitors
                                .get(self.monitor_idx)
                                .map(|m| m.label())
                                .unwrap_or_else(|| t!(NO_MONITORS).into());
                            egui::ComboBox::from_id_salt("monitor").width(combo_w).selected_text(label).show_ui(ui, |ui| {
                                for (i, m) in self.monitors.iter().enumerate() {
                                    ui.selectable_value(&mut self.monitor_idx, i, m.label());
                                }
                            });
                        }
                        SourceKind::Window => {
                            let label = self
                                .windows
                                .get(self.window_idx)
                                .map(|w| w.label())
                                .unwrap_or_else(|| t!(NO_WINDOWS).into());
                            egui::ComboBox::from_id_salt("window").width(combo_w).selected_text(label).show_ui(ui, |ui| {
                                for (i, w) in self.windows.iter().enumerate() {
                                    ui.selectable_value(&mut self.window_idx, i, w.label());
                                }
                            });
                        }
                        SourceKind::Region => {
                            if tinted_button_small(ui, t!(SELECT_REGION)).clicked() {
                                self.open_picker();
                            }
                            ui.add(egui::Label::new(secondary(self.source_label())).truncate());
                        }
                    }
                });
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        egui::Frame::new().fill(PREVIEW_BG).corner_radius(CornerRadius::same(12)).show(ui, |ui| {
            ui.set_min_size(avail);
            match &self.preview_tex {
                Some(tex) if self.preview_dims.0 > 0 => {
                    let (w, h) = (self.preview_dims.0 as f32, self.preview_dims.1 as f32);
                    let scale = ((avail.x - 16.0) / w).min((avail.y - 16.0) / h).min(3.0);
                    let size = egui::vec2(w * scale, h * scale);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::from_texture(&*tex).fit_to_exact_size(size).corner_radius(6.0));
                    });
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        let text = match (&self.live.error, self.selected_source()) {
                            (Some(e), _) => t!(PREVIEW_UNAVAILABLE, e),
                            (None, None) => t!(PREVIEW_PICK_SOURCE).into(),
                            (None, Some(_)) => t!(PREVIEW_STARTING).into(),
                        };
                        ui.label(secondary(text).size(15.0));
                    });
                }
            }
        });
    }

    // ----- settings pages ---------------------------------------------------------

    fn page_general(&mut self, ui: &mut egui::Ui) {
        let mut tab = self.general_tab;
        segmented(
            ui,
            "general",
            &[
                (GeneralTab::Output, None, t!(SECTION_OUTPUT)),
                (GeneralTab::Recording, None, t!(SECTION_RECORDING)),
                (GeneralTab::Appearance, None, t!(SECTION_APPEARANCE)),
                (GeneralTab::Sources, None, t!(SECTION_SOURCES)),
            ],
            &mut tab,
        );
        self.general_tab = tab;
        ui.add_space(4.0);
        match tab {
            GeneralTab::Output => {
                section_header(ui, t!(SECTION_OUTPUT));
                Card::show(ui, |card| {
                    card.row(t!(ROW_SAVE_TO), |ui| {
                        if tinted_button_small(ui, t!(OPEN)).clicked() {
                            open_folder(&self.output_dir);
                        }
                        if tinted_button_small(ui, t!(CHOOSE_FOLDER)).clicked()
                            && let Some(dir) = rfd::FileDialog::new().set_directory(&self.output_dir).pick_folder()
                        {
                            self.output_dir = dir;
                            self.library.refresh(&self.output_dir, true);
                            self.save_settings();
                        }
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(secondary(self.output_dir.display().to_string())).truncate());
                    });
                    card.row(t!(ROW_FILE_PREFIX), |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.file_prefix).desired_width(180.0));
                    });
                });
                footnote(
                    ui,
                    &format!("→ {}-YYYYMMDD-HHMMSS.{}", self.file_prefix.trim(), self.format.container.extension()),
                );
            }
            GeneralTab::Recording => {
                section_header(ui, t!(SECTION_RECORDING));
                let before = (self.countdown_enabled, self.countdown_secs);
                Card::show(ui, |card| {
                    switch_row(card, t!(COUNTDOWN_CHECKBOX), &mut self.countdown_enabled);
                    card.row(t!(ROW_COUNTDOWN), |ui| {
                        ui.add_enabled_ui(self.countdown_enabled, |ui| {
                            ui.add(egui::DragValue::new(&mut self.countdown_secs).range(1..=10).suffix(" s"));
                        });
                    });
                });
                footnote(ui, t!(COUNTDOWN_NOTE));
                if before != (self.countdown_enabled, self.countdown_secs) {
                    self.save_settings();
                }
            }
            GeneralTab::Appearance => {
                section_header(ui, t!(SECTION_APPEARANCE));
                Card::show(ui, |card| {
                    card.row(t!(LANGUAGE), |ui| {
                        let mut lang = self.language;
                        egui::ComboBox::from_id_salt("language")
                            .width(180.0)
                            .selected_text(lang.native_name())
                            .show_ui(ui, |ui| {
                                for l in Lang::ALL {
                                    ui.selectable_value(&mut lang, l, l.native_name());
                                }
                            });
                        if lang != self.language {
                            self.set_language(lang);
                        }
                    });
                });
                footnote(ui, t!(LANGUAGE_HINT));
            }
            GeneralTab::Sources => {
                section_header(ui, t!(SECTION_SOURCES));
                Card::show(ui, |card| {
                    card.row(t!(ROW_DEVICES), |ui| {
                        if tinted_button_small(ui, t!(REFRESH_DEVICES)).clicked() {
                            self.refresh_sources();
                        }
                    });
                    let path =
                        Settings::path().map(|p| p.display().to_string()).unwrap_or_else(|| t!(NONE_PAREN).into());
                    card.text_row(t!(ROW_SETTINGS_FILE), &path);
                });
            }
        }
    }

    /// Switches the interface language and persists the choice.
    fn set_language(&mut self, lang: Lang) {
        self.language = lang;
        i18n::set_lang(lang);
        self.save_settings();
    }

    fn page_video(&mut self, ui: &mut egui::Ui) {
        let mut vt = self.video_tab;
        segmented(ui, "video", &[(VideoTab::Record, None, t!(TAB_RECORD)), (VideoTab::Mouse, None, t!(TAB_MOUSE))], &mut vt);
        self.video_tab = vt;
        ui.add_space(4.0);
        match self.video_tab {
            VideoTab::Record => self.video_record_tab(ui),
            VideoTab::Mouse => self.video_mouse_tab(ui),
        }
    }

    fn video_record_tab(&mut self, ui: &mut egui::Ui) {
        let recording = self.is_recording();
        section_header(ui, t!(BOX_AUDIO));
        ui.add_enabled_ui(!recording, |ui| {
            Card::show(ui, |card| {
                switch_row(card, t!(CHK_SYSTEM_AUDIO), &mut self.system_audio);
                switch_row(card, t!(CHK_MICROPHONE), &mut self.mic_enabled);
                card.row(t!(ROW_DEVICE), |ui| {
                    ui.add_enabled_ui(self.mic_enabled && !self.mics.is_empty(), |ui| {
                        let label = self.mics.get(self.mic_idx).cloned().unwrap_or_else(|| t!(NO_INPUT_DEVICES).into());
                        let w = (ui.available_width() - 8.0).clamp(160.0, 360.0);
                        egui::ComboBox::from_id_salt("mic").width(w).selected_text(label).show_ui(ui, |ui| {
                            for (i, m) in self.mics.iter().enumerate() {
                                ui.selectable_value(&mut self.mic_idx, i, m);
                            }
                        });
                    });
                });
            });
        });

        section_header(ui, &t!(SECTION_FORMAT, self.format.container.label()));
        let (video_title, video_detail) = self.format.video_summary(&self.encoders, self.source_size());
        let (audio_title, audio_detail) = self.format.audio_summary(self.audio_sources_label());
        let mut open: Option<FormatSection> = None;
        ui.add_enabled_ui(!recording, |ui| {
            Card::show(ui, |card| {
                if card.nav_row(t!(BOX_VIDEO), &format!("{video_title} · {video_detail}")).clicked() {
                    open = Some(FormatSection::Video);
                }
                if card.nav_row(t!(BOX_AUDIO), &format!("{audio_title} · {audio_detail}")).clicked() {
                    open = Some(FormatSection::Audio);
                }
            });
        });
        if let Some(section) = open
            && !recording
        {
            self.open_format_dialog(section);
        }
        let note = if self.encoder_rx.is_some() {
            format!("{} · {}", t!(FORMAT_SETTINGS_TIP), t!(SCANNING_ENCODERS))
        } else {
            t!(FORMAT_SETTINGS_TIP).to_string()
        };
        footnote(ui, &note);
    }

    pub(super) fn audio_sources_label(&self) -> &'static str {
        match (self.system_audio, self.mic_enabled) {
            (true, true) => t!(SRC_SYSTEM_AND_MIC),
            (true, false) => t!(SRC_SYSTEM),
            (false, true) => t!(SRC_MIC),
            (false, false) => t!(SRC_NO_AUDIO),
        }
    }

    fn video_mouse_tab(&mut self, ui: &mut egui::Ui) {
        let current = self.mouse_fx.read().unwrap().clone();
        let mut fx = current.clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(400.0);
                section_header(ui, t!(SECTION_MOUSE_FX));
                Card::show(ui, |card| {
                    switch_row(card, t!(CHK_SHOW_CURSOR), &mut fx.show_cursor);
                    card.row(t!(ROW_SIZE_INDENT).trim(), |ui| {
                        ui.add_enabled_ui(fx.show_cursor, |ui| {
                            size_combo(ui, "cursor_size", &mut fx.cursor_size);
                            if fx.cursor_size != 100 {
                                ui.label(secondary(t!(APP_DRAWN)).small());
                            }
                        });
                    });
                });
                Card::show(ui, |card| {
                    switch_row(card, t!(CHK_CLICK_EFFECT), &mut fx.click_effect);
                    card.row(t!(ROW_SIZE_INDENT).trim(), |ui| {
                        ui.add_enabled_ui(fx.click_effect, |ui| size_combo(ui, "click_size", &mut fx.click_size));
                    });
                    card.row(t!(ROW_LEFT_CLICK_COLOR).trim(), |ui| {
                        ui.add_enabled_ui(fx.click_effect, |ui| color_swatch(ui, &mut fx.left_color));
                    });
                    card.row(t!(ROW_RIGHT_CLICK_COLOR).trim(), |ui| {
                        ui.add_enabled_ui(fx.click_effect, |ui| color_swatch(ui, &mut fx.right_color));
                    });
                });
                Card::show(ui, |card| {
                    switch_row(card, t!(CHK_HIGHLIGHT), &mut fx.highlight);
                    card.row(t!(ROW_SIZE_INDENT).trim(), |ui| {
                        ui.add_enabled_ui(fx.highlight, |ui| size_combo(ui, "highlight_size", &mut fx.highlight_size));
                    });
                    card.row(t!(ROW_HIGHLIGHT_COLOR).trim(), |ui| {
                        ui.add_enabled_ui(fx.highlight, |ui| color_swatch(ui, &mut fx.highlight_color));
                    });
                    card.row(t!(ROW_OPACITY).trim(), |ui| {
                        ui.add_enabled_ui(fx.highlight, |ui| {
                            ui.add(egui::DragValue::new(&mut fx.highlight_opacity).range(0..=100).suffix(" %"));
                            ui.add(egui::Slider::new(&mut fx.highlight_opacity, 0..=100).show_value(false));
                        });
                    });
                });
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.set_width(224.0);
                section_header(ui, t!(TAB_PREVIEW));
                Card::show(ui, |card| {
                    card.custom(|ui| {
                        self.fx_preview(ui, &fx);
                        ui.label(secondary(t!(FX_PREVIEW_HINT)).small());
                    });
                });
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
        section_header(ui, t!(SECTION_SNAPSHOT));
        Card::show(ui, |card| {
            card.text_row(t!(BOX_IMAGE), &format!("PNG · {}", t!(SNAPSHOT_DETAIL)));
            card.row(t!(ROW_CAPTURE), |ui| {
                if tinted_button_small(ui, &format!("{}  {}", icons::CAMERA, t!(TAKE_SNAPSHOT_NOW))).clicked() {
                    self.take_snapshot();
                }
            });
            card.text_row(t!(ROW_SOURCE), &self.source_label());
        });
    }

    fn page_about(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        let icon = self.about_icon(ui.ctx());
        ui.vertical_centered(|ui| {
            if let Some(tex) = icon {
                ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(Vec2::splat(72.0)).corner_radius(16.0));
            }
            ui.add_space(8.0);
            ui.label(heading("openclip"));
            ui.label(secondary(t!(ABOUT_VERSION, env!("CARGO_PKG_VERSION"))));
        });
        ui.add_space(16.0);
        section_header(ui, t!(SECTION_UPDATES));
        self.about_update_rows(ui);
        ui.add_space(6.0);
        Card::show(ui, |card| {
            card.custom(|ui| {
                ui.label(RichText::new(t!(ABOUT_TAGLINE)).color(LABEL));
                ui.label(secondary(t!(ABOUT_VIDEO)));
                ui.label(secondary(t!(ABOUT_AUDIO)));
            });
            card.custom(|ui| {
                ui.label(secondary(t!(ABOUT_LICENSE)).small());
            });
            card.row_inline("", |ui| {
                ui.hyperlink_to("github.com/catalingrigoriev285/openclip", "https://github.com/catalingrigoriev285/openclip");
            });
        });
    }

    /// The app icon as a texture (About page), decoded once.
    fn about_icon(&mut self, ctx: &egui::Context) -> Option<TextureHandle> {
        if self.about_icon.is_none() {
            let icon = eframe::icon_data::from_png_bytes(APP_ICON_PNG).ok()?;
            let image = ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            self.about_icon = Some(ctx.load_texture("about-icon", image, TextureOptions::LINEAR));
        }
        self.about_icon.clone()
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let w = (ui.available_width() - 80.0).max(60.0);
            ui.allocate_ui_with_layout(Vec2::new(w, 26.0), Layout::left_to_right(Align::Center), |ui| {
                match &self.message {
                    Some((msg, is_err)) => {
                        let color = if *is_err { RED } else { GREEN };
                        if !*is_err
                            && let Some(path) = self.last_file.clone()
                            && tinted_button_small(ui, t!(OPEN_FOLDER)).clicked()
                        {
                            reveal_in_folder(&path);
                        }
                        ui.add(egui::Label::new(RichText::new(msg).color(color).small()).truncate());
                    }
                    None => {
                        ui.label(secondary(t!(FOOTER_IDLE)).small());
                    }
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                ui.label(secondary(format!("v{}", env!("CARGO_PKG_VERSION"))).small());
            });
        });
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = self.library.confirm_delete.clone() else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-delete")).frame(sheet_frame()).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(heading(t!(DELETE_TITLE)));
            ui.add_space(6.0);
            ui.label(RichText::new(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).color(LABEL));
            ui.label(secondary(t!(DELETE_BODY)));
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if destructive_button(ui, t!(DELETE)).clicked() {
                    match self.library.delete(&path, &self.output_dir) {
                        Ok(()) => self.message = Some((t!(MSG_DELETED, path.display()), false)),
                        Err(e) => self.message = Some((t!(MSG_DELETE_FAILED, e), true)),
                    }
                    close = true;
                }
                if gray_button(ui, t!(CANCEL)).clicked() {
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
        self.poll_encoders();
        self.poll_update(&ctx);
        self.tick_countdown(&ctx);
        self.poll_preview(&ctx);
        self.track_message();
        if std::mem::take(&mut self.start_compact) {
            self.enter_compact(&ctx);
        }

        if let State::Picking(picker) = &mut self.state {
            match picker.show(&ctx) {
                PickerOutcome::Pending => {}
                PickerOutcome::Selected(monitor_id, rect) => {
                    self.region = Some((monitor_id, rect));
                    self.source_kind = SourceKind::Region;
                    self.state = State::Idle;
                    // Like other recorders: collapse to the mini bar next to the
                    // area and show the border around it.
                    if !self.compact {
                        self.enter_compact(&ctx);
                    }
                    self.place_bar_near_region(&ctx);
                }
                PickerOutcome::Cancelled => self.state = State::Idle,
            }
        }

        if self.compact && !self.intercept_close(&ctx) {
            self.follow_bar(&ctx);
            self.region_frame(&ctx);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 0)))
                .show(ui, |ui| self.minibar(ui, &ctx));
            self.delete_dialog(&ctx);
            self.show_format_dialog(&ctx);
            return;
        }

        // Not compact: closes the border viewports (and drops any drag state) if
        // the mini bar was just dismissed.
        self.region_frame(&ctx);

        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 10)))
            .show(ui, |ui| self.toolbar(ui, &ctx));
        egui::Panel::top("status")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 6)))
            .show(ui, |ui| self.status_strip(ui, &ctx));
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(12, 5)))
            .show(ui, |ui| self.footer(ui));
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(200.0)
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(20)))
            .show(ui, |ui| self.page(ui));

        self.countdown_overlay(&ctx);
        self.delete_dialog(&ctx);
        self.show_format_dialog(&ctx);
        self.update_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        self.cancel_update_download();
        self.save_settings();
        self.live.stop();
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            let _ = rec.stop();
        }
    }
}

// ----- small widgets kept here -------------------------------------------------------

/// Percent size selector used by the mouse-effect settings.
/// Breakdown behind the one-line counters: which encoder is running, where every
/// slot went, and what a slot costs against its budget. "Screen updates" is the
/// only number that reveals a *capture* shortfall — repeated frames hide it from
/// the frame rate, because a repeat is encoded and written like any other frame.
fn counters_tooltip(ui: &mut egui::Ui, s: &Stats, elapsed: Duration) {
    let secs = elapsed.as_secs_f64().max(0.001);
    let get = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    let target = get(&s.target_fps).max(1);
    let budget_us = 1_000_000 / target;
    let ms = |us: u64| format!("{:.1} ms", us as f64 / 1e3);

    egui::Grid::new("rec-counters").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        let mut row = |label: String, value: String| {
            ui.label(secondary(label));
            ui.label(value);
            ui.end_row();
        };
        if let Some(enc) = s.encoder() {
            row(t!(COUNTER_ENCODER).to_string(), enc);
        }
        row(t!(COUNTER_SCREEN_FPS).to_string(), format!("{:.1} fps", get(&s.frames_captured) as f64 / secs));
        row(
            t!(COUNTER_FILE_FPS).to_string(),
            format!("{:.1} / {target} fps", get(&s.frames_encoded) as f64 / secs),
        );
        row(t!(COUNTER_CAPTURED).to_string(), get(&s.frames_captured).to_string());
        row(t!(COUNTER_ENCODED).to_string(), get(&s.frames_encoded).to_string());
        row(t!(COUNTER_REPEATED).to_string(), get(&s.frames_repeated).to_string());
        row(t!(COUNTER_SUPERSEDED).to_string(), get(&s.frames_superseded).to_string());
        row(t!(COUNTER_DROPPED).to_string(), get(&s.frames_dropped).to_string());
        row(t!(COUNTER_SLOTS_SKIPPED).to_string(), get(&s.slots_skipped).to_string());
        row(t!(COUNTER_SKIPPED).to_string(), get(&s.frames_skipped).to_string());
        row(t!(COUNTER_ENCODE_MS).to_string(), ms(get(&s.encode_us)));
        row(t!(COUNTER_MUX_MS).to_string(), ms(get(&s.mux_us)));
        row(
            t!(COUNTER_SLOT_MS).to_string(),
            t!(COUNTER_OF_BUDGET, format!("{:.1}", get(&s.slot_us) as f64 / 1e3), budget_us / 1000),
        );
    });
}

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
    ui.spacing_mut().interact_size = Vec2::new(64.0, 26.0);
    egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque);
    *rgb = [c.r(), c.g(), c.b()];
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

pub(super) fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub(super) fn human_bytes(b: u64) -> String {
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
