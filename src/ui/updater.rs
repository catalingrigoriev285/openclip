//! Update UI: the background check (start-up / on demand), the "Update to vX"
//! chip in the status strip, the check row on the About and General pages and
//! the update modal (release notes, download progress, restart). The work
//! itself — GitHub API, download, verification, exe swap — is [`crate::update`].

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use eframe::egui::{self, RichText};

use super::theme::*;
use super::{App, State, human_bytes, icons, settings_row};
use crate::i18n::{self, key};
use crate::t;
use crate::update::{self, Progress, Release};

pub(super) enum UpdateState {
    /// Not checked (yet) — the start-up check is off or still to be spawned.
    Idle,
    Checking(mpsc::Receiver<anyhow::Result<Option<Release>>>),
    UpToDate,
    Available(Box<Release>),
    Downloading {
        rel: Box<Release>,
        progress: Arc<Progress>,
        rx: mpsc::Receiver<anyhow::Result<PathBuf>>,
    },
    /// The executable on disk has been replaced; the new build runs after a restart.
    Installed { rel: Box<Release>, exe: PathBuf },
    Failed { rel: Option<Box<Release>>, error: String },
}

impl UpdateState {
    fn release(&self) -> Option<&Release> {
        match self {
            UpdateState::Available(rel)
            | UpdateState::Downloading { rel, .. }
            | UpdateState::Installed { rel, .. } => Some(rel),
            UpdateState::Failed { rel, .. } => rel.as_deref(),
            UpdateState::Idle | UpdateState::Checking(_) | UpdateState::UpToDate => None,
        }
    }

    fn busy(&self) -> bool {
        matches!(self, UpdateState::Checking(_) | UpdateState::Downloading { .. })
    }
}

enum Action {
    None,
    Download,
    Retry,
    Cancel,
    OpenPage,
    Restart,
}

fn version_label(rel: &Release) -> String {
    format!("v{}", rel.version)
}

impl App {
    /// Asks GitHub for the latest release on a background thread. No-op while
    /// a check or a download is already running.
    pub(super) fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.update.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("openclip-update".into()).spawn(move || {
            let _ = tx.send(update::check());
            ctx.request_repaint();
        });
        self.update = match spawned {
            Ok(_) => UpdateState::Checking(rx),
            Err(e) => UpdateState::Failed { rel: None, error: e.to_string() },
        };
    }

    /// Picks up results from the check / download threads (called every frame).
    pub(super) fn poll_update(&mut self, ctx: &egui::Context) {
        let state = std::mem::replace(&mut self.update, UpdateState::Idle);
        self.update = match state {
            UpdateState::Checking(rx) => match rx.try_recv() {
                Ok(Ok(Some(rel))) => {
                    log::info!("update available: {} ({})", rel.version, rel.html_url);
                    self.message = Some((t!(MSG_UPDATE_AVAILABLE, version_label(&rel)), false));
                    UpdateState::Available(Box::new(rel))
                }
                Ok(Ok(None)) => UpdateState::UpToDate,
                Ok(Err(e)) => {
                    log::warn!("update check failed: {e:#}");
                    UpdateState::Failed { rel: None, error: format!("{e:#}") }
                }
                Err(mpsc::TryRecvError::Empty) => UpdateState::Checking(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    UpdateState::Failed { rel: None, error: "the update check did not finish".into() }
                }
            },
            UpdateState::Downloading { rel, progress, rx } => match rx.try_recv() {
                Ok(Ok(exe)) => UpdateState::Installed { rel, exe },
                Ok(Err(_)) if progress.cancel.load(Ordering::Relaxed) => UpdateState::Available(rel),
                Ok(Err(e)) => {
                    log::warn!("update failed: {e:#}");
                    UpdateState::Failed { rel: Some(rel), error: format!("{e:#}") }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    UpdateState::Downloading { rel, progress, rx }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    UpdateState::Failed { rel: Some(rel), error: "the download did not finish".into() }
                }
            },
            other => other,
        };
    }

    /// Downloads and installs the available release on a background thread.
    fn start_update_download(&mut self, ctx: &egui::Context) {
        let UpdateState::Available(rel) = &self.update else { return };
        if !matches!(self.state, State::Idle) || rel.asset.is_none() || !update::install_dir_writable() {
            return;
        }
        let rel = rel.clone();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let (rel2, progress2, ctx2) = (rel.clone(), progress.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("openclip-update-download".into()).spawn(move || {
            let _ = tx.send(update::download_and_install(&rel2, &progress2));
            ctx2.request_repaint();
        });
        self.update = match spawned {
            Ok(_) => UpdateState::Downloading { rel, progress, rx },
            Err(e) => UpdateState::Failed { rel: Some(rel), error: e.to_string() },
        };
    }

    /// Asks a running download to stop (Cancel button, app exit); the thread
    /// removes its temporary files.
    pub(super) fn cancel_update_download(&mut self) {
        if let UpdateState::Downloading { progress, .. } = &self.update {
            progress.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// "Update to vX" button for the status strip; drawn only when idle.
    pub(super) fn update_chip(&mut self, ui: &mut egui::Ui) {
        if !matches!(self.update, UpdateState::Available(_) | UpdateState::Installed { .. }) {
            return;
        }
        let Some(rel) = self.update.release() else { return };
        let text = format!("{} {}", icons::DOWNLOAD, t!(UPDATE_CHIP, version_label(rel)));
        if ui.small_button(RichText::new(text).color(ACCENT)).on_hover_text(t!(UPDATE_CHIP_TIP)).clicked() {
            self.update_modal = true;
        }
    }

    /// "Check for updates" button followed by the current status (About and General pages).
    pub(super) fn update_check_row(&mut self, ui: &mut egui::Ui) {
        let mut check = false;
        let mut open = false;
        ui.horizontal(|ui| {
            let button = egui::Button::new(format!("{} {}", icons::REFRESH, t!(UPDATE_CHECK_BUTTON)));
            if ui.add_enabled(!self.update.busy(), button).clicked() {
                check = true;
            }
            match &self.update {
                UpdateState::Idle => {}
                UpdateState::Checking(_) => {
                    ui.spinner();
                    ui.label(RichText::new(t!(UPDATE_CHECKING)).color(TEXT_DIM));
                }
                UpdateState::UpToDate => {
                    ui.label(RichText::new(format!("{} {}", icons::CHECK, t!(UPDATE_UP_TO_DATE))).color(OK_GREEN));
                }
                UpdateState::Available(rel) | UpdateState::Downloading { rel, .. } => {
                    ui.label(RichText::new(t!(UPDATE_AVAILABLE, version_label(rel))).color(ACCENT));
                    open = ui.small_button(t!(UPDATE_DETAILS)).clicked();
                }
                UpdateState::Installed { rel, .. } => {
                    ui.label(RichText::new(t!(UPDATE_INSTALLED_SHORT, version_label(rel))).color(OK_GREEN));
                    open = ui.small_button(t!(UPDATE_DETAILS)).clicked();
                }
                UpdateState::Failed { rel, error } => {
                    let text = match rel {
                        Some(_) => t!(UPDATE_FAILED, error),
                        None => t!(UPDATE_CHECK_FAILED, error),
                    };
                    ui.label(RichText::new(text).color(WARN_YELLOW));
                    if rel.is_some() {
                        open = ui.small_button(t!(UPDATE_DETAILS)).clicked();
                    }
                }
            }
        });
        if check {
            let ctx = ui.ctx().clone();
            self.start_update_check(&ctx);
        }
        if open {
            self.update_modal = true;
        }
    }

    /// General → Updates: the start-up toggle (saved on change) and the check row.
    pub(super) fn general_update_rows(&mut self, ui: &mut egui::Ui) {
        let before = self.check_updates;
        settings_row(ui, t!(ROW_UPDATES), |ui| {
            ui.checkbox(&mut self.check_updates, t!(UPDATES_CHECKBOX));
        });
        settings_row(ui, "", |ui| {
            ui.label(RichText::new(t!(UPDATES_NOTE)).color(TEXT_DIM).small());
        });
        if before != self.check_updates {
            self.save_settings();
        }
        ui.horizontal(|ui| {
            ui.add_space(130.0 + ui.spacing().item_spacing.x);
            self.update_check_row(ui);
        });
    }

    /// The update modal; shown last in the full layout like the other dialogs.
    pub(super) fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_modal {
            return;
        }
        // One modal at a time: two in the same frame fight over the backdrop and focus.
        if self.format_dialog.is_open() || self.library.confirm_delete.is_some() {
            return;
        }
        let Some(rel) = self.update.release().cloned() else {
            self.update_modal = false;
            return;
        };
        let installed_exe = match &self.update {
            UpdateState::Installed { exe, .. } => Some(exe.clone()),
            _ => None,
        };
        let downloading = matches!(self.update, UpdateState::Downloading { .. });
        let idle = matches!(self.state, State::Idle);
        let local = update::local_version();
        let mut close = false;
        let mut action = Action::None;
        let modal = egui::Modal::new(egui::Id::new("update")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(t!(UPDATE_TITLE));
            ui.add_space(4.0);
            ui.label(
                RichText::new(t!(UPDATE_VERSIONS, version_label(&rel), format!("v{local}"))).color(TEXT_BRIGHT).strong(),
            );
            ui.add_space(8.0);
            ui.label(RichText::new(t!(UPDATE_NOTES)).color(TEXT_DIM));
            egui::Frame::new()
                .fill(NAV_BG)
                .inner_margin(egui::Margin::same(8))
                .corner_radius(egui::CornerRadius::same(3))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        let body = rel.body.trim();
                        if body.is_empty() {
                            ui.label(RichText::new(t!(UPDATE_NO_NOTES)).color(TEXT_DIM));
                        } else {
                            ui.label(RichText::new(body).color(TEXT_NORMAL));
                        }
                    });
                });
            ui.add_space(10.0);
            match &self.update {
                UpdateState::Available(_) => {
                    let can_install = idle && rel.asset.is_some() && update::install_dir_writable();
                    if !can_install {
                        let why = if !idle {
                            key::UPDATE_LOCKED_RECORDING
                        } else if rel.asset.is_none() {
                            key::UPDATE_NO_ASSET
                        } else {
                            key::UPDATE_MANUAL_ONLY
                        };
                        ui.label(RichText::new(i18n::t(why)).color(WARN_YELLOW));
                        ui.add_space(6.0);
                    }
                    ui.horizontal(|ui| {
                        let label = format!("{} {}", icons::DOWNLOAD, t!(UPDATE_DOWNLOAD_INSTALL));
                        let install = egui::Button::new(RichText::new(label).color(TEXT_BRIGHT)).fill(ACCENT);
                        if ui.add_enabled(can_install, install).clicked() {
                            action = Action::Download;
                        }
                        if ui.button(t!(UPDATE_OPEN_PAGE)).clicked() {
                            action = Action::OpenPage;
                        }
                        if ui.button(t!(UPDATE_LATER)).clicked() {
                            close = true;
                        }
                    });
                }
                UpdateState::Downloading { progress, .. } => {
                    let done = progress.downloaded.load(Ordering::Relaxed);
                    let total = rel.asset.as_ref().map(|a| a.size).unwrap_or(0);
                    let fraction = if total > 0 { (done as f32 / total as f32).min(1.0) } else { 0.0 };
                    let text = if total > 0 && done >= total {
                        t!(UPDATE_INSTALLING).to_string()
                    } else {
                        t!(UPDATE_DOWNLOADING, human_bytes(done), human_bytes(total))
                    };
                    ui.add(egui::ProgressBar::new(fraction).text(text).animate(true));
                    ui.add_space(8.0);
                    if ui.button(t!(CANCEL)).clicked() {
                        action = Action::Cancel;
                    }
                }
                UpdateState::Installed { .. } => {
                    ui.label(RichText::new(t!(UPDATE_INSTALLED, version_label(&rel))).color(OK_GREEN));
                    if !idle {
                        ui.label(RichText::new(t!(UPDATE_LOCKED_RECORDING)).color(WARN_YELLOW));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let restart = egui::Button::new(RichText::new(t!(UPDATE_RESTART_NOW)).color(TEXT_BRIGHT)).fill(ACCENT);
                        if ui.add_enabled(idle, restart).clicked() {
                            action = Action::Restart;
                        }
                        if ui.button(t!(UPDATE_LATER)).clicked() {
                            close = true;
                        }
                    });
                }
                UpdateState::Failed { error, .. } => {
                    ui.label(RichText::new(t!(UPDATE_FAILED, error)).color(ERR_RED));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if rel.asset.is_some() && ui.add_enabled(idle, egui::Button::new(t!(UPDATE_RETRY))).clicked() {
                            action = Action::Retry;
                        }
                        if ui.button(t!(UPDATE_OPEN_PAGE)).clicked() {
                            action = Action::OpenPage;
                        }
                        if ui.button(t!(OK)).clicked() {
                            close = true;
                        }
                    });
                }
                UpdateState::Idle | UpdateState::Checking(_) | UpdateState::UpToDate => close = true,
            }
        });
        match action {
            Action::None => {}
            Action::Download => self.start_update_download(ctx),
            Action::Retry => {
                if let UpdateState::Failed { rel: Some(rel), .. } = std::mem::replace(&mut self.update, UpdateState::Idle) {
                    self.update = UpdateState::Available(rel);
                    self.start_update_download(ctx);
                }
            }
            Action::Cancel => self.cancel_update_download(),
            Action::OpenPage => ctx.open_url(egui::OpenUrl::new_tab(&rel.html_url)),
            Action::Restart => {
                if let Some(exe) = installed_exe {
                    match update::relaunch(&exe) {
                        Ok(()) => {
                            log::info!("restarting into the updated executable");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => self.message = Some((t!(UPDATE_RESTART_FAILED, e), true)),
                    }
                }
            }
        }
        // Clicking outside / Esc closes the dialog, except while a download is
        // running (Cancel is explicit so the progress is never orphaned).
        if close || (modal.should_close() && !downloading) {
            self.update_modal = false;
        }
    }
}
