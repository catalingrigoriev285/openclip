//! "Format settings" modal, laid out like classic recorders: File Type,
//! Video (size / fps / codec / quality / profile) and Audio (codec / bitrate /
//! channels / frequency). Edits go to a draft that is committed on OK.

use eframe::egui::{self, Align, Layout, Margin, RichText, Vec2};

use super::theme::*;
use crate::settings::{
    AudioCodec, Container, FormatSettings, H264Profile, HevcProfile, RateControl, SizeMode, VideoCodec,
    AAC_BITRATES, FPS_PRESETS, QUALITY_STEPS, SAMPLE_RATES, SIZE_PRESETS,
};
use crate::t;
use crate::video::encoder::EncoderInfo;

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

pub struct FormatDialog {
    open: bool,
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
            draft: FormatSettings::default(),
            custom_fps: false,
            advanced: None,
            notes: Vec::new(),
            rescan: false,
        }
    }

    pub fn open(&mut self, current: &FormatSettings, encoders: &[EncoderInfo]) {
        self.draft = current.clone();
        self.notes = self.draft.normalize(encoders);
        self.custom_fps = !FPS_PRESETS.contains(&self.draft.fps);
        self.advanced = None;
        self.open = true;
    }

    /// True once after the user asked for the encoder list to be refreshed.
    pub fn take_rescan(&mut self) -> bool {
        std::mem::take(&mut self.rescan)
    }

    /// Draws the modal while open. Returns `Ok` exactly once when OK is pressed.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        encoders: &[EncoderInfo],
        source: Option<(u32, u32)>,
        recording: bool,
    ) -> DialogOutcome {
        if !self.open {
            return DialogOutcome::None;
        }
        let before = self.draft.clone();
        let mut outcome = DialogOutcome::None;
        let modal = egui::Modal::new(egui::Id::new("format-settings")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading(t!(FMT_TITLE));
            ui.add_space(8.0);

            group(ui, t!(FMT_GROUP_FILE_TYPE), |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(150.0);
                    ui.radio_value(&mut self.draft.container, Container::Avi, "AVI");
                    ui.add_space(60.0);
                    ui.radio_value(&mut self.draft.container, Container::Mp4, "MP4");
                });
            });

            group(ui, t!(BOX_VIDEO), |ui| {
                self.video_rows(ui, encoders, source);
            });

            group(ui, t!(BOX_AUDIO), |ui| {
                self.audio_rows(ui);
            });

            for n in &self.notes {
                ui.label(RichText::new(n).color(WARN_YELLOW).small());
            }
            if recording {
                ui.label(RichText::new(t!(FMT_LOCKED)).color(WARN_YELLOW).small());
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.hyperlink_to(t!(FMT_HELP), "https://github.com/catalingrigoriev285/openclip#format-settings");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(t!(CANCEL)).min_size(Vec2::new(100.0, 28.0))).clicked() {
                        outcome = DialogOutcome::Cancel;
                    }
                    let ok = egui::Button::new(RichText::new(t!(OK)).color(TEXT_BRIGHT)).min_size(Vec2::new(100.0, 28.0));
                    if ui.add_enabled(!recording, ok).clicked() {
                        self.notes = self.draft.normalize(encoders);
                        outcome = DialogOutcome::Ok(self.draft.clone());
                    }
                });
            });
        });
        if self.draft != before {
            self.notes = self.draft.normalize(encoders);
        }
        if let Some(adv) = self.advanced {
            self.show_advanced(ctx, adv, encoders, source);
        } else if modal.should_close() && matches!(outcome, DialogOutcome::None) {
            outcome = DialogOutcome::Cancel;
        }
        if !matches!(outcome, DialogOutcome::None) {
            self.open = false;
            self.advanced = None;
        }
        outcome
    }

    fn video_rows(&mut self, ui: &mut egui::Ui, encoders: &[EncoderInfo], source: Option<(u32, u32)>) {
        let d = &mut self.draft;
        row(ui, t!(FMT_ROW_SIZE), |ui| {
            let is_percent = matches!(d.size, SizeMode::Percent { .. });
            egui::ComboBox::from_id_salt("fmt-size").width(230.0).selected_text(d.size.label()).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.size, SizeMode::Full, t!(FMT_FULL_SIZE));
                ui.selectable_value(&mut d.size, SizeMode::Half, t!(FMT_HALF_SIZE));
                for (w, h) in SIZE_PRESETS {
                    ui.selectable_value(&mut d.size, SizeMode::Preset { width: w, height: h }, format!("{w}×{h}"));
                }
                if ui.selectable_label(is_percent, t!(FMT_CUSTOM_PERCENT)).clicked() && !is_percent {
                    d.size = SizeMode::Percent { x: 100, y: 100 };
                }
            });
            if let Some((w, h)) = source {
                let (ow, oh) = d.size.resolve(w, h);
                ui.label(RichText::new(format!("→ {ow}×{oh}")).color(TEXT_DIM).small());
            }
        });
        if let SizeMode::Percent { x, y } = &mut d.size {
            row(ui, "", |ui| {
                ui.add(egui::DragValue::new(x).range(10..=100).suffix(" %"));
                ui.label("X");
                ui.add(egui::DragValue::new(y).range(10..=100).suffix(" %"));
            });
        }

        row(ui, t!(FMT_ROW_FPS), |ui| {
            let label = if self.custom_fps { t!(FMT_CUSTOM_FPS, d.fps) } else { d.fps.to_string() };
            egui::ComboBox::from_id_salt("fmt-fps").width(230.0).selected_text(label).show_ui(ui, |ui| {
                for f in FPS_PRESETS {
                    if ui.selectable_label(!self.custom_fps && d.fps == f, f.to_string()).clicked() {
                        d.fps = f;
                        self.custom_fps = false;
                    }
                }
                if ui.selectable_label(self.custom_fps, t!(FMT_CUSTOM)).clicked() {
                    self.custom_fps = true;
                }
            });
            if self.custom_fps {
                ui.add(egui::DragValue::new(&mut d.fps).range(1..=240).suffix(" fps"));
            }
        });

        row(ui, t!(FMT_ROW_CODEC), |ui| {
            let current = d.video_codec.label(encoders);
            egui::ComboBox::from_id_salt("fmt-vcodec").width(230.0).selected_text(current).show_ui(ui, |ui| {
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
                        .color(TEXT_DIM)
                        .small(),
                    );
                }
            });
            if ui.button("...").on_hover_text(t!(FMT_ENCODER_DETAILS)).clicked() {
                self.advanced = Some(Advanced::Codec);
            }
        });

        row(ui, t!(FMT_ROW_QUALITY), |ui| {
            let label = match d.rate_control {
                RateControl::Quality(q) => q.to_string(),
                RateControl::ConstantBitrate { kbps } => t!(CBR_LABEL, kbps),
            };
            egui::ComboBox::from_id_salt("fmt-quality").width(230.0).selected_text(label).show_ui(ui, |ui| {
                for q in QUALITY_STEPS {
                    let text = match q {
                        100 => t!(FMT_QUALITY_BEST).to_string(),
                        10 => t!(FMT_QUALITY_SMALLEST).to_string(),
                        _ => q.to_string(),
                    };
                    ui.selectable_value(&mut d.rate_control, RateControl::Quality(q), text);
                }
            });
            if ui.button("...").on_hover_text(t!(FMT_BITRATE_TIP)).clicked() {
                self.advanced = Some(Advanced::Quality);
            }
        });

        row(ui, t!(FMT_ROW_PROFILE), |ui| {
            if d.video_codec.is_hevc() {
                egui::ComboBox::from_id_salt("fmt-profile-hevc")
                    .width(230.0)
                    .selected_text(d.profiles.hevc.label())
                    .show_ui(ui, |ui| {
                        for p in HevcProfile::ALL {
                            ui.selectable_value(&mut d.profiles.hevc, p, p.label());
                        }
                    });
            } else {
                egui::ComboBox::from_id_salt("fmt-profile-h264")
                    .width(230.0)
                    .selected_text(d.profiles.h264.label())
                    .show_ui(ui, |ui| {
                        for p in H264Profile::ALL {
                            ui.selectable_value(&mut d.profiles.h264, p, p.label());
                        }
                    });
            }
            ui.button("?").on_hover_text(t!(FMT_PROFILE_HELP));
        });
    }

    fn audio_rows(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        row(ui, t!(FMT_ROW_CODEC), |ui| {
            egui::ComboBox::from_id_salt("fmt-acodec").width(230.0).selected_text(d.audio_codec.label()).show_ui(ui, |ui| {
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
            });
        });
        row(ui, t!(FMT_ROW_BITRATE), |ui| {
            let allowed = d.audio_codec.allowed_bitrates();
            ui.add_enabled_ui(!allowed.is_empty(), |ui| {
                let label = if allowed.is_empty() { "—".to_string() } else { d.audio_bitrate_kbps.to_string() };
                egui::ComboBox::from_id_salt("fmt-abitrate").width(230.0).selected_text(label).show_ui(ui, |ui| {
                    for &b in allowed {
                        ui.selectable_value(&mut d.audio_bitrate_kbps, b, b.to_string());
                    }
                });
            });
            ui.label("kbps");
        });
        row(ui, t!(FMT_ROW_CHANNELS), |ui| {
            let label = if d.audio_channels == 1 { t!(MONO) } else { t!(STEREO) };
            egui::ComboBox::from_id_salt("fmt-channels").width(230.0).selected_text(label).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.audio_channels, 1, t!(MONO));
                ui.selectable_value(&mut d.audio_channels, 2, t!(STEREO));
            });
        });
        row(ui, t!(FMT_ROW_FREQUENCY), |ui| {
            egui::ComboBox::from_id_salt("fmt-rate").width(230.0).selected_text(d.audio_sample_rate.to_string()).show_ui(ui, |ui| {
                for r in SAMPLE_RATES {
                    ui.selectable_value(&mut d.audio_sample_rate, r, r.to_string());
                }
            });
            ui.label("Hz");
        });
    }

    fn show_advanced(&mut self, ctx: &egui::Context, which: Advanced, encoders: &[EncoderInfo], source: Option<(u32, u32)>) {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("format-settings-advanced")).show(ctx, |ui| {
            ui.set_width(420.0);
            match which {
                Advanced::Quality => {
                    ui.heading(t!(FMT_ROW_BITRATE));
                    ui.add_space(6.0);
                    let d = &mut self.draft;
                    let mut quality_mode = matches!(d.rate_control, RateControl::Quality(_));
                    if ui.radio_value(&mut quality_mode, true, t!(FMT_QUALITY_MODE)).clicked()
                        && !matches!(d.rate_control, RateControl::Quality(_))
                    {
                        d.rate_control = RateControl::Quality(80);
                    }
                    if ui.radio_value(&mut quality_mode, false, t!(FMT_CBR_MODE)).clicked()
                        && !matches!(d.rate_control, RateControl::ConstantBitrate { .. })
                    {
                        let kbps = source.map(|(w, h)| d.target_bitrate_kbps(w, h)).unwrap_or(6000);
                        d.rate_control = RateControl::ConstantBitrate { kbps };
                    }
                    ui.add_space(4.0);
                    match &mut d.rate_control {
                        RateControl::Quality(q) => {
                            row(ui, t!(FMT_ROW_QUALITY), |ui| {
                                ui.add(egui::Slider::new(q, 10..=100).step_by(10.0));
                            });
                        }
                        RateControl::ConstantBitrate { kbps } => {
                            row(ui, t!(FMT_ROW_BITRATE), |ui| {
                                ui.add(egui::DragValue::new(kbps).range(200..=100_000).suffix(" kbps").speed(50));
                            });
                        }
                    }
                    if let Some((w, h)) = source {
                        let (ow, oh) = d.size.resolve(w, h);
                        ui.label(
                            RichText::new(t!(FMT_BITRATE_ESTIMATE, d.target_bitrate_kbps(ow, oh), ow, oh, d.fps))
                                .color(TEXT_DIM)
                                .small(),
                        );
                    }
                    row(ui, t!(FMT_ROW_KEYFRAME), |ui| {
                        ui.add(egui::DragValue::new(&mut d.keyframe_interval_s).range(0.5..=10.0).suffix(" s").speed(0.1));
                    });
                    ui.label(RichText::new(t!(FMT_RATE_CONTROL_NOTE)).color(TEXT_DIM).small());
                }
                Advanced::Codec => {
                    ui.heading(t!(FMT_ENCODER_DETAILS));
                    ui.add_space(6.0);
                    let codec = self.draft.video_codec.clone();
                    match codec.info(encoders) {
                        Some(info) => {
                            row(ui, t!(FMT_ROW_ENCODER), |ui| {
                                ui.label(RichText::new(&info.label).color(TEXT_BRIGHT));
                            });
                            row(ui, t!(FMT_ROW_VENDOR), |ui| {
                                ui.label(info.vendor.label());
                            });
                            row(ui, t!(FMT_ROW_HARDWARE), |ui| {
                                ui.label(if info.hardware { t!(YES_GPU) } else { t!(NO_CPU) });
                            });
                            row(ui, t!(FMT_ROW_TRANSFORM), |ui| {
                                ui.label(RichText::new(&info.friendly_name).small());
                            });
                            row(ui, "CLSID", |ui| {
                                ui.label(RichText::new(&info.clsid).monospace().small());
                            });
                        }
                        None => {
                            row(ui, t!(FMT_ROW_ENCODER), |ui| {
                                ui.label(RichText::new(codec.generic_label()).color(TEXT_BRIGHT));
                            });
                            row(ui, t!(FMT_ROW_DETAILS), |ui| {
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
                        ui.label(RichText::new(t!(FMT_MF_COUNT, found)).color(TEXT_DIM).small());
                        if ui.button(t!(FMT_RESCAN)).clicked() {
                            self.rescan = true;
                        }
                    }
                }
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(t!(CLOSE)).min_size(Vec2::new(90.0, 26.0))).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.advanced = None;
        }
    }
}

/// Titled, bordered group like the boxes in the classic dialog.
fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(title).color(TEXT_NORMAL));
    egui::Frame::group(ui.style()).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
    ui.add_space(8.0);
}

/// Right-aligned label column followed by the controls.
fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(110.0, 24.0), Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(label).color(TEXT_NORMAL));
        });
        add(ui);
    });
    ui.add_space(2.0);
}
