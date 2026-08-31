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
use super::widgets::*;
use super::{App, State, human_bytes, icons};
use crate::i18n::{self, key};
use crate::t;
use crate::update::{self, Progress, Release};

/// State of the game-capture sidecar beside the executable.
///
/// The updater shipped before the hook did, and the version that shipped first
/// extracted the executable alone. Every install that reached the current build
/// through it therefore has no `openclip_hook64.dll` — and, being up to date,
/// is never offered another download. This is the way back: the same release
/// archive, with only the sidecar taken out of it.
pub(super) enum RepairState {
    /// The sidecar is there, or this platform has none.
    Present,
    /// Missing. Waiting on a release to take one from.
    Missing,
    Downloading { progress: Arc<Progress>, rx: mpsc::Receiver<anyhow::Result<()>> },
    /// Back on disk. It is only picked up on the next launch: the DLL is loaded
    /// once per process and deliberately never unloaded.
    Restored,
    Failed(String),
}

impl RepairState {
    /// Whether the user should be shown a way to fix this.
    pub(super) fn wanted(&self) -> bool {
        matches!(self, RepairState::Missing | RepairState::Failed(_))
    }

    fn busy(&self) -> bool {
        matches!(self, RepairState::Downloading { .. })
    }
}

pub(super) enum UpdateState {
    /// Not checked (yet) — the start-up check is off or still to be spawned.
    Idle,
    Checking(mpsc::Receiver<anyhow::Result<Release>>),
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
    ///
    /// One request answers two questions — whether there is a newer build, and
    /// where to get a replacement sidecar from — so a missing DLL costs no
    /// second round trip.
    pub(super) fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.update.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("openclip-update".into()).spawn(move || {
            let _ = tx.send(update::latest());
            ctx.request_repaint();
        });
        self.update = match spawned {
            Ok(_) => UpdateState::Checking(rx),
            Err(e) => UpdateState::Failed { rel: None, error: e.to_string() },
        };
    }

    /// Picks up results from the check / download threads (called every frame).
    pub(super) fn poll_update(&mut self, ctx: &egui::Context) {
        self.poll_repair(ctx);
        let state = std::mem::replace(&mut self.update, UpdateState::Idle);
        self.update = match state {
            UpdateState::Checking(rx) => match rx.try_recv() {
                Ok(Ok(rel)) => {
                    // The same release answers the sidecar question, but only
                    // when it *is* this build: a DLL from a different release
                    // than the running exe is what the ABI check rejects.
                    if self.repair.wanted() && rel.version == update::local_version() {
                        self.start_sidecar_repair(&rel, ctx);
                    }
                    if update::is_newer(&rel.version, &update::local_version()) {
                        log::info!("update available: {} ({})", rel.version, rel.html_url);
                        self.message = Some((t!(MSG_UPDATE_AVAILABLE, version_label(&rel)), false));
                        UpdateState::Available(Box::new(rel))
                    } else {
                        UpdateState::UpToDate
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("update check failed: {e:#}");
                    if self.repair.wanted() {
                        self.repair = RepairState::Failed(format!("{e:#}"));
                    }
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
        // The hook DLL is mapped into every game it was injected into and cannot
        // be swapped underneath them. Refusing here removes the whole class of
        // exe-and-hook-from-different-builds problem.
        if self.game_armed() {
            self.message = Some((crate::t!(MSG_GAME_HOOKED_CANNOT_UPDATE).into(), true));
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

    /// Downloads the release archive and puts the missing sidecar back.
    ///
    /// Gated exactly like an update: the DLL is mapped into openclip itself
    /// from the first injection onward and into every game it reached, so it
    /// must not be swapped while Game mode is armed.
    fn start_sidecar_repair(&mut self, rel: &Release, ctx: &egui::Context) {
        if self.repair.busy() || rel.asset.is_none() {
            return;
        }
        if !matches!(self.state, State::Idle) || self.game_armed() || !update::install_dir_writable() {
            return;
        }
        let rel = rel.clone();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let (progress2, ctx2) = (progress.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("openclip-repair".into()).spawn(move || {
            let _ = tx.send(update::repair_sidecar(&rel, &progress2));
            ctx2.request_repaint();
        });
        self.repair = match spawned {
            Ok(_) => RepairState::Downloading { progress, rx },
            Err(e) => RepairState::Failed(e.to_string()),
        };
    }

    /// The Repair button: ask GitHub again, which restarts the whole flow.
    pub(super) fn retry_sidecar_repair(&mut self, ctx: &egui::Context) {
        if self.repair.busy() {
            return;
        }
        self.repair = RepairState::Missing;
        self.start_update_check(ctx);
    }

    fn poll_repair(&mut self, ctx: &egui::Context) {
        let state = std::mem::replace(&mut self.repair, RepairState::Present);
        self.repair = match state {
            RepairState::Downloading { progress, rx } => match rx.try_recv() {
                Ok(Ok(())) => {
                    log::info!("the game-capture component was restored");
                    self.message = Some((t!(MSG_HOOK_REPAIRED).into(), false));
                    RepairState::Restored
                }
                Ok(Err(e)) => {
                    log::warn!("could not restore the game-capture component: {e:#}");
                    RepairState::Failed(format!("{e:#}"))
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    RepairState::Downloading { progress, rx }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    RepairState::Failed("the download did not finish".into())
                }
            },
            other => other,
        };
    }

    /// What the Game card says about the sidecar, if anything.
    pub(super) fn repair_status(&self) -> Option<(String, egui::Color32)> {
        match &self.repair {
            RepairState::Present => None,
            RepairState::Missing => Some((t!(HOOK_MISSING).into(), ORANGE)),
            RepairState::Downloading { progress, .. } => {
                Some((t!(HOOK_REPAIRING, human_bytes(progress.downloaded.load(Ordering::Relaxed))), BLUE))
            }
            RepairState::Restored => Some((t!(HOOK_REPAIRED_RESTART).into(), GREEN)),
            RepairState::Failed(e) => Some((t!(HOOK_REPAIR_FAILED, e), ORANGE)),
        }
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
        let text = format!("{}  {}", icons::DOWNLOAD, t!(UPDATE_CHIP, version_label(rel)));
        if capsule_button(ui, Tint::Blue, &text, t!(UPDATE_CHIP_TIP)).clicked() {
            self.update_modal = true;
        }
    }

    /// "Check for updates" button followed by the current status (About and General pages).
    pub(super) fn update_check_row(&mut self, ui: &mut egui::Ui) {
        let mut check = false;
        let mut open = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.update.busy(), |ui| {
                if tinted_button_small(ui, &format!("{}  {}", icons::REFRESH, t!(UPDATE_CHECK_BUTTON))).clicked() {
                    check = true;
                }
            });
            match &self.update {
                UpdateState::Idle => {}
                UpdateState::Checking(_) => {
                    ui.spinner();
                    ui.label(RichText::new(t!(UPDATE_CHECKING)).color(LABEL_2));
                }
                UpdateState::UpToDate => {
                    ui.label(RichText::new(format!("{} {}", icons::CHECK, t!(UPDATE_UP_TO_DATE))).color(GREEN));
                }
                UpdateState::Available(rel) | UpdateState::Downloading { rel, .. } => {
                    ui.label(RichText::new(t!(UPDATE_AVAILABLE, version_label(rel))).color(BLUE));
                    open = tinted_button_small(ui, t!(UPDATE_DETAILS)).clicked();
                }
                UpdateState::Installed { rel, .. } => {
                    ui.label(RichText::new(t!(UPDATE_INSTALLED_SHORT, version_label(rel))).color(GREEN));
                    open = tinted_button_small(ui, t!(UPDATE_DETAILS)).clicked();
                }
                UpdateState::Failed { rel, error } => {
                    let text = match rel {
                        Some(_) => t!(UPDATE_FAILED, error),
                        None => t!(UPDATE_CHECK_FAILED, error),
                    };
                    ui.label(RichText::new(text).color(ORANGE));
                    if rel.is_some() {
                        open = tinted_button_small(ui, t!(UPDATE_DETAILS)).clicked();
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

    /// About → Updates: the start-up toggle (saved on change), the check row
    /// and — only when there is something to say — the sidecar's state.
    pub(super) fn about_update_rows(&mut self, ui: &mut egui::Ui) {
        let before = self.check_updates;
        let ctx = ui.ctx().clone();
        let mut fix = false;
        Card::show(ui, |card| {
            switch_row(card, t!(UPDATES_CHECKBOX), &mut self.check_updates);
            card.row_inline("", |ui| self.update_check_row(ui));
            // Reachable without arming Game mode, which is the state an install
            // with no sidecar is stuck in.
            if let Some((text, colour)) = self.repair_status() {
                let offer = self.repair.wanted();
                card.row_inline("", |ui| {
                    ui.label(RichText::new(text).color(colour));
                    if offer {
                        fix = tinted_button_small(ui, t!(HOOK_REPAIR_BUTTON)).clicked();
                    }
                });
            }
        });
        footnote(ui, t!(UPDATES_NOTE));
        if fix {
            self.retry_sidecar_repair(&ctx);
        }
        if before != self.check_updates {
            self.save_settings();
        }
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
        let idle = matches!(self.state, State::Idle) && !self.game_armed();
        let local = update::local_version();
        let mut close = false;
        let mut action = Action::None;
        let modal = egui::Modal::new(egui::Id::new("update")).frame(sheet_frame()).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(heading(t!(UPDATE_TITLE)));
            ui.add_space(4.0);
            ui.label(RichText::new(t!(UPDATE_VERSIONS, version_label(&rel), format!("v{local}"))).color(LABEL));
            ui.add_space(8.0);
            ui.label(RichText::new(t!(UPDATE_NOTES)).color(LABEL_2));
            egui::Frame::new()
                .fill(FILL)
                .inner_margin(egui::Margin::same(12))
                .corner_radius(egui::CornerRadius::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        let body = rel.body.trim();
                        if body.is_empty() {
                            ui.label(RichText::new(t!(UPDATE_NO_NOTES)).color(LABEL_2));
                        } else {
                            ui.label(RichText::new(body).color(LABEL));
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
                        ui.label(RichText::new(i18n::t(why)).color(ORANGE));
                        ui.add_space(6.0);
                    }
                    ui.horizontal(|ui| {
                        let label = format!("{}  {}", icons::DOWNLOAD, t!(UPDATE_DOWNLOAD_INSTALL));
                        ui.add_enabled_ui(can_install, |ui| {
                            if primary_button(ui, &label).clicked() {
                                action = Action::Download;
                            }
                        });
                        if tinted_button(ui, t!(UPDATE_OPEN_PAGE)).clicked() {
                            action = Action::OpenPage;
                        }
                        if gray_button(ui, t!(UPDATE_LATER)).clicked() {
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
                    ui.add(egui::ProgressBar::new(fraction).text(text).animate(true).corner_radius(6.0));
                    ui.add_space(8.0);
                    if gray_button(ui, t!(CANCEL)).clicked() {
                        action = Action::Cancel;
                    }
                }
                UpdateState::Installed { .. } => {
                    ui.label(RichText::new(t!(UPDATE_INSTALLED, version_label(&rel))).color(GREEN));
                    if !idle {
                        ui.label(RichText::new(t!(UPDATE_LOCKED_RECORDING)).color(ORANGE));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(idle, |ui| {
                            if primary_button(ui, t!(UPDATE_RESTART_NOW)).clicked() {
                                action = Action::Restart;
                            }
                        });
                        if gray_button(ui, t!(UPDATE_LATER)).clicked() {
                            close = true;
                        }
                    });
                }
                UpdateState::Failed { error, .. } => {
                    ui.label(RichText::new(t!(UPDATE_FAILED, error)).color(RED));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if rel.asset.is_some() {
                            ui.add_enabled_ui(idle, |ui| {
                                if primary_button(ui, t!(UPDATE_RETRY)).clicked() {
                                    action = Action::Retry;
                                }
                            });
                        }
                        if tinted_button(ui, t!(UPDATE_OPEN_PAGE)).clicked() {
                            action = Action::OpenPage;
                        }
                        if gray_button(ui, t!(OK)).clicked() {
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
