//! "Format settings" modal, laid out like classic recorders: File Type,
//! Video (size / fps / codec / quality / profile) and Audio (codec / bitrate /
//! channels / frequency). Edits go to a draft that is committed on OK.

use eframe::egui::{self, Align, Layout, RichText, Vec2};

use super::theme::*;
use super::widgets::*;
use crate::settings::{
    AudioCodec, Container, FormatSettings, H264Profile, HevcProfile, RateControl, SizeMode, VideoCodec,
    AAC_BITRATES, FPS_PRESETS, QUALITY_STEPS, SAMPLE_RATES, SIZE_PRESETS,
};
use crate::t;
use crate::video::encoder::EncoderInfo;

/// Content width of the dialog sheet.
const DIALOG_W: f32 = 480.0;
/// Fixed label column and field widths so every row lines up.
const LABEL_W: f32 = 110.0;
const FIELD_W: f32 = 200.0;
/// Trailing column (…, ?, units, hints) right of the field.
const TRAIL_W: f32 = 96.0;

pub enum DialogOutcome {
    None,
    Ok(FormatSettings),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advanced {
    Quality,
    Codec,
}

/// Which half of the settings a dialog instance edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatSection {
    Video,
    Audio,
}

pub struct FormatDialog {
    open: bool,
    section: FormatSection,
    draft: FormatSettings,
    /// The FPS combo shows a free-form field after "Custom…" was picked.
    custom_fps: bool,
    advanced: Option<Advanced>,
    notes: Vec<String>,
    rescan: bool,
}

impl Default for FormatDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatDialog {
    pub fn new() -> Self {
        Self {
            open: false,
            section: FormatSection::Video,
            draft: FormatSettings::default(),
            custom_fps: false,
            advanced: None,
            notes: Vec::new(),
            rescan: false,
        }
    }

    pub fn open(&mut self, current: &FormatSettings, encoders: &[EncoderInfo], section: FormatSection) {
        self.section = section;
        self.draft = current.clone();
        self.notes = self.draft.normalize(encoders);
        self.custom_fps = !FPS_PRESETS.contains(&self.draft.fps);
        self.advanced = None;
        self.open = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// True once after the user asked for the encoder list to be refreshed.
    pub fn take_rescan(&mut self) -> bool {
        std::mem::take(&mut self.rescan)
    }

    /// Draws the dialog while open. Returns `Ok` exactly once when OK is pressed.
    /// In the full window it is a modal sheet; in compact mode (the mini bar is
    /// far too small for a sheet) it opens as its own always-on-top window.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        encoders: &[EncoderInfo],
        source: Option<(u32, u32)>,
        recording: bool,
        compact: bool,
    ) -> DialogOutcome {
        if !self.open {
            return DialogOutcome::None;
        }
        let before = self.draft.clone();
        let mut outcome = DialogOutcome::None;
        if compact {
            let size = match self.section {
                FormatSection::Video => [DIALOG_W + 40.0, 470.0],
                FormatSection::Audio => [DIALOG_W + 40.0, 420.0],
            };
            let builder = egui::ViewportBuilder::default()
                .with_title(self.title())
                .with_inner_size(size)
                .with_resizable(false)
                .with_minimize_button(false)
                .with_maximize_button(false)
                .with_always_on_top();
            ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("format-dialog"), builder, |ctx, _| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(CARD).inner_margin(egui::Margin::same(20)))
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            outcome = self.body(ui, encoders, source, recording);
                        });
                    });
                if let Some(adv) = self.advanced {
                    self.show_advanced(ctx, adv, encoders, source);
                }
                if ctx.input(|i| i.viewport().close_requested()) && matches!(outcome, DialogOutcome::None) {
                    outcome = DialogOutcome::Cancel;
                }
            });
        } else {
            let modal = egui::Modal::new(egui::Id::new("format-settings")).frame(sheet_frame()).show(ctx, |ui| {
                ui.set_width(DIALOG_W);
                outcome = self.body(ui, encoders, source, recording);
            });
            if let Some(adv) = self.advanced {
                self.show_advanced(ctx, adv, encoders, source);
            } else if modal.should_close() && matches!(outcome, DialogOutcome::None) {
                outcome = DialogOutcome::Cancel;
            }
        }
        if self.draft != before {
            self.notes = self.draft.normalize(encoders);
        }
        if !matches!(outcome, DialogOutcome::None) {
            self.open = false;
            self.advanced = None;
        }
        outcome
    }

    fn title(&self) -> String {
        let what = match self.section {
            FormatSection::Video => t!(BOX_VIDEO),
            FormatSection::Audio => t!(BOX_AUDIO),
        };
        format!("{} – {what}", t!(FMT_TITLE))
    }

    /// Heading, file type, the section's rows, notes and the OK / Cancel pair.
    fn body(&mut self, ui: &mut egui::Ui, encoders: &[EncoderInfo], source: Option<(u32, u32)>, recording: bool) -> DialogOutcome {
        let mut outcome = DialogOutcome::None;
        ui.label(heading(self.title()));
        ui.add_space(4.0);

        group(ui, t!(FMT_GROUP_FILE_TYPE), |ui| {
            row_wide(ui, "", |ui| {
                segmented(
                    ui,
                    "fmt-container",
                    &[(Container::Avi, None, "AVI"), (Container::Mp4, None, "MP4")],
                    &mut self.draft.container,
                );
            });
        });

        match self.section {
            FormatSection::Video => group(ui, t!(BOX_VIDEO), |ui| self.video_rows(ui, encoders, source)),
            FormatSection::Audio => group(ui, t!(BOX_AUDIO), |ui| self.audio_rows(ui)),
        }

        for n in &self.notes {
            footnote(ui, n);
        }
        if recording {
            footnote(ui, t!(FMT_LOCKED));
        }
        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_enabled_ui(!recording, |ui| {
                if primary_button_min(ui, t!(OK), 110.0).clicked() {
                    self.notes = self.draft.normalize(encoders);
                    outcome = DialogOutcome::Ok(self.draft.clone());
                }
            });
            if gray_button_min(ui, t!(CANCEL), 110.0).clicked() {
                outcome = DialogOutcome::Cancel;
            }
        });
        outcome
    }

    fn video_rows(&mut self, ui: &mut egui::Ui, encoders: &[EncoderInfo], source: Option<(u32, u32)>) {
        let d = &mut self.draft;
        let is_percent = matches!(d.size, SizeMode::Percent { .. });
        let size_hint = source.map(|(w, h)| {
            let (ow, oh) = d.size.resolve(w, h);
            format!("{ow}×{oh}")
        });
        row(
            ui,
            t!(FMT_ROW_SIZE),
            |ui| {
                egui::ComboBox::from_id_salt("fmt-size").width(FIELD_W).truncate().selected_text(d.size.label()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.size, SizeMode::Full, t!(FMT_FULL_SIZE));
                    ui.selectable_value(&mut d.size, SizeMode::Half, t!(FMT_HALF_SIZE));
                    for (w, h) in SIZE_PRESETS {
                        ui.selectable_value(&mut d.size, SizeMode::Preset { width: w, height: h }, format!("{w}×{h}"));
                    }
                    if ui.selectable_label(is_percent, t!(FMT_CUSTOM_PERCENT)).clicked() && !is_percent {
                        d.size = SizeMode::Percent { x: 100, y: 100 };
                    }
                });
            },
            |ui| {
                if let Some(hint) = size_hint {
                    ui.label(RichText::new(hint).color(LABEL_2).small());
                }
            },
        );
        if let SizeMode::Percent { x, y } = &mut d.size {
            row(
                ui,
                "",
                |ui| {
                    ui.add(egui::DragValue::new(x).range(10..=100).suffix(" %"));
                    ui.label(RichText::new("×").color(LABEL_2));
                    ui.add(egui::DragValue::new(y).range(10..=100).suffix(" %"));
                },
                |_| {},
            );
        }

        let custom_fps = &mut self.custom_fps;
        let show_custom = *custom_fps;
        let mut fps_edit = d.fps;
        row(
            ui,
            t!(FMT_ROW_FPS),
            |ui| {
                let label = if *custom_fps { t!(FMT_CUSTOM_FPS, d.fps) } else { d.fps.to_string() };
                egui::ComboBox::from_id_salt("fmt-fps").width(FIELD_W).truncate().selected_text(label).show_ui(ui, |ui| {
                    for f in FPS_PRESETS {
                        if ui.selectable_label(!*custom_fps && d.fps == f, f.to_string()).clicked() {
                            d.fps = f;
                            *custom_fps = false;
                        }
                    }
                    if ui.selectable_label(*custom_fps, t!(FMT_CUSTOM)).clicked() {
                        *custom_fps = true;
                    }
                });
            },
            |ui| {
                if show_custom {
                    ui.add(egui::DragValue::new(&mut fps_edit).range(1..=240).suffix(" fps"));
                }
            },
        );
        if show_custom && fps_edit != d.fps {
            d.fps = fps_edit;
        }

        let advanced = &mut self.advanced;
        row(
            ui,
            t!(FMT_ROW_CODEC),
            |ui| {
                let current = d.video_codec.label(encoders);
                egui::ComboBox::from_id_salt("fmt-vcodec").width(FIELD_W).truncate().selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.video_codec, VideoCodec::Auto, VideoCodec::Auto.label(encoders))
                        .on_hover_text(t!(FMT_AUTO_TIP));
                    ui.selectable_value(&mut d.video_codec, VideoCodec::OpenH264, VideoCodec::OpenH264.label(encoders));
                    for e in encoders {
                        let codec = e.codec();
                        let resp = ui.selectable_label(d.video_codec == codec, &e.label).on_hover_text(&e.friendly_name);
                        if resp.clicked() {
                            d.video_codec = codec;
                        }
                    }
                    if d.video_codec.needs_mf() && d.video_codec.info(encoders).is_none() {
                        ui.add_enabled(false, egui::Button::selectable(true, d.video_codec.label(encoders)))
                            .on_disabled_hover_text(t!(FMT_ENCODER_MISSING));
                    }
                    if encoders.is_empty() {
                        ui.label(
                            RichText::new(if FormatSettings::platform_has_mf() {
                                t!(FMT_NO_MF_ENCODERS)
                            } else {
                                t!(FMT_MF_WINDOWS_ONLY)
                            })
                            .color(LABEL_2)
                            .small(),
                        );
                    }
                });
            },
            |ui| {
                if tinted_button_small(ui, "…").on_hover_text(t!(FMT_ENCODER_DETAILS)).clicked() {
                    *advanced = Some(Advanced::Codec);
                }
            },
        );

        row(
            ui,
            t!(FMT_ROW_QUALITY),
            |ui| {
                let label = match d.rate_control {
                    RateControl::Quality(q) => q.to_string(),
                    RateControl::ConstantBitrate { kbps } => t!(CBR_LABEL, kbps),
                };
                egui::ComboBox::from_id_salt("fmt-quality").width(FIELD_W).truncate().selected_text(label).show_ui(ui, |ui| {
                    for q in QUALITY_STEPS {
                        let text = match q {
                            100 => t!(FMT_QUALITY_BEST).to_string(),
                            10 => t!(FMT_QUALITY_SMALLEST).to_string(),
                            _ => q.to_string(),
                        };
                        ui.selectable_value(&mut d.rate_control, RateControl::Quality(q), text);
                    }
                });
            },
            |ui| {
                if tinted_button_small(ui, "…").on_hover_text(t!(FMT_BITRATE_TIP)).clicked() {
                    *advanced = Some(Advanced::Quality);
                }
            },
        );

        row(
            ui,
            t!(FMT_ROW_PROFILE),
            |ui| {
                if d.video_codec.is_hevc() {
                    egui::ComboBox::from_id_salt("fmt-profile-hevc")
                        .width(FIELD_W)
                        .truncate()
                        .selected_text(d.profiles.hevc.label())
                        .show_ui(ui, |ui| {
                            for p in HevcProfile::ALL {
                                ui.selectable_value(&mut d.profiles.hevc, p, p.label());
                            }
                        });
                } else {
                    egui::ComboBox::from_id_salt("fmt-profile-h264")
                        .width(FIELD_W)
                        .truncate()
                        .selected_text(d.profiles.h264.label())
                        .show_ui(ui, |ui| {
                            for p in H264Profile::ALL {
                                ui.selectable_value(&mut d.profiles.h264, p, p.label());
                            }
                        });
                }
            },
            |ui| {
                tinted_button_small(ui, "?").on_hover_text(t!(FMT_PROFILE_HELP));
            },
        );
    }

    fn audio_rows(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        row(
            ui,
            t!(FMT_ROW_CODEC),
            |ui| {
                egui::ComboBox::from_id_salt("fmt-acodec").width(FIELD_W).truncate().selected_text(d.audio_codec.label()).show_ui(
                    ui,
                    |ui| {
                        for c in AudioCodec::ALL {
                            let (available, why) = match c {
                                AudioCodec::Mp3 => (true, ""),
                                AudioCodec::Aac => (FormatSettings::platform_has_mf(), t!(FMT_AAC_NEEDS_WINDOWS)),
                                AudioCodec::Pcm => (d.container == Container::Avi, t!(FMT_PCM_AVI_ONLY)),
                            };
                            let resp = ui.add_enabled(available, egui::Button::selectable(d.audio_codec == c, c.label()));
                            if resp.clicked() {
                                d.audio_codec = c;
                                if !c.allowed_bitrates().is_empty() && !c.allowed_bitrates().contains(&d.audio_bitrate_kbps) {
                                    d.audio_bitrate_kbps = if c == AudioCodec::Aac { AAC_BITRATES[2] } else { 160 };
                                }
                            }
                            if !available {
                                resp.on_disabled_hover_text(why);
                            }
                        }
                    },
                );
            },
            |_| {},
        );
        let allowed = d.audio_codec.allowed_bitrates();
        row(
            ui,
            t!(FMT_ROW_BITRATE),
            |ui| {
                ui.add_enabled_ui(!allowed.is_empty(), |ui| {
                    let label = if allowed.is_empty() { "—".to_string() } else { d.audio_bitrate_kbps.to_string() };
                    egui::ComboBox::from_id_salt("fmt-abitrate").width(FIELD_W).truncate().selected_text(label).show_ui(ui, |ui| {
                        for &b in allowed {
                            ui.selectable_value(&mut d.audio_bitrate_kbps, b, b.to_string());
                        }
                    });
                });
            },
            |ui| {
                ui.label(RichText::new("kbps").color(LABEL_2));
            },
        );
        row(
            ui,
            t!(FMT_ROW_CHANNELS),
            |ui| {
                let label = if d.audio_channels == 1 { t!(MONO) } else { t!(STEREO) };
                egui::ComboBox::from_id_salt("fmt-channels").width(FIELD_W).truncate().selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.audio_channels, 1, t!(MONO));
                    ui.selectable_value(&mut d.audio_channels, 2, t!(STEREO));
                });
            },
            |_| {},
        );
        row(
            ui,
            t!(FMT_ROW_FREQUENCY),
            |ui| {
                egui::ComboBox::from_id_salt("fmt-rate")
                    .width(FIELD_W)
                    .truncate()
                    .selected_text(d.audio_sample_rate.to_string())
                    .show_ui(ui, |ui| {
                        for r in SAMPLE_RATES {
                            ui.selectable_value(&mut d.audio_sample_rate, r, r.to_string());
                        }
                    });
            },
            |ui| {
                ui.label(RichText::new("Hz").color(LABEL_2));
            },
        );
    }

    fn show_advanced(&mut self, ctx: &egui::Context, which: Advanced, encoders: &[EncoderInfo], source: Option<(u32, u32)>) {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("format-settings-advanced")).frame(sheet_frame()).show(ctx, |ui| {
            ui.set_width(420.0);
            match which {
                Advanced::Quality => {
                    ui.label(heading(t!(FMT_ROW_BITRATE)));
                    ui.add_space(8.0);
                    let d = &mut self.draft;
                    let mut quality_mode = matches!(d.rate_control, RateControl::Quality(_));
                    let items = [(true, None, t!(FMT_QUALITY_MODE)), (false, None, t!(FMT_CBR_MODE))];
                    if segmented(ui, "fmt-rate-mode", &items, &mut quality_mode) {
                        d.rate_control = if quality_mode {
                            RateControl::Quality(80)
                        } else {
                            let kbps = source.map(|(w, h)| d.target_bitrate_kbps(w, h)).unwrap_or(6000);
                            RateControl::ConstantBitrate { kbps }
                        };
                    }
                    ui.add_space(8.0);
                    match &mut d.rate_control {
                        RateControl::Quality(q) => {
                            row_wide(ui, t!(FMT_ROW_QUALITY), |ui| {
                                ui.add(egui::Slider::new(q, 10..=100).step_by(10.0));
                            });
                        }
                        RateControl::ConstantBitrate { kbps } => {
                            row_wide(ui, t!(FMT_ROW_BITRATE), |ui| {
                                ui.add(egui::DragValue::new(kbps).range(200..=100_000).suffix(" kbps").speed(50));
                            });
                        }
                    }
                    if let Some((w, h)) = source {
                        let (ow, oh) = d.size.resolve(w, h);
                        ui.label(
                            RichText::new(t!(FMT_BITRATE_ESTIMATE, d.target_bitrate_kbps(ow, oh), ow, oh, d.fps))
                                .color(LABEL_2)
                                .small(),
                        );
                    }
                    row_wide(ui, t!(FMT_ROW_KEYFRAME), |ui| {
                        ui.add(egui::DragValue::new(&mut d.keyframe_interval_s).range(0.5..=10.0).suffix(" s").speed(0.1));
                    });
                    ui.label(RichText::new(t!(FMT_RATE_CONTROL_NOTE)).color(LABEL_2).small());
                }
                Advanced::Codec => {
                    ui.label(heading(t!(FMT_ENCODER_DETAILS)));
                    ui.add_space(8.0);
                    let codec = self.draft.video_codec.clone();
                    match codec.info(encoders) {
                        Some(info) => {
                            row_wide(ui, t!(FMT_ROW_ENCODER), |ui| {
                                ui.label(RichText::new(&info.label).color(LABEL));
                            });
                            row_wide(ui, t!(FMT_ROW_VENDOR), |ui| {
                                ui.label(info.vendor.label());
                            });
                            row_wide(ui, t!(FMT_ROW_HARDWARE), |ui| {
                                ui.label(if info.hardware { t!(YES_GPU) } else { t!(NO_CPU) });
                            });
                            row_wide(ui, t!(FMT_ROW_TRANSFORM), |ui| {
                                ui.label(RichText::new(&info.friendly_name).small());
                            });
                            row_wide(ui, "CLSID", |ui| {
                                ui.label(RichText::new(&info.clsid).monospace().small());
                            });
                        }
                        None => {
                            row_wide(ui, t!(FMT_ROW_ENCODER), |ui| {
                                ui.label(RichText::new(codec.generic_label()).color(LABEL));
                            });
                            row_wide(ui, t!(FMT_ROW_DETAILS), |ui| {
                                ui.label(if codec == VideoCodec::OpenH264 {
                                    t!(FMT_OPENH264_DETAILS)
                                } else {
                                    t!(FMT_NOT_FOUND)
                                });
                            });
                        }
                    }
                    ui.add_space(6.0);
                    if FormatSettings::platform_has_mf() {
                        let found = encoders.len();
                        ui.label(RichText::new(t!(FMT_MF_COUNT, found)).color(LABEL_2).small());
                        if ui.button(t!(FMT_RESCAN)).clicked() {
                            self.rescan = true;
                        }
                    }
                }
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if gray_button(ui, t!(CLOSE)).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.advanced = None;
        }
    }
}

/// Section header plus an elevated card (the sheet is `CARD`, so rows sit on `FILL`).
fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    section_header(ui, title);
    Card::show_with(ui, FILL, |card| {
        card.flush(|ui| {
            ui.add_space(4.0);
            add(ui);
            ui.add_space(4.0);
        });
    });
}

/// Label column followed by free-flowing controls (file type, advanced dialogs).
fn row_wide(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(Vec2::new(width, 36.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_height(36.0);
        ui.add_space(PAD);
        ui.allocate_ui_with_layout(Vec2::new(LABEL_W, 24.0), Layout::left_to_right(Align::Center), |ui| {
            ui.label(RichText::new(label).color(LABEL));
        });
        add(ui);
    });
}

/// Three fixed columns — label | field | trailing — so every row starts and
/// ends at the same x. The field and trailing slots are right-aligned.
fn row(ui: &mut egui::Ui, label: &str, field: impl FnOnce(&mut egui::Ui), trailing: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(Vec2::new(width, 38.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_height(38.0);
        ui.add_space(PAD);
        ui.label(RichText::new(label).color(LABEL));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(PAD);
            ui.allocate_ui_with_layout(Vec2::new(TRAIL_W, 30.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(TRAIL_W);
                ui.set_max_width(TRAIL_W);
                trailing(ui);
            });
            ui.allocate_ui_with_layout(Vec2::new(FIELD_W, 30.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(FIELD_W);
                ui.set_max_width(FIELD_W);
                field(ui);
            });
        });
    });
}
