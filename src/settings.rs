//! User settings: the recording format (container, codecs, size, quality …)
//! plus the other preferences that survive a restart. Persisted as JSON in the
//! platform config directory (`%APPDATA%\openclip\settings.json` on Windows).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::i18n::Lang;
use crate::t;
use crate::video::encoder::EncoderInfo;
use crate::video::mouse_fx::MouseFx;
use crate::video::watermark::Watermark;

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Container {
    #[default]
    Mp4,
    Avi,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Avi => "avi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4",
            Container::Avi => "AVI",
        }
    }
}

/// The video encoder to use. Media Foundation encoders are identified by the
/// transform CLSID (32 hex digits) so a specific GPU's encoder can be chosen.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VideoCodec {
    /// The best available H.264 encoder: a hardware encoder when present,
    /// otherwise OpenH264.
    #[default]
    Auto,
    /// Bundled OpenH264 (CPU, every platform).
    OpenH264,
    /// A Windows Media Foundation transform (hardware NVENC / AMF / Quick Sync /
    /// DX12, or a software encoder).
    Mf { hevc: bool, clsid: String },
}

impl VideoCodec {
    pub fn is_hevc(&self) -> bool {
        matches!(self, VideoCodec::Mf { hevc: true, .. })
    }

    /// Whether this codec needs Windows Media Foundation.
    pub fn needs_mf(&self) -> bool {
        matches!(self, VideoCodec::Mf { .. })
    }

    pub fn family(&self) -> &'static str {
        if self.is_hevc() { "H265/HEVC" } else { "H264" }
    }

    /// What `Auto` stands for on this machine: the first hardware H.264
    /// encoder, else OpenH264.
    pub fn resolve_auto(available: &[EncoderInfo]) -> VideoCodec {
        available.iter().find(|e| e.hardware && !e.hevc).map(EncoderInfo::codec).unwrap_or(VideoCodec::OpenH264)
    }

    /// The enumerated encoder behind this choice, if present on this machine.
    pub fn info<'a>(&self, available: &'a [EncoderInfo]) -> Option<&'a EncoderInfo> {
        match self {
            VideoCodec::Auto => Self::resolve_auto(available).info(available),
            VideoCodec::OpenH264 => None,
            VideoCodec::Mf { clsid, .. } => available.iter().find(|e| &e.clsid == clsid),
        }
    }

    /// Label used when no enumerated encoder describes this codec better.
    pub fn generic_label(&self) -> String {
        match self {
            VideoCodec::Auto => t!(CODEC_AUTO_GENERIC).into(),
            VideoCodec::OpenH264 => t!(CODEC_OPENH264).into(),
            VideoCodec::Mf { .. } => t!(CODEC_MF_GENERIC, self.family()),
        }
    }

    /// Display label, preferring the enumerated encoder's vendor-specific name.
    pub fn label(&self, available: &[EncoderInfo]) -> String {
        match self {
            VideoCodec::Auto => t!(CODEC_AUTO_RESOLVED, Self::resolve_auto(available).label(available)),
            _ => self.info(available).map(|e| e.label.clone()).unwrap_or_else(|| self.generic_label()),
        }
    }
}

/// Resolves a command-line style encoder choice: `openh264`, `h264-hw`,
/// `h264-sw`, `hevc`, `hevc-sw`, a substring of an encoder label (`nvenc`,
/// `quick`, `dx12`, …) or a CLSID.
pub fn pick_encoder(query: &str, available: &[EncoderInfo]) -> Option<VideoCodec> {
    let q = query.trim().to_ascii_lowercase();
    let find = |f: &dyn Fn(&EncoderInfo) -> bool| available.iter().find(|e| f(e)).map(EncoderInfo::codec);
    match q.as_str() {
        "auto" => Some(VideoCodec::Auto),
        "openh264" | "cpu" => Some(VideoCodec::OpenH264),
        "h264-hw" | "h264" => find(&|e| !e.hevc && e.hardware),
        "h264-sw" => find(&|e| !e.hevc && !e.hardware),
        "hevc" | "hevc-hw" | "h265" => find(&|e| e.hevc && e.hardware),
        "hevc-sw" | "h265-sw" => find(&|e| e.hevc && !e.hardware),
        _ => find(&|e| e.clsid == q || e.label.to_ascii_lowercase().contains(&q)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum H264Profile {
    #[default]
    Auto,
    Baseline,
    Main,
    High,
}

impl H264Profile {
    pub const ALL: [H264Profile; 4] = [H264Profile::Auto, H264Profile::Baseline, H264Profile::Main, H264Profile::High];

    pub fn label(self) -> &'static str {
        match self {
            H264Profile::Auto => t!(AUTO),
            H264Profile::Baseline => "Baseline",
            H264Profile::Main => "Main",
            H264Profile::High => "High",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HevcProfile {
    #[default]
    Auto,
    Main,
}

impl HevcProfile {
    pub const ALL: [HevcProfile; 2] = [HevcProfile::Auto, HevcProfile::Main];

    pub fn label(self) -> &'static str {
        match self {
            HevcProfile::Auto => t!(AUTO),
            HevcProfile::Main => "Main",
        }
    }
}

/// Profile choices kept per codec family so switching codecs keeps each one's pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Profiles {
    pub h264: H264Profile,
    pub hevc: HevcProfile,
}

/// Output size relative to the captured source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SizeMode {
    #[default]
    Full,
    Half,
    /// Fit inside `width`×`height` keeping the aspect ratio (never upscales).
    Preset { width: u32, height: u32 },
    /// Scale each axis by a percentage (10–100).
    Percent { x: u32, y: u32 },
}

pub const SIZE_PRESETS: [(u32, u32); 4] = [(1920, 1080), (1280, 720), (854, 480), (640, 360)];

impl SizeMode {
    /// Output dimensions for a source of `src_w`×`src_h`: always even and at least 16 px.
    pub fn resolve(self, src_w: u32, src_h: u32) -> (u32, u32) {
        let (w, h) = match self {
            SizeMode::Full => (src_w, src_h),
            SizeMode::Half => (src_w / 2, src_h / 2),
            SizeMode::Preset { width, height } => {
                let s = (width as f64 / src_w.max(1) as f64).min(height as f64 / src_h.max(1) as f64).min(1.0);
                ((src_w as f64 * s).round() as u32, (src_h as f64 * s).round() as u32)
            }
            SizeMode::Percent { x, y } => {
                let (x, y) = (x.clamp(10, 100), y.clamp(10, 100));
                ((src_w as u64 * x as u64 / 100) as u32, (src_h as u64 * y as u64 / 100) as u32)
            }
        };
        (w.max(16) & !1, h.max(16) & !1)
    }

    pub fn label(self) -> String {
        match self {
            SizeMode::Full => t!(FMT_FULL_SIZE).into(),
            SizeMode::Half => t!(FMT_HALF_SIZE).into(),
            SizeMode::Preset { width, height } => format!("{width}×{height}"),
            SizeMode::Percent { x, y } => t!(FMT_CUSTOM_SIZE_LABEL, x, y),
        }
    }
}

/// How the video bitrate is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// 10–100 in steps of 10 (like classic recorders); the bitrate is derived from it.
    Quality(u8),
    ConstantBitrate { kbps: u32 },
}

impl Default for RateControl {
    fn default() -> Self {
        RateControl::Quality(80)
    }
}

pub const QUALITY_STEPS: [u8; 10] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioCodec {
    #[default]
    Mp3,
    Aac,
    /// Uncompressed 16-bit PCM (AVI only).
    Pcm,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 3] = [AudioCodec::Mp3, AudioCodec::Aac, AudioCodec::Pcm];

    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Mp3 => "MP3",
            AudioCodec::Aac => "AAC",
            AudioCodec::Pcm => "PCM",
        }
    }

    pub fn allowed_bitrates(self) -> &'static [u32] {
        match self {
            AudioCodec::Mp3 => &MP3_BITRATES,
            AudioCodec::Aac => &AAC_BITRATES,
            AudioCodec::Pcm => &[],
        }
    }
}

pub const FPS_PRESETS: [u32; 10] = [10, 15, 20, 24, 25, 30, 48, 50, 60, 120];
pub const MP3_BITRATES: [u32; 8] = [64, 96, 128, 160, 192, 224, 256, 320];
/// The only rates the Microsoft AAC encoder accepts (12/16/20/24 kB/s).
pub const AAC_BITRATES: [u32; 4] = [96, 128, 160, 192];
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

/// Everything in the "Format settings" dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatSettings {
    pub container: Container,
    pub size: SizeMode,
    pub fps: u32,
    pub video_codec: VideoCodec,
    pub rate_control: RateControl,
    /// Seconds between keyframes.
    pub keyframe_interval_s: f32,
    pub profiles: Profiles,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,
    pub audio_channels: u16,
    pub audio_sample_rate: u32,
}

impl Default for FormatSettings {
    fn default() -> Self {
        Self {
            container: Container::Mp4,
            size: SizeMode::Full,
            fps: 30,
            video_codec: VideoCodec::Auto,
            rate_control: RateControl::default(),
            keyframe_interval_s: 2.0,
            profiles: Profiles::default(),
            audio_codec: AudioCodec::Mp3,
            audio_bitrate_kbps: 160,
            audio_channels: 2,
            audio_sample_rate: 48_000,
        }
    }
}

impl FormatSettings {
    /// Whether Media Foundation codecs (AAC, hardware H.264/HEVC) can exist on this platform.
    pub const fn platform_has_mf() -> bool {
        cfg!(windows)
    }

    /// Applies the compatibility rules and returns a note for every change made.
    pub fn normalize(&mut self, available: &[EncoderInfo]) -> Vec<String> {
        let mut notes = Vec::new();

        if self.audio_codec == AudioCodec::Pcm && self.container == Container::Mp4 {
            self.audio_codec = if Self::platform_has_mf() { AudioCodec::Aac } else { AudioCodec::Mp3 };
            notes.push(t!(NOTE_PCM_AVI_ONLY, self.audio_codec.label()));
        }
        if self.audio_codec == AudioCodec::Aac && !Self::platform_has_mf() {
            self.audio_codec = AudioCodec::Mp3;
            notes.push(t!(NOTE_AAC_NEEDS_MF).into());
        }
        if self.video_codec.is_hevc() && self.container == Container::Avi {
            // Prefer a hardware H.264 encoder, then any H.264 encoder, then OpenH264.
            let h264 = available
                .iter()
                .find(|e| !e.hevc && e.hardware)
                .or_else(|| available.iter().find(|e| !e.hevc))
                .map(EncoderInfo::codec)
                .unwrap_or(VideoCodec::OpenH264);
            self.video_codec = h264;
            notes.push(t!(NOTE_HEVC_MP4_ONLY, self.video_codec.label(available)));
        }
        if self.video_codec.needs_mf() && self.video_codec.info(available).is_none() {
            let wanted = self.video_codec.family();
            self.video_codec = VideoCodec::OpenH264;
            notes.push(t!(NOTE_ENCODER_MISSING, wanted));
        }

        let allowed = self.audio_codec.allowed_bitrates();
        if !allowed.is_empty() && !allowed.contains(&self.audio_bitrate_kbps) {
            let want = self.audio_bitrate_kbps;
            self.audio_bitrate_kbps = *allowed.iter().min_by_key(|b| b.abs_diff(want)).unwrap();
        }
        if !matches!(self.audio_channels, 1 | 2) {
            self.audio_channels = 2;
        }
        if !SAMPLE_RATES.contains(&self.audio_sample_rate) {
            self.audio_sample_rate = 48_000;
        }
        self.fps = self.fps.clamp(1, 240);
        if let SizeMode::Percent { x, y } = &mut self.size {
            *x = (*x).clamp(10, 100);
            *y = (*y).clamp(10, 100);
        }
        match &mut self.rate_control {
            RateControl::Quality(q) => *q = ((*q as u32).clamp(10, 100) as f32 / 10.0).round() as u8 * 10,
            RateControl::ConstantBitrate { kbps } => *kbps = (*kbps).clamp(200, 100_000),
        }
        if !self.keyframe_interval_s.is_finite() {
            self.keyframe_interval_s = 2.0;
        }
        self.keyframe_interval_s = self.keyframe_interval_s.clamp(0.5, 10.0);
        notes
    }

    /// Bitrate for an output of `w`×`h` at the configured frame rate.
    pub fn target_bitrate_kbps(&self, w: u32, h: u32) -> u32 {
        match self.rate_control {
            RateControl::ConstantBitrate { kbps } => kbps,
            RateControl::Quality(q) => {
                let q = q.clamp(10, 100) as f64;
                // Bits per pixel per frame: 0.02 at q=10 … 0.20 at q=100.
                let bpp = 0.02 + (q - 10.0) / 90.0 * 0.18;
                let mut bps = w as f64 * h as f64 * self.fps.max(1) as f64 * bpp;
                if self.video_codec.is_hevc() {
                    bps *= 0.65;
                }
                (bps / 1000.0).round().clamp(500.0, 100_000.0) as u32
            }
        }
    }

    pub fn keyframe_interval_frames(&self) -> u32 {
        (self.keyframe_interval_s.max(0.1) * self.fps.max(1) as f32).round().max(1.0) as u32
    }

    pub fn quality_label(&self) -> String {
        match self.rate_control {
            RateControl::Quality(q) => t!(QUALITY_LABEL, q),
            RateControl::ConstantBitrate { kbps } => t!(CBR_LABEL, kbps),
        }
    }

    pub fn profile_label(&self) -> &'static str {
        if self.video_codec.is_hevc() { self.profiles.hevc.label() } else { self.profiles.h264.label() }
    }

    /// (title, detail) for the video summary card.
    pub fn video_summary(&self, available: &[EncoderInfo], source: Option<(u32, u32)>) -> (String, String) {
        let size = match source {
            Some((w, h)) if w > 0 => {
                let (ow, oh) = self.size.resolve(w, h);
                format!("{ow}×{oh}")
            }
            _ => self.size.label(),
        };
        let bitrate = match (self.rate_control, source) {
            (RateControl::Quality(_), Some((w, h))) if w > 0 => {
                let (ow, oh) = self.size.resolve(w, h);
                t!(VIDEO_SUMMARY_BITRATE, self.target_bitrate_kbps(ow, oh))
            }
            _ => String::new(),
        };
        (
            self.video_codec.label(available),
            t!(VIDEO_SUMMARY, size, self.fps, self.quality_label(), bitrate, self.profile_label()),
        )
    }

    /// (title, detail) for the audio summary card; `sources` describes what is recorded.
    pub fn audio_summary(&self, sources: &str) -> (String, String) {
        let ch = if self.audio_channels == 1 { t!(MONO_LOWER) } else { t!(STEREO_LOWER) };
        let rate = format!("{:.1}KHz", self.audio_sample_rate as f64 / 1000.0);
        let detail = match self.audio_codec {
            AudioCodec::Pcm => t!(AUDIO_SUMMARY_PCM, rate, ch, sources),
            _ => t!(AUDIO_SUMMARY, rate, ch, self.audio_bitrate_kbps, sources),
        };
        (self.audio_codec.label().to_string(), detail)
    }
}

/// Root of `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub format: FormatSettings,
    /// `None` → the default output folder.
    pub output_dir: Option<PathBuf>,
    pub file_prefix: String,
    pub system_audio: bool,
    pub mic_enabled: bool,
    /// Microphone by name (re-resolved to a device index at load).
    pub mic_name: Option<String>,
    pub mouse_fx: MouseFx,
    /// The openclip badge burned into recordings and snapshots.
    pub watermark: Watermark,
    /// Count down before recording starts.
    pub countdown_enabled: bool,
    pub countdown_secs: u32,
    /// Interface language; applied to [`crate::i18n`] by [`Settings::load`].
    pub language: Lang,
    /// Look for a newer release on GitHub when the app starts (see [`crate::update`]).
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: FormatSettings::default(),
            output_dir: None,
            file_prefix: "openclip".into(),
            system_audio: true,
            mic_enabled: false,
            mic_name: None,
            mouse_fx: MouseFx::default(),
            watermark: Watermark::default(),
            countdown_enabled: true,
            countdown_secs: 3,
            language: Lang::default(),
            check_updates: true,
        }
    }
}

pub const SETTINGS_FILE: &str = "settings.json";

/// Picks where settings live: next to the executable (portable) when that
/// folder is writable or already holds a settings file, else the per-user
/// config directory.
pub fn choose_path(exe_dir: Option<&Path>, portable_exists: bool, writable: bool, roaming: Option<PathBuf>) -> Option<PathBuf> {
    match exe_dir {
        Some(dir) if portable_exists || writable => Some(dir.join(SETTINGS_FILE)),
        _ => roaming,
    }
}

pub(crate) fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".openclip-write-test-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl Settings {
    /// The per-user location used before settings became portable.
    pub fn legacy_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("openclip").join(SETTINGS_FILE))
    }

    /// `settings.json` next to the executable when possible (portable install),
    /// otherwise the per-user config directory.
    pub fn path() -> Option<PathBuf> {
        static CHOSEN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        CHOSEN
            .get_or_init(|| {
                let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
                let portable_exists = exe_dir.as_ref().map(|d| d.join(SETTINGS_FILE).is_file()).unwrap_or(false);
                let writable = exe_dir.as_ref().map(|d| dir_is_writable(d)).unwrap_or(false);
                choose_path(exe_dir.as_deref(), portable_exists, writable, Self::legacy_path())
            })
            .clone()
    }

    /// Loads the settings and activates the saved interface language; missing
    /// or corrupt files yield the defaults. A settings file from the old
    /// per-user location is picked up when the portable one does not exist yet
    /// (it moves on the next save).
    pub fn load() -> Settings {
        let settings = Self::load_raw();
        crate::i18n::set_lang(settings.language);
        settings
    }

    fn load_raw() -> Settings {
        let Some(path) = Self::path() else { return Settings::default() };
        let mut candidates = vec![path];
        if let Some(legacy) = Self::legacy_path()
            && !candidates.contains(&legacy)
        {
            candidates.push(legacy);
        }
        for path in candidates {
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                    // Not normalized here: the codec list is only known once the
                    // encoders have been enumerated; the app normalizes then.
                    Ok(s) => {
                        log::info!("settings loaded from {}", path.display());
                        return s;
                    }
                    Err(e) => {
                        log::warn!("settings: {} is not valid, using defaults: {e}", path.display());
                        return Settings::default();
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!("settings: cannot read {}: {e}", path.display());
                    return Settings::default();
                }
            }
        }
        Settings::default()
    }

    /// Writes the settings atomically (temp file + rename).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config directory on this system"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        log::info!("settings saved to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::encoder::Vendor;

    fn nvenc() -> EncoderInfo {
        EncoderInfo {
            hevc: false,
            label: "H264 (NVIDIA® NVENC)".into(),
            friendly_name: "NVIDIA H.264 Encoder MFT".into(),
            vendor: Vendor::Nvidia,
            hardware: true,
            clsid: "60f445605a204857bfefd29773cb8040".into(),
        }
    }

    #[test]
    fn size_modes_resolve_even_dims() {
        assert_eq!(SizeMode::Full.resolve(1921, 1081), (1920, 1080));
        assert_eq!(SizeMode::Half.resolve(1920, 1080), (960, 540));
        assert_eq!(SizeMode::Preset { width: 1280, height: 720 }.resolve(2560, 1440), (1280, 720));
        // Never upscales.
        assert_eq!(SizeMode::Preset { width: 1920, height: 1080 }.resolve(800, 600), (800, 600));
        assert_eq!(SizeMode::Percent { x: 50, y: 75 }.resolve(1000, 1000), (500, 750));
        assert_eq!(SizeMode::Percent { x: 10, y: 10 }.resolve(20, 20), (16, 16));
    }

    #[test]
    fn normalize_enforces_container_rules() {
        let mut f = FormatSettings { container: Container::Mp4, audio_codec: AudioCodec::Pcm, ..Default::default() };
        let notes = f.normalize(&[]);
        assert_ne!(f.audio_codec, AudioCodec::Pcm);
        assert_eq!(notes.len(), 1);

        let hevc = VideoCodec::Mf { hevc: true, clsid: "abc".into() };
        let mut f = FormatSettings { video_codec: hevc.clone(), ..Default::default() };
        f.normalize(&[]);
        assert_eq!(f.video_codec, VideoCodec::OpenH264);

        // HEVC in AVI falls back to an available hardware H.264 encoder.
        let mut f = FormatSettings { video_codec: hevc, container: Container::Avi, ..Default::default() };
        f.normalize(&[nvenc()]);
        assert_eq!(f.video_codec, nvenc().codec());

        let mut f = FormatSettings { audio_bitrate_kbps: 150, audio_codec: AudioCodec::Aac, ..Default::default() };
        f.normalize(&[]);
        if FormatSettings::platform_has_mf() {
            assert_eq!(f.audio_bitrate_kbps, 160);
        } else {
            assert_eq!(f.audio_codec, AudioCodec::Mp3);
        }
    }

    #[test]
    fn auto_codec_resolves_and_parses_legacy() {
        assert_eq!(FormatSettings::default().video_codec, VideoCodec::Auto);
        assert_eq!(VideoCodec::resolve_auto(&[]), VideoCodec::OpenH264);
        assert_eq!(VideoCodec::resolve_auto(&[nvenc()]), nvenc().codec());
        assert!(VideoCodec::Auto.label(&[nvenc()]).contains("NVENC"));
        assert!(!VideoCodec::Auto.needs_mf());
        let mut f = FormatSettings::default();
        assert!(f.normalize(&[]).is_empty(), "Auto never needs coercion");
        let legacy: FormatSettings = serde_json::from_str(r#"{"video_codec":"OpenH264"}"#).unwrap();
        assert_eq!(legacy.video_codec, VideoCodec::OpenH264);
        assert_eq!(pick_encoder("auto", &[]), Some(VideoCodec::Auto));
    }

    #[test]
    fn settings_path_prefers_portable() {
        let exe = Path::new("C:/apps/openclip");
        let roaming = Some(PathBuf::from("C:/Users/x/AppData/Roaming/openclip/settings.json"));
        assert_eq!(choose_path(Some(exe), false, true, roaming.clone()), Some(exe.join("settings.json")));
        assert_eq!(choose_path(Some(exe), true, false, roaming.clone()), Some(exe.join("settings.json")));
        assert_eq!(choose_path(Some(exe), false, false, roaming.clone()), roaming.clone());
        assert_eq!(choose_path(None, false, true, roaming.clone()), roaming);
        assert!(Settings::default().countdown_enabled);
        assert_eq!(Settings::default().countdown_secs, 3);
    }

    #[test]
    fn picks_encoders_by_name() {
        let list = [nvenc()];
        assert_eq!(pick_encoder("openh264", &list), Some(VideoCodec::OpenH264));
        assert_eq!(pick_encoder("h264-hw", &list), Some(nvenc().codec()));
        assert_eq!(pick_encoder("nvenc", &list), Some(nvenc().codec()));
        assert_eq!(pick_encoder("hevc", &list), None);
        assert_eq!(VideoCodec::OpenH264.label(&list), "H264 (OpenH264, CPU)");
        assert_eq!(nvenc().codec().label(&list), "H264 (NVIDIA® NVENC)");
    }

    #[test]
    fn quality_maps_to_sane_bitrates() {
        let f = FormatSettings { rate_control: RateControl::Quality(80), ..Default::default() };
        let kbps = f.target_bitrate_kbps(1920, 1080);
        assert!((8_000..=12_000).contains(&kbps), "{kbps}");
        let low = FormatSettings { rate_control: RateControl::Quality(10), ..Default::default() };
        assert!(low.target_bitrate_kbps(1920, 1080) < kbps);
        let cbr = FormatSettings { rate_control: RateControl::ConstantBitrate { kbps: 1234 }, ..Default::default() };
        assert_eq!(cbr.target_bitrate_kbps(1920, 1080), 1234);
    }

    #[test]
    fn settings_roundtrip_json() {
        let s = Settings {
            file_prefix: "x".into(),
            format: FormatSettings { video_codec: nvenc().codec(), ..Default::default() },
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_prefix, "x");
        assert_eq!(back.format, s.format);
        // Unknown / missing fields are tolerated.
        let partial: Settings = serde_json::from_str(r#"{"format":{"fps":60}}"#).unwrap();
        assert_eq!(partial.format.fps, 60);
        assert_eq!(partial.format.container, Container::Mp4);
    }
}
