//! egui application: source picker, settings, live preview and recording controls.

pub mod picker;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions};

use crate::audio::capture::list_input_devices;
use crate::capture::monitors::{list_monitors, list_windows, screenshot_source, MonitorInfo, WindowInfo};
use crate::capture::{Rect, Source};
use crate::pipeline::{RecordConfig, Recorder};
use crate::video::preview::{make_preview, PreviewImage};

use picker::{Picker, PickerOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Monitor,
    Window,
    Region,
}

enum State {
    Idle,
    Picking(Picker),
    Recording(Recorder),
}

/// Request/response pair for the background thumbnail worker.
struct ThumbWorker {
    tx: Sender<Source>,
    rx: Receiver<PreviewImage>,
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
    show_cursor: bool,
    system_audio: bool,
    mic_enabled: bool,
    mic_idx: usize,
    output_dir: PathBuf,
    state: State,
    preview_tex: Option<TextureHandle>,
    preview_dims: (u32, u32),
    preview_source: Option<Source>,
    last_thumb_request: Option<Instant>,
    thumbs: ThumbWorker,
    thumb_pending: bool,
    message: Option<(String, bool)>, // (text, is_error)
    last_file: Option<PathBuf>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.0);
        let monitors = list_monitors().unwrap_or_default();
        let windows = list_windows().unwrap_or_default();
        let mics = list_input_devices();
        let output_dir = default_output_dir();
        let (req_tx, req_rx) = mpsc::channel::<Source>();
        let (img_tx, img_rx) = mpsc::channel::<PreviewImage>();
        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("openclip-thumbs".into())
            .spawn(move || {
                while let Ok(src) = req_rx.recv() {
                    // Coalesce: only the newest request matters.
                    let src = req_rx.try_iter().last().unwrap_or(src);
                    if let Ok(frame) = screenshot_source(&src) {
                        if img_tx.send(make_preview(&frame, 640)).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .expect("thumbnail thread");
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
            show_cursor: true,
            system_audio: true,
            mic_enabled: false,
            mic_idx: 0,
            output_dir,
            state: State::Idle,
            preview_tex: None,
            preview_dims: (0, 0),
            preview_source: None,
            last_thumb_request: None,
            thumbs: ThumbWorker { tx: req_tx, rx: img_rx },
            thumb_pending: false,
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

    fn refresh_sources(&mut self) {
        self.monitors = list_monitors().unwrap_or_default();
        self.windows = list_windows().unwrap_or_default();
        self.mics = list_input_devices();
        self.monitor_idx = self.monitor_idx.min(self.monitors.len().saturating_sub(1));
        self.window_idx = self.window_idx.min(self.windows.len().saturating_sub(1));
        self.mic_idx = self.mic_idx.min(self.mics.len().saturating_sub(1));
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let Some(source) = self.selected_source() else {
            self.message = Some(("Select something to record first.".into(), true));
            return;
        };
        let file = format!("openclip-{}.mp4", timestamp());
        let output = self.output_dir.join(file);
        let config = RecordConfig {
            source,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            half_resolution: self.half_resolution,
            show_cursor: self.show_cursor,
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
            Err(e) => self.message = Some((format!("Could not start recording: {e:#}"), true)),
        }
    }

    fn stop_recording(&mut self) {
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            match rec.stop() {
                Ok(path) => {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    self.message = Some((format!("Saved {} ({})", path.display(), human_bytes(size)), false));
                    self.last_file = Some(path);
                }
                Err(e) => self.message = Some((format!("Recording failed: {e:#}"), true)),
            }
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
        match &self.state {
            State::Recording(rec) => {
                if let Some(img) = rec.preview().take() {
                    self.upload_preview(ctx, &img);
                }
            }
            State::Idle => {
                let source = self.selected_source();
                let changed = source != self.preview_source;
                let due = self.last_thumb_request.map(|t| t.elapsed() > Duration::from_millis(1500)).unwrap_or(true);
                if let Some(src) = source.clone()
                    && (changed || due)
                    && !self.thumb_pending
                {
                    let _ = self.thumbs.tx.send(src);
                    self.thumb_pending = true;
                    self.last_thumb_request = Some(Instant::now());
                    self.preview_source = source;
                }
                if let Some(img) = self.thumbs.rx.try_iter().last() {
                    self.thumb_pending = false;
                    self.upload_preview(ctx, &img);
                }
                ctx.request_repaint_after(Duration::from_millis(500));
            }
            State::Picking(_) => {}
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let recording = matches!(self.state, State::Recording(_));
        ui.add_enabled_ui(!recording, |ui| {
            ui.heading("Source");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.source_kind, SourceKind::Monitor, "Monitor");
                ui.selectable_value(&mut self.source_kind, SourceKind::Window, "Window");
                ui.selectable_value(&mut self.source_kind, SourceKind::Region, "Region");
                if ui.small_button("⟳").on_hover_text("Refresh monitors, windows and devices").clicked() {
                    self.refresh_sources();
                }
            });
            match self.source_kind {
                SourceKind::Monitor => {
                    let label = self.monitors.get(self.monitor_idx).map(|m| m.label()).unwrap_or("No monitors".into());
                    egui::ComboBox::from_id_salt("monitor").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        for (i, m) in self.monitors.iter().enumerate() {
                            ui.selectable_value(&mut self.monitor_idx, i, m.label());
                        }
                    });
                }
                SourceKind::Window => {
                    let label = self.windows.get(self.window_idx).map(|w| w.label()).unwrap_or("No windows".into());
                    egui::ComboBox::from_id_salt("window").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        for (i, w) in self.windows.iter().enumerate() {
                            ui.selectable_value(&mut self.window_idx, i, w.label());
                        }
                    });
                }
                SourceKind::Region => {
                    match self.region {
                        Some((mid, r)) => {
                            let mon = self.monitors.iter().find(|m| m.id == mid).map(|m| m.name.clone()).unwrap_or_default();
                            ui.label(format!("{}×{} at ({}, {}) on {}", r.width, r.height, r.x, r.y, mon));
                        }
                        None => {
                            ui.label("No region selected");
                        }
                    }
                    if ui.button("Select region…").clicked() {
                        match Picker::new(&self.monitors) {
                            Ok(p) => self.state = State::Picking(p),
                            Err(e) => self.message = Some((format!("Region picker: {e:#}"), true)),
                        }
                    }
                }
            }

            ui.add_space(12.0);
            ui.heading("Video");
            ui.horizontal(|ui| {
                ui.label("Frame rate");
                for f in [15u32, 24, 30, 60] {
                    ui.selectable_value(&mut self.fps, f, f.to_string());
                }
            });
            ui.horizontal(|ui| {
                ui.label("Bitrate");
                ui.add(egui::Slider::new(&mut self.bitrate_kbps, 500..=20_000).suffix(" kbps").logarithmic(true));
            });
            ui.checkbox(&mut self.half_resolution, "Half resolution (faster, smaller files)");
            ui.checkbox(&mut self.show_cursor, "Show mouse cursor");

            ui.add_space(12.0);
            ui.heading("Audio");
            ui.checkbox(&mut self.system_audio, "System audio (what you hear)");
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.mic_enabled, "Microphone");
                ui.add_enabled_ui(self.mic_enabled && !self.mics.is_empty(), |ui| {
                    let label = self.mics.get(self.mic_idx).cloned().unwrap_or("No input devices".into());
                    egui::ComboBox::from_id_salt("mic").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        for (i, m) in self.mics.iter().enumerate() {
                            ui.selectable_value(&mut self.mic_idx, i, m);
                        }
                    });
                });
            });

            ui.add_space(12.0);
            ui.heading("Output");
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.output_dir.display().to_string()).small()).on_hover_text(self.output_dir.display().to_string());
            });
            if ui.button("Choose folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(&self.output_dir).pick_folder()
            {
                self.output_dir = dir;
            }
        });
    }

    fn record_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            match &self.state {
                State::Recording(rec) => {
                    let stop = ui.add(egui::Button::new(RichText::new("■  Stop").size(18.0)).min_size([120.0, 36.0].into()));
                    let s = rec.stats();
                    let elapsed = rec.elapsed();
                    let encoded = s.frames_encoded.load(Ordering::Relaxed);
                    let dropped = s.frames_dropped.load(Ordering::Relaxed);
                    let bytes = s.bytes_written.load(Ordering::Relaxed);
                    let (w, h) = (s.width.load(Ordering::Relaxed), s.height.load(Ordering::Relaxed));
                    let fps = if elapsed.as_secs_f64() > 0.5 { encoded as f64 / elapsed.as_secs_f64() } else { 0.0 };
                    ui.label(RichText::new("●").color(Color32::RED).size(18.0));
                    ui.label(format!(
                        "{}   {}×{}   {:.1} fps   {} dropped   {}",
                        format_duration(elapsed),
                        w,
                        h,
                        fps,
                        dropped,
                        human_bytes(bytes)
                    ));
                    if let Some(n) = s.audio_note.lock().unwrap().as_ref() {
                        ui.label(RichText::new(n).color(Color32::YELLOW));
                    }
                    let failed = s.error().is_some() || rec.is_finished();
                    if stop.clicked() || failed {
                        self.stop_recording();
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(250));
                    }
                }
                _ => {
                    let can_record = self.selected_source().is_some() && matches!(self.state, State::Idle);
                    let btn = egui::Button::new(RichText::new("●  Record").size(18.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(200, 40, 40))
                        .min_size([120.0, 36.0].into());
                    if ui.add_enabled(can_record, btn).clicked() {
                        self.start_recording(ctx);
                    }
                    if let Some(path) = self.last_file.clone()
                        && ui.button("Open folder").clicked()
                    {
                        open_folder(&path);
                    }
                }
            }
        });
        if let Some((msg, is_err)) = &self.message {
            let color = if *is_err { Color32::from_rgb(230, 80, 80) } else { Color32::from_rgb(90, 200, 120) };
            ui.label(RichText::new(msg).color(color));
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        egui::Frame::new().fill(Color32::from_gray(18)).corner_radius(6.0).show(ui, |ui| {
            ui.set_min_size(avail);
            match &self.preview_tex {
                Some(tex) if self.preview_dims.0 > 0 => {
                    let (w, h) = (self.preview_dims.0 as f32, self.preview_dims.1 as f32);
                    let scale = (avail.x / w).min(avail.y / h).min(4.0);
                    let size = egui::vec2(w * scale, h * scale);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::from_texture(&*tex).fit_to_exact_size(size));
                    });
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Preview").color(Color32::GRAY).size(20.0));
                    });
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_preview(&ctx);

        // Region picker runs as extra viewports on top of everything.
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

        egui::Panel::left("sidebar").resizable(false).exact_size(340.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                self.sidebar(ui);
                ui.add_space(6.0);
            });
        });
        egui::Panel::bottom("record_bar").show(ui, |ui| {
            ui.add_space(6.0);
            self.record_bar(ui, &ctx);
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.preview_panel(ui);
        });
    }

    fn on_exit(&mut self) {
        if let State::Recording(rec) = std::mem::replace(&mut self.state, State::Idle) {
            let _ = rec.stop();
        }
    }
}

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
    // Civil date from days since epoch (Howard Hinnant's algorithm), local time not required.
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
    let dir = path.parent().unwrap_or(path);
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
