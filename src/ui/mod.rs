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
mod thumbs;
mod updater;
mod viewer;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

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
use crate::video::watermark::{Corner, Watermark, WatermarkRenderer};
use openclip_overlay::FpsOverlay;

use format_dialog::{DialogOutcome, FormatDialog, FormatSection};
use library::{reveal_in_folder, Library, LibraryTab};
use picker::{Picker, PickerOutcome};
use thumbs::Thumbs;
use viewer::Viewer;
use theme::*;
use widgets::*;

type SharedFx = Arc<RwLock<MouseFx>>;
type SharedWatermark = Arc<RwLock<Watermark>>;

/// Size of the full window. It opens at its minimum, so the app takes as
/// little of the screen as it can while every page still fits.
pub const WINDOW_SIZE: Vec2 = Vec2::new(820.0, 600.0);

/// Application icon shown on the About page (same artwork as the window icon).
const APP_ICON_PNG: &[u8] = crate::video::watermark::LOGO_PNG;

/// Height of one library row: the poster tile plus a little air.
const ROW_H: f32 = 70.0;
/// Poster tile in a library row, 16:9 so landscape recordings fill it.
const THUMB_W: f32 = 96.0;
const THUMB_H: f32 = 54.0;
/// Mouse tab: settings column, gap and preview column widths. Below their sum
/// the two are stacked instead of placed side by side.
const FX_SETTINGS_W: f32 = 400.0;
const FX_GAP: f32 = 16.0;
const FX_PREVIEW_W: f32 = 224.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Region,
    Monitor,
    Window,
    /// Whatever game openclip's hook is currently loaded into. Unlike the
    /// others this is not picked from a list — you cannot alt-tab into a
    /// fullscreen game to select it — so it arms and waits instead.
    Game,
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
    Watermark,
    Game,
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
    fn ensure(&mut self, source: Option<Source>, fx: &SharedFx, wm: &SharedWatermark, ctx: &egui::Context) {
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
        let wm = wm.clone();
        let is_window = matches!(source, Source::Window { .. });
        let src = source.clone();
        let mut origin = source_origin(&source).unwrap_or((0, 0));
        let mut sampler: Option<MouseSampler> = None;
        let mut badge: Option<WatermarkRenderer> = None;
        let mut badge_failed = false;
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
            let wm = *wm.read().unwrap();
            if wm.any_overlay() {
                if badge.is_none() && !badge_failed {
                    badge = WatermarkRenderer::new();
                    badge_failed = badge.is_none();
                }
                if let Some(b) = &mut badge {
                    b.apply(&wm, &mut frame);
                }
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
    watermark: SharedWatermark,
    /// Composes the badge for snapshots and the Watermark tab. Built on first
    /// use; stays `None` when the bundled artwork cannot be loaded.
    watermark_renderer: Option<WatermarkRenderer>,
    watermark_unavailable: bool,
    /// The badge texture on the Watermark tab, and the pixel height it is for.
    watermark_tex: Option<(u32, TextureHandle)>,
    /// The in-game frame-rate counter's appearance.
    fps_overlay: FpsOverlay,
    /// Composes the counter for the Game tab's preview.
    fps_badge: Option<openclip_overlay::FpsBadge>,
    fps_badge_tex: Option<(u32, TextureHandle)>,
    /// Watches for a game to hook. Only on Windows; a stub elsewhere.
    #[cfg(windows)]
    game: crate::game::GameWatcher,
    /// The one-time explanation of what game mode loads into a game.
    game_consented: bool,
    game_consent_open: bool,
    game_ignored: Vec<String>,
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
    /// Poster frames and durations for the files the library lists.
    thumbs: Thumbs,
    /// The full-window media viewer, when a library file is open in it.
    viewer: Option<Viewer>,
    /// Playback level, remembered between files for this session only.
    viewer_volume: f32,
    viewer_muted: bool,
    preview_tex: Option<TextureHandle>,
    preview_dims: (u32, u32),
    message: Option<(String, bool)>, // (text, is_error)
    message_at: Option<Instant>,
    last_message: Option<String>,
    last_file: Option<PathBuf>,
    /// Compact "mini bar" mode (style floating recorder bar).
    compact: bool,
    /// Outer rect of the full window, restored when leaving compact mode.
    saved_rect: Option<egui::Rect>,
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
    /// Debug aid: `OPENCLIP_OPEN_VIEWER=<path>` opens that file in the media
    /// viewer at start-up, since it otherwise takes a double-click to reach.
    start_viewer: Option<PathBuf>,
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
            watermark: Arc::new(RwLock::new(settings.watermark)),
            watermark_renderer: None,
            watermark_unavailable: false,
            watermark_tex: None,
            fps_overlay: settings.fps_overlay,
            fps_badge: None,
            fps_badge_tex: None,
            #[cfg(windows)]
            game: Default::default(),
            game_consented: settings.game_consented,
            game_consent_open: false,
            game_ignored: settings.game_ignored.clone(),
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
            thumbs: Thumbs::new(),
            viewer: None,
            viewer_volume: 1.0,
            viewer_muted: false,
            preview_tex: None,
            preview_dims: (0, 0),
            message: None,
            message_at: None,
            last_message: None,
            last_file: None,
            compact: false,
            saved_rect: None,
            frame_styled: false,
            frame_excluded: false,
            frame_parts: 0,
            frame_drag: None,
            frame_pointer: None,
            bar_size: minibar::BAR_SIZE,
            about_icon: None,
            start_compact: cfg!(debug_assertions) && std::env::var_os("OPENCLIP_START_COMPACT").is_some(),
            start_viewer: cfg!(debug_assertions)
                .then(|| std::env::var_os("OPENCLIP_OPEN_VIEWER").map(PathBuf::from))
                .flatten(),
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
            // Only once something is actually hooked — which is exactly when the
            // in-game counter appears, so the precondition is visible.
            SourceKind::Game => self.hooked_pid().map(|pid| Source::Game { pid }),
        }
    }

    /// The process id of the hooked game, if there is one.
    pub(super) fn hooked_pid(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            self.game.state.hooked_pid()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Pixel size of the selected source, if known.
    pub(super) fn source_size(&self) -> Option<(u32, u32)> {
        let (w, h) = match self.source_kind {
            SourceKind::Monitor => self.monitors.get(self.monitor_idx).map(|m| (m.width, m.height)).unwrap_or((0, 0)),
            SourceKind::Window => self.windows.get(self.window_idx).map(|w| (w.width, w.height)).unwrap_or((0, 0)),
            SourceKind::Region => self.region.map(|(_, r)| (r.width, r.height)).unwrap_or((0, 0)),
            // The hook reports the game's back-buffer size once it is publishing.
            SourceKind::Game => self.game_frame_size().unwrap_or((0, 0)),
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
            watermark: *self.watermark.read().unwrap(),
            fps_overlay: self.fps_overlay,
            game_consented: self.game_consented,
            game_ignored: self.game_ignored.clone(),
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
            SourceKind::Game => self.game_label(),
        }
    }

    /// What the toolbar and mini bar say for the Game source.
    fn game_label(&self) -> String {
        #[cfg(windows)]
        {
            use crate::game::WatchState;
            match &self.game.state {
                WatchState::Hooked { exe, api, session } => {
                    t!(GAME_HOOKED_LABEL, exe, api.label(), format!("{:.0}", session.present_fps()))
                }
                WatchState::Waiting => t!(GAME_WAITING).into(),
                WatchState::Refused { exe, reason } => crate::game::watcher::refusal_message(exe, reason),
                WatchState::Failed { error, .. } => error.clone(),
                WatchState::Off => t!(GAME_NOT_ARMED).into(),
            }
        }
        #[cfg(not(windows))]
        {
            t!(GAME_WINDOWS_ONLY).into()
        }
    }

    /// The size of the frames the hook is publishing, once it is.
    fn game_frame_size(&self) -> Option<(u32, u32)> {
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            let session = self.game.state.session()?;
            let c = session.control();
            let (w, h) = (c.width.load(Ordering::Relaxed), c.height.load(Ordering::Relaxed));
            (w > 0 && h > 0).then_some((w, h))
        }
        #[cfg(not(windows))]
        {
            None
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
        let game = matches!(source, Source::Game { .. });
        let config = RecordConfig {
            source,
            format,
            // The pointer is somewhere on the desktop, not in the game's back
            // buffer, so cursor and click effects would land in an unrelated
            // corner of the picture. The watermark is frame-relative and fine.
            mouse_fx: if game { MouseFx::default_off() } else { self.mouse_fx.read().unwrap().clone() },
            watermark: *self.watermark.read().unwrap(),
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
        let wm = *self.watermark.read().unwrap();
        let result = (|| {
            let mut frame = screenshot_source(&source)?;
            if let Some(r) = self.watermark_renderer() {
                r.apply(&wm, &mut frame);
            }
            std::fs::create_dir_all(&self.output_dir).ok();
            let img = xcap::image::RgbaImage::from_raw(frame.width, frame.height, frame.data)
                .ok_or_else(|| anyhow::anyhow!("bad image buffer"))?;
            img.save(&path).map_err(|e| anyhow::anyhow!("{e}"))
        })();
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
                        self.live.ensure(self.selected_source(), &self.mouse_fx, &self.watermark, ctx);
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
                    (SourceKind::Game, Some(icons::GAMEPAD), t!(MODE_GAME)),
                ];
                if segmented(ui, "mode", &items, &mut kind) {
                    self.select_mode(kind, ctx);
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
    fn select_mode(&mut self, kind: SourceKind, ctx: &egui::Context) {
        self.source_kind = kind;
        let needs_choice = match kind {
            SourceKind::Window => true,
            SourceKind::Monitor => self.monitors.len() > 1,
            SourceKind::Region | SourceKind::Game => false,
        };
        // Game mode loads code into another process, so it does not start until
        // the user has been told that once and agreed.
        if kind == SourceKind::Game {
            if self.game_consented {
                self.arm_game_mode(ctx);
            } else {
                self.game_consent_open = true;
            }
            self.show_preview_tab();
            return;
        }
        self.disarm_game_mode();
        if kind == SourceKind::Region && self.region.is_none() {
            self.open_picker();
        } else if needs_choice {
            self.show_preview_tab();
        }
    }

    /// Starts watching for a game to hook.
    pub(super) fn arm_game_mode(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            let ctx = ctx.clone();
            self.game.arm(&self.game_ignored, move || ctx.request_repaint());
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    /// Picks up what the watcher found and keeps the counter's appearance in
    /// step with the settings (called every frame).
    pub(super) fn poll_game(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            self.game.poll();
            if self.game.state.is_armed() {
                self.game.push_overlay(openclip_overlay::OverlaySettings {
                    enabled: self.fps_overlay.enabled,
                    corner: Corner::ALL.iter().position(|c| *c == self.fps_overlay.position).unwrap_or(0) as u8,
                    size: self.fps_overlay.size as u16,
                    opacity: self.fps_overlay.opacity as u8,
                    burn_in: self.fps_overlay.in_recording,
                });
                // The hooked game's frame rate changes constantly, and it is on
                // screen; keep the label moving without spinning the GUI.
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    /// The one-time explanation of what Game mode does, before it does it.
    fn game_consent_dialog(&mut self, ctx: &egui::Context) {
        if !self.game_consent_open {
            return;
        }
        let mut accepted = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("game-consent")).frame(sheet_frame()).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(heading(t!(GAME_CONSENT_TITLE)));
            ui.add_space(10.0);
            ui.label(t!(GAME_CONSENT_BODY));
            ui.add_space(8.0);
            ui.label(RichText::new(t!(GAME_CONSENT_ANTICHEAT)).color(ORANGE));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if primary_button(ui, t!(GAME_CONSENT_ACCEPT)).clicked() {
                    accepted = true;
                }
                if gray_button(ui, t!(CANCEL)).clicked() {
                    cancelled = true;
                }
            });
        });
        if accepted {
            self.game_consent_open = false;
            self.game_consented = true;
            self.save_settings();
            self.arm_game_mode(ctx);
        } else if cancelled {
            self.game_consent_open = false;
            // Backing out of the explanation means backing out of the mode.
            if self.source_kind == SourceKind::Game {
                self.source_kind = SourceKind::Monitor;
            }
        }
    }

    /// The hook's version in the hooked game, for the status card.
    #[cfg(windows)]
    fn game_session_info(&self) -> Option<String> {
        let session = self.game.state.session()?;
        let (major, minor, patch) = session.hook_version()?;
        let api = session.api().label();
        Some(format!("{major}.{minor}.{patch} · {api}"))
    }

    /// The executable of the hooked game, if any.
    #[cfg(windows)]
    fn hooked_exe(&self) -> Option<String> {
        match &self.game.state {
            crate::game::WatchState::Hooked { exe, .. } => Some(exe.clone()),
            _ => None,
        }
    }

    /// Never hook this executable again, and let go of it now.
    #[cfg(windows)]
    fn ignore_game(&mut self, exe: &str) {
        if !self.game_ignored.iter().any(|e| e.eq_ignore_ascii_case(exe)) {
            self.game_ignored.push(exe.to_string());
        }
        self.game.ignore(exe);
        self.save_settings();
    }

    /// Opens the hook's own log, which is the only place a problem inside a
    /// fullscreen game can be reported.
    #[cfg(windows)]
    fn open_hook_log(&mut self) {
        let Some(pid) = self.hooked_pid() else { return };
        let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
        let path = std::path::PathBuf::from(base).join("openclip").join(format!("hook-{pid}.log"));
        if path.exists() {
            library::open_with_default(&path);
        } else {
            self.message = Some((t!(MSG_GAME_NO_LOG).into(), true));
        }
    }

    /// The counter as it will look in a game, at a legible size.
    fn fps_overlay_preview_card(&mut self, ui: &mut egui::Ui, fps: &FpsOverlay) {
        section_header(ui, t!(TAB_PREVIEW));
        Card::show(ui, |card| {
            card.custom(|ui| {
                self.fps_overlay_preview(ui, fps);
                ui.label(secondary(t!(FPS_PREVIEW_HINT)).small());
            });
        });
    }

    fn fps_overlay_preview(&mut self, ui: &mut egui::Ui, fps: &FpsOverlay) {
        // Both states side by side: the colour *is* the feature, and showing one
        // of them would leave the other a surprise.
        let recording = self.is_recording();
        ui.horizontal(|ui| {
            for (state, label) in [
                (openclip_overlay::HookState::Ready, t!(GAME_STATE_READY)),
                (openclip_overlay::HookState::Recording, t!(GAME_STATE_RECORDING)),
            ] {
                ui.vertical(|ui| {
                    self.fps_badge_swatch(ui, fps, state, recording);
                    ui.label(secondary(label).small());
                });
                ui.add_space(8.0);
            }
        });
    }

    fn fps_badge_swatch(
        &mut self,
        ui: &mut egui::Ui,
        fps: &FpsOverlay,
        state: openclip_overlay::HookState,
        live: bool,
    ) {
        const PREVIEW_H: u32 = 34;
        let badge = self.fps_badge.get_or_insert_with(|| {
            openclip_overlay::FpsBadge::new().expect("the bundled font parses; the watermark uses it too")
        });
        // A plausible reading when there is no game, the real one when there is.
        let text = if live { "120" } else { "120" };
        let sprite = badge.sprite_for(PREVIEW_H, text, state.color());
        let image = ColorImage::from_rgba_unmultiplied(
            [sprite.width as usize, sprite.height as usize],
            &sprite.rgba,
        );
        let tex = ui.ctx().load_texture(format!("fps-badge-{}", state.as_u32()), image, TextureOptions::LINEAR);
        let size = egui::vec2(sprite.width as f32, sprite.height as f32);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(6), PREVIEW_BG);
        let opacity = fps.opacity.min(100) as f32 / 100.0;
        egui::Image::new(&tex).tint(Color32::WHITE.gamma_multiply(opacity)).paint_at(ui, rect);
    }

    /// Whether the watcher is running.
    pub(super) fn game_armed(&self) -> bool {
        #[cfg(windows)]
        {
            self.game.state.is_armed()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub(super) fn disarm_game_mode(&mut self) {
        #[cfg(windows)]
        {
            self.game.disarm();
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

    fn page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        match self.tab {
            Tab::Home => self.page_home(ui, ctx),
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

    fn page_home(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
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
            _ => self.library_panel(ui, ctx),
        }
    }

    fn library_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
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

        // File list: a poster frame on the left, then name and details.
        let list_h = (ui.available_height() - 60.0).max(80.0);
        let mut clicked: Option<usize> = None;
        let mut activated: Option<usize> = None;
        self.thumbs.retain(&self.library.entries);
        let entries = &self.library.entries;
        let thumbs = &mut self.thumbs;
        let selected_idx = self.library.selected;
        let tab = self.library.tab;
        let empty_label = tab.empty_label();
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
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                        // The probe runs on a background thread; until it
                        // answers the row shows a placeholder tile.
                        let (texture, duration) =
                            thumbs.get(e).map(|t| (t.texture.clone(), t.duration)).unwrap_or((None, None));
                        let p = ui.painter();
                        if i > 0 {
                            p.hline((rect.left() + PAD)..=rect.right(), rect.top(), Stroke::new(1.0, SEPARATOR));
                        }
                        let pill = rect.shrink2(Vec2::new(6.0, 3.0));
                        if selected {
                            p.rect_filled(pill, CornerRadius::same(10), BLUE);
                        } else if resp.hovered() {
                            p.rect_filled(pill, CornerRadius::same(10), FILL_HOVER);
                        }
                        let well = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + PAD, rect.center().y - THUMB_H / 2.0),
                            Vec2::new(THUMB_W, THUMB_H),
                        );
                        paint_thumb(p, well, texture.as_ref(), tab);
                        let (name_color, meta_color) =
                            if selected { (Color32::WHITE, Color32::WHITE) } else { (LABEL, LABEL_2) };
                        let text_left = well.right() + 12.0;
                        let text_rect = egui::Rect::from_min_max(
                            egui::pos2(text_left, rect.top()),
                            egui::pos2(rect.right() - PAD, rect.bottom()),
                        );
                        let clip = p.with_clip_rect(text_rect);
                        clip.text(
                            egui::pos2(text_left, rect.center().y - 9.0),
                            Align2::LEFT_CENTER,
                            &e.name,
                            FontId::proportional(13.0),
                            name_color,
                        );
                        clip.text(
                            egui::pos2(text_left, rect.center().y + 10.0),
                            Align2::LEFT_CENTER,
                            entry_details(e, duration),
                            FontId::proportional(11.0),
                            meta_color,
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
            if let Some(path) = self.library.entries.get(i).map(|e| e.path.clone()) {
                self.open_viewer(ctx, path);
            }
        }

        // Actions.
        let selected = self.library.selected_entry().map(|e| e.path.clone());
        let mut open: Option<PathBuf> = None;
        ui.horizontal(|ui| {
            let has = selected.is_some();
            ui.add_enabled_ui(has, |ui| {
                if tinted_button(ui, &format!("{}  {}", icons::PLAY, t!(PLAY))).clicked()
                    && let Some(p) = &selected
                {
                    open = Some(p.clone());
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
        if let Some(path) = open {
            self.open_viewer(ctx, path);
        }
    }

    fn source_row(&mut self, ui: &mut egui::Ui, recording: bool) {
        ui.add_enabled_ui(!recording, |ui| {
            Card::show(ui, |card| {
                let label = match self.source_kind {
                    SourceKind::Monitor => t!(MODE_MONITOR),
                    SourceKind::Window => t!(MODE_WINDOW),
                    SourceKind::Region => t!(MODE_REGION),
                    SourceKind::Game => t!(MODE_GAME),
                };
                card.row(label, |ui| {
                    if icon_button(ui, icons::REFRESH, t!(REFRESH_SOURCES_TIP)).clicked() {
                        self.refresh_sources();
                    }
                    let combo_w = combo_width(ui);
                    match self.source_kind {
                        SourceKind::Monitor => {
                            let label = self
                                .monitors
                                .get(self.monitor_idx)
                                .map(|m| m.label())
                                .unwrap_or_else(|| t!(NO_MONITORS).into());
                            egui::ComboBox::from_id_salt("monitor")
                                .width(combo_w)
                                .truncate()
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    truncate_items(ui);
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
                            egui::ComboBox::from_id_salt("window")
                                .width(combo_w)
                                .truncate()
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    truncate_items(ui);
                                    for (i, w) in self.windows.iter().enumerate() {
                                        let resp = ui.selectable_value(&mut self.window_idx, i, w.label());
                                        resp.on_hover_text(w.label());
                                    }
                                });
                        }
                        SourceKind::Region => {
                            if tinted_button_small(ui, t!(SELECT_REGION)).clicked() {
                                self.open_picker();
                            }
                            ui.add(egui::Label::new(secondary(self.source_label())).truncate());
                        }
                        SourceKind::Game => {
                            let armed = self.game_armed();
                            let button = if armed { t!(GAME_DISARM) } else { t!(GAME_ARM) };
                            if tinted_button_small(ui, button).clicked() {
                                if armed {
                                    self.disarm_game_mode();
                                } else if self.game_consented {
                                    self.arm_game_mode(ui.ctx());
                                } else {
                                    self.game_consent_open = true;
                                }
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
                    // `fit_size` carries the clamp that keeps a squeezed window
                    // from asking egui for a negative-sized image.
                    let size = fit_size(avail, self.preview_dims, 16.0, 3.0);
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
        segmented(
            ui,
            "video",
            &[
                (VideoTab::Record, None, t!(TAB_RECORD)),
                (VideoTab::Mouse, None, t!(TAB_MOUSE)),
                (VideoTab::Watermark, None, t!(TAB_WATERMARK)),
                (VideoTab::Game, None, t!(TAB_GAME)),
            ],
            &mut vt,
        );
        self.video_tab = vt;
        ui.add_space(4.0);
        match self.video_tab {
            VideoTab::Record => self.video_record_tab(ui),
            VideoTab::Mouse => self.video_mouse_tab(ui),
            VideoTab::Watermark => self.video_watermark_tab(ui),
            VideoTab::Game => self.video_game_tab(ui),
        }
    }

    /// Game mode: the in-game counter's appearance, and what is hooked.
    fn video_game_tab(&mut self, ui: &mut egui::Ui) {
        let current = self.fps_overlay;
        let mut fps = current;
        if ui.available_width() >= FX_SETTINGS_W + FX_GAP + FX_PREVIEW_W {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(FX_SETTINGS_W);
                    fps_overlay_cards(ui, &mut fps);
                });
                ui.add_space(FX_GAP);
                ui.vertical(|ui| {
                    ui.set_width(FX_PREVIEW_W);
                    self.fps_overlay_preview_card(ui, &fps);
                });
            });
        } else {
            fps_overlay_cards(ui, &mut fps);
            self.fps_overlay_preview_card(ui, &fps);
        }
        if fps != current {
            self.fps_overlay = fps;
            self.save_settings();
        }
        self.game_status_card(ui);
    }

    /// What the watcher has found, and the way out of it.
    fn game_status_card(&mut self, ui: &mut egui::Ui) {
        section_header(ui, t!(SECTION_GAME_CAPTURE));
        let armed = self.game_armed();
        Card::show(ui, |card| {
            card.text_row(t!(GAME_STATUS), &self.game_label());
            #[cfg(windows)]
            if let Some(session) = self.game_session_info() {
                card.text_row(t!(GAME_HOOK_VERSION), &session);
            }
            card.row(t!(GAME_MODE_ROW), |ui| {
                if tinted_button_small(ui, if armed { t!(GAME_DISARM) } else { t!(GAME_ARM) }).clicked() {
                    if armed {
                        self.disarm_game_mode();
                    } else if self.game_consented {
                        self.arm_game_mode(ui.ctx());
                    } else {
                        self.game_consent_open = true;
                    }
                }
            });
            #[cfg(windows)]
            {
                let hooked = self.hooked_exe();
                if let Some(exe) = hooked {
                    card.row(t!(GAME_IGNORE_ROW), |ui| {
                        if gray_button(ui, t!(GAME_IGNORE)).clicked() {
                            self.ignore_game(&exe);
                        }
                    });
                }
                card.row(t!(GAME_HOOK_LOG), |ui| {
                    if gray_button(ui, t!(OPEN)).clicked() {
                        self.open_hook_log();
                    }
                });
            }
        });
        footnote(ui, t!(GAME_ANTICHEAT_FOOTNOTE));
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
                        let w = combo_width(ui);
                        egui::ComboBox::from_id_salt("mic").width(w).truncate().selected_text(label).show_ui(ui, |ui| {
                            truncate_items(ui);
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
        // Side by side only while both columns fit; in a narrow window the
        // preview column would be clipped by the panel, so stack it below.
        if ui.available_width() >= FX_SETTINGS_W + FX_GAP + FX_PREVIEW_W {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(FX_SETTINGS_W);
                    mouse_fx_cards(ui, &mut fx);
                });
                ui.add_space(FX_GAP);
                ui.vertical(|ui| {
                    ui.set_width(FX_PREVIEW_W);
                    self.mouse_fx_preview_card(ui, &fx);
                });
            });
        } else {
            mouse_fx_cards(ui, &mut fx);
            self.mouse_fx_preview_card(ui, &fx);
        }
        if fx != current {
            *self.mouse_fx.write().unwrap() = fx;
        }
    }

    fn video_watermark_tab(&mut self, ui: &mut egui::Ui) {
        let current = *self.watermark.read().unwrap();
        let mut wm = current;
        if ui.available_width() >= FX_SETTINGS_W + FX_GAP + FX_PREVIEW_W {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(FX_SETTINGS_W);
                    watermark_cards(ui, &mut wm);
                });
                ui.add_space(FX_GAP);
                ui.vertical(|ui| {
                    ui.set_width(FX_PREVIEW_W);
                    self.watermark_preview_card(ui, &wm);
                });
            });
        } else {
            watermark_cards(ui, &mut wm);
            self.watermark_preview_card(ui, &wm);
        }
        if wm != current {
            *self.watermark.write().unwrap() = wm;
            self.save_settings();
        }
    }

    fn watermark_preview_card(&mut self, ui: &mut egui::Ui, wm: &Watermark) {
        section_header(ui, t!(TAB_PREVIEW));
        Card::show(ui, |card| {
            card.custom(|ui| {
                self.watermark_preview(ui, wm);
                ui.label(secondary(t!(WATERMARK_PREVIEW_HINT)).small());
            });
        });
    }

    /// A 16:9 checkerboard with the real badge in the chosen corner. At this
    /// scale a 1080p badge would be about five points tall, so it is drawn
    /// `EXAGGERATION` times larger to stay readable; everything else (corner,
    /// margin, proportions, opacity) matches what lands in the file.
    fn watermark_preview(&mut self, ui: &mut egui::Ui, wm: &Watermark) {
        const EXAGGERATION: f32 = 4.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(200.0, 112.0), Sense::hover());
        let p = ui.painter_at(rect);
        let cell = 10.0;
        for cy in 0..(rect.height() / cell).ceil() as i32 {
            for cx in 0..(rect.width() / cell).ceil() as i32 {
                let c = if (cx + cy) % 2 == 0 { CHECKER_LIGHT } else { CHECKER_DARK };
                let r = egui::Rect::from_min_size(
                    rect.min + Vec2::new(cx as f32 * cell, cy as f32 * cell),
                    Vec2::splat(cell),
                )
                .intersect(rect);
                p.rect_filled(r, CornerRadius::ZERO, c);
            }
        }
        if !wm.any_overlay() {
            return;
        }
        // Badge height relative to a 1080p recording, applied to the preview.
        let rel = wm.badge_height(1080) as f32 / 1080.0;
        let h = (rect.height() * rel * EXAGGERATION).max(12.0);
        let px = (h * ui.ctx().pixels_per_point()).round() as u32;
        let Some(tex) = self.watermark_texture(ui.ctx(), px) else { return };
        let size = tex.size_vec2();
        let (w, m) = (h * size.x / size.y.max(1.0), h * 0.55);
        let (left, top) = (rect.left() + m, rect.top() + m);
        let (right, bottom) = (rect.right() - w - m, rect.bottom() - h - m);
        let min = match wm.position {
            Corner::TopLeft => egui::pos2(left, top),
            Corner::TopRight => egui::pos2(right, top),
            Corner::BottomLeft => egui::pos2(left, bottom),
            Corner::BottomRight => egui::pos2(right, bottom),
        };
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let tint = Color32::from_white_alpha((wm.opacity.min(100) * 255 / 100) as u8);
        p.image(tex.id(), egui::Rect::from_min_size(min, Vec2::new(w, h)), uv, tint);
    }

    /// The badge as a texture, recomposed only when its size changes.
    fn watermark_texture(&mut self, ctx: &egui::Context, px: u32) -> Option<TextureHandle> {
        if self.watermark_tex.as_ref().map(|(h, _)| *h) != Some(px) {
            let sprite = self.watermark_renderer()?.sprite(px);
            let image =
                ColorImage::from_rgba_unmultiplied([sprite.width as usize, sprite.height as usize], &sprite.rgba);
            self.watermark_tex = Some((px, ctx.load_texture("watermark", image, TextureOptions::LINEAR)));
        }
        self.watermark_tex.as_ref().map(|(_, tex)| tex.clone())
    }

    /// The badge renderer, built on first use. `None` means the bundled artwork
    /// could not be loaded (already logged), so callers simply skip the badge.
    fn watermark_renderer(&mut self) -> Option<&mut WatermarkRenderer> {
        if self.watermark_renderer.is_none() && !self.watermark_unavailable {
            self.watermark_renderer = WatermarkRenderer::new();
            self.watermark_unavailable = self.watermark_renderer.is_none();
        }
        self.watermark_renderer.as_mut()
    }

    fn mouse_fx_preview_card(&mut self, ui: &mut egui::Ui, fx: &MouseFx) {
        section_header(ui, t!(TAB_PREVIEW));
        Card::show(ui, |card| {
            card.custom(|ui| {
                self.fx_preview(ui, fx);
                ui.label(secondary(t!(FX_PREVIEW_HINT)).small());
            });
        });
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
            let on = self.watermark.read().unwrap().any_overlay();
            if card.nav_row(t!(ROW_WATERMARK), if on { t!(ON) } else { t!(OFF) }).clicked() {
                self.tab = Tab::Video;
                self.video_tab = VideoTab::Watermark;
            }
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
                        Ok(()) => {
                        if self.viewer.as_ref().is_some_and(|v| v.path == path) {
                            self.close_viewer();
                        }
                        self.message = Some((t!(MSG_DELETED, path.display()), false));
                    }
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
        if self.thumbs.poll(&ctx) {
            ctx.request_repaint();
        }
        self.tick_countdown(&ctx);
        self.poll_game(&ctx);
        self.poll_preview(&ctx);
        self.track_message();
        if let Some(path) = self.start_viewer.take() {
            self.open_viewer(&ctx, path);
        }
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

        // The media viewer owns the whole window while it is open: no toolbar,
        // status strip, navigation or footer behind it.
        if self.viewer_is_open() {
            self.viewer_ui(ui, &ctx);
            self.delete_dialog(&ctx);
            return;
        }

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
            .show(ui, |ui| self.page(ui, &ctx));

        self.countdown_overlay(&ctx);
        self.delete_dialog(&ctx);
        self.show_format_dialog(&ctx);
        self.game_consent_dialog(&ctx);
        self.update_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        self.close_viewer();
        // Let go of any hooked game, so its counter goes away with us rather
        // than waiting for the hook to notice we died.
        self.disarm_game_mode();
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

/// The mouse-effect settings cards (everything but the live preview).
fn mouse_fx_cards(ui: &mut egui::Ui, fx: &mut MouseFx) {
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
}

/// The watermark settings cards (everything but the live preview).
fn watermark_cards(ui: &mut egui::Ui, wm: &mut Watermark) {
    section_header(ui, t!(SECTION_WATERMARK));
    Card::show(ui, |card| {
        switch_row(card, t!(CHK_WATERMARK), &mut wm.enabled);
        card.row(t!(ROW_POSITION_INDENT).trim(), |ui| {
            ui.add_enabled_ui(wm.enabled, |ui| corner_combo(ui, &mut wm.position));
        });
        card.row(t!(ROW_SIZE_INDENT).trim(), |ui| {
            ui.add_enabled_ui(wm.enabled, |ui| size_combo(ui, "watermark_size", &mut wm.size));
        });
        card.row(t!(ROW_OPACITY).trim(), |ui| {
            ui.add_enabled_ui(wm.enabled, |ui| {
                ui.add(egui::DragValue::new(&mut wm.opacity).range(0..=100).suffix(" %"));
                ui.add(egui::Slider::new(&mut wm.opacity, 0..=100).show_value(false));
            });
        });
    });
    footnote(ui, t!(WATERMARK_FOOTNOTE));
}

/// The counter's settings, laid out like the watermark's — they are the same
/// kind of thing and there is no reason for them to look different.
fn fps_overlay_cards(ui: &mut egui::Ui, fps: &mut FpsOverlay) {
    section_header(ui, t!(SECTION_FPS_COUNTER));
    Card::show(ui, |card| {
        switch_row(card, t!(CHK_FPS_COUNTER), &mut fps.enabled);
        card.row(t!(ROW_POSITION_INDENT).trim(), |ui| {
            ui.add_enabled_ui(fps.enabled, |ui| corner_combo(ui, &mut fps.position));
        });
        card.row(t!(ROW_SIZE_INDENT).trim(), |ui| {
            ui.add_enabled_ui(fps.enabled, |ui| size_combo(ui, "fps_counter_size", &mut fps.size));
        });
        card.row(t!(ROW_OPACITY).trim(), |ui| {
            ui.add_enabled_ui(fps.enabled, |ui| {
                ui.add(egui::DragValue::new(&mut fps.opacity).range(0..=100).suffix(" %"));
                ui.add(egui::Slider::new(&mut fps.opacity, 0..=100).show_value(false));
            });
        });
        switch_row(card, t!(CHK_FPS_IN_RECORDING), &mut fps.in_recording);
    });
    footnote(ui, t!(FPS_COUNTER_FOOTNOTE));
}

fn corner_label(corner: Corner) -> &'static str {
    match corner {
        Corner::TopLeft => t!(POS_TOP_LEFT),
        Corner::TopRight => t!(POS_TOP_RIGHT),
        Corner::BottomLeft => t!(POS_BOTTOM_LEFT),
        Corner::BottomRight => t!(POS_BOTTOM_RIGHT),
    }
}

fn corner_combo(ui: &mut egui::Ui, value: &mut Corner) {
    egui::ComboBox::from_id_salt("watermark_corner")
        .width(combo_width(ui))
        .truncate()
        .selected_text(corner_label(*value))
        .show_ui(ui, |ui| {
            for corner in Corner::ALL {
                ui.selectable_value(value, corner, corner_label(corner));
            }
        });
}

/// Width for a combo that has to stay inside its card row. `ComboBox::width`
/// is only a *minimum*: with the default (Extend) wrap mode a long window
/// title or device name grows the button past the row, so pair this with
/// `ComboBox::truncate`.
fn combo_width(ui: &egui::Ui) -> f32 {
    let avail = ui.available_width();
    (avail - 8.0).min(360.0).clamp(60.0, avail.max(60.0))
}

/// Truncates the entries of a combo popup too — egui opens the menu with
/// `Extend`, so long titles would widen the popup instead of the button.
fn truncate_items(ui: &mut egui::Ui) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
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
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

pub(super) fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Draws the poster tile: the frame itself when one has been decoded, a tinted
/// icon for the file kind while it is being read (or when it cannot be).
fn paint_thumb(p: &egui::Painter, well: egui::Rect, texture: Option<&TextureHandle>, tab: LibraryTab) {
    p.rect_filled(well, CornerRadius::same(6), PREVIEW_BG);
    let Some(texture) = texture else {
        let icon = match tab {
            LibraryTab::Videos => icons::FILM,
            LibraryTab::Images => icons::IMAGE,
            LibraryTab::Audios => icons::MUSIC,
        };
        p.text(well.center(), Align2::CENTER_CENTER, icon, FontId::proportional(18.0), LABEL_3);
        return;
    };
    // Letterbox rather than crop, so the tile shows the whole frame.
    let size = texture.size_vec2();
    let scale = (well.width() / size.x).min(well.height() / size.y);
    let fitted = egui::Rect::from_center_size(well.center(), size * scale);
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    p.add(
        egui::epaint::RectShape::filled(fitted, CornerRadius::same(6), Color32::WHITE)
            .with_texture(texture.id(), uv),
    );
}

/// The second line of a library row: size, when the file was made and, for
/// anything with a running time, how long it is.
fn entry_details(entry: &library::Entry, duration: Option<Duration>) -> String {
    let mut s = human_bytes(entry.size);
    s.push_str("  ·  ");
    s.push_str(&local_datetime(entry.created));
    if let Some(d) = duration {
        s.push_str("  ·  ");
        s.push_str(&short_duration(d));
    }
    s
}

/// `M:SS`, widening to `H:MM:SS` for recordings past the hour.
pub(super) fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// `YYYY-MM-DD HH:MM` in the local time zone. Digits only: no month name
/// needs translating, and the order is the same in every language.
fn local_datetime(t: SystemTime) -> String {
    let Ok(since_epoch) = t.duration_since(std::time::UNIX_EPOCH) else { return String::new() };
    let local = since_epoch.as_secs() as i64 + local_offset_secs();
    if local < 0 {
        return String::new();
    }
    let (y, m, d, hh, mm, _) = civil(local as u64);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Seconds to add to UTC to get local time. Read once: a session does not
/// outlive a time-zone change often enough to be worth re-reading per row.
fn local_offset_secs() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        #[cfg(windows)]
        {
            use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION};
            const DAYLIGHT: u32 = 2; // TIME_ZONE_ID_DAYLIGHT
            let mut tz = TIME_ZONE_INFORMATION::default();
            // SAFETY: plain Win32 call filling in a stack struct.
            let id = unsafe { GetTimeZoneInformation(&mut tz) };
            if id == TIME_ZONE_ID_INVALID {
                return 0;
            }
            // Bias is "UTC = local + bias" in minutes, so the sign flips.
            let bias = tz.Bias + if id == DAYLIGHT { tz.DaylightBias } else { tz.StandardBias };
            -(bias as i64) * 60
        }
        #[cfg(not(windows))]
        0
    })
}

/// Civil date and clock time from seconds since the Unix epoch (Howard
/// Hinnant's algorithm).
fn civil(secs: u64) -> (i64, i64, i64, u64, u64, u64) {
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
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
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
