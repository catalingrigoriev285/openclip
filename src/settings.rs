//! User settings: the recording format (container, codecs, size, quality …)
//! plus the other preferences that survive a restart. Persisted as JSON in the
//! platform config directory (`%APPDATA%\openclip\settings.json` on Windows).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::video::encoder::EncoderInfo;
use crate::video::mouse_fx::MouseFx;

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

/// What the user picked; the concrete encoder is resolved when recording starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VideoCodec {
    /// Bundled OpenH264 (CPU, every platform).
    #[default]
    OpenH264,
    /// H.264 through a hardware Media Foundation encoder (NVENC / AMF / Quick Sync).
    MfH264Hardware,
    /// Microsoft's software H.264 Media Foundation encoder.
    MfH264Software,
    /// H.265 / HEVC through a hardware Media Foundation encoder.
    MfHevcHardware,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 4] =
        [VideoCodec::OpenH264, VideoCodec::MfH264Hardware, VideoCodec::MfH264Software, VideoCodec::MfHevcHardware];

    pub fn is_hevc(self) -> bool {
        matches!(self, VideoCodec::MfHevcHardware)
    }

    /// Whether this codec needs Windows Media Foundation.
    pub fn needs_mf(self) -> bool {
        !matches!(self, VideoCodec::OpenH264)
    }

    /// Label used when no enumerated encoder describes this codec better.
    pub fn generic_label(self) -> &'static str {
        match self {
            VideoCodec::OpenH264 => "H264 (OpenH264, CPU)",
            VideoCodec::MfH264Hardware => "H264 (GPU hardware encoder)",
            VideoCodec::MfH264Software => "H264 (Microsoft software)",
            VideoCodec::MfHevcHardware => "H265/HEVC (GPU hardware encoder)",
        }
    }

    /// Display label, preferring the enumerated encoder's vendor-specific name.
    pub fn label(self, available: &[EncoderInfo]) -> String {
        available
            .iter()
            .find(|e| e.codec == self)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| self.generic_label().to_string())
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
            H264Profile::Auto => "Auto",
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
            HevcProfile::Auto => "Auto",
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
            SizeMode::Full => "Full Size".into(),
            SizeMode::Half => "Half Size".into(),
            SizeMode::Preset { width, height } => format!("{width}×{height}"),
            SizeMode::Percent { x, y } => format!("Custom {x}% × {y}%"),
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
            video_codec: VideoCodec::OpenH264,
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
        let has = |c: VideoCodec| available.iter().any(|e| e.codec == c);

        if self.audio_codec == AudioCodec::Pcm && self.container == Container::Mp4 {
            self.audio_codec = if Self::platform_has_mf() { AudioCodec::Aac } else { AudioCodec::Mp3 };
            notes.push(format!("PCM audio is only available in AVI; using {}.", self.audio_codec.label()));
        }
        if self.audio_codec == AudioCodec::Aac && !Self::platform_has_mf() {
            self.audio_codec = AudioCodec::Mp3;
            notes.push("AAC needs Windows Media Foundation; using MP3.".into());
        }
        if self.video_codec.is_hevc() && self.container == Container::Avi {
            self.video_codec = if has(VideoCodec::MfH264Hardware) { VideoCodec::MfH264Hardware } else { VideoCodec::OpenH264 };
            notes.push(format!("HEVC is only written to MP4; using {}.", self.video_codec.label(available)));
        }
        if self.video_codec.needs_mf() && !has(self.video_codec) {
            let wanted = self.video_codec.generic_label();
            self.video_codec = VideoCodec::OpenH264;
            notes.push(format!("{wanted} is not available on this system; using OpenH264."));
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
            RateControl::Quality(q) => format!("quality {q}"),
            RateControl::ConstantBitrate { kbps } => format!("{kbps} kbps CBR"),
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
                format!(" (≈ {} kbps)", self.target_bitrate_kbps(ow, oh))
            }
            _ => String::new(),
        };
        (
            self.video_codec.label(available),
            format!("{size}, {} fps, {}{bitrate}, {} profile", self.fps, self.quality_label(), self.profile_label()),
        )
    }

    /// (title, detail) for the audio summary card; `sources` describes what is recorded.
    pub fn audio_summary(&self, sources: &str) -> (String, String) {
        let ch = if self.audio_channels == 1 { "mono" } else { "stereo" };
        let rate = format!("{:.1}KHz", self.audio_sample_rate as f64 / 1000.0);
        let detail = match self.audio_codec {
            AudioCodec::Pcm => format!("{rate}, {ch}, 16-bit – {sources}"),
            _ => format!("{rate}, {ch}, {}kbps – {sources}", self.audio_bitrate_kbps),
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
        }
    }
}

impl Settings {
    /// `<config dir>/openclip/settings.json`, if a config directory exists.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("openclip").join("settings.json"))
    }

    /// Loads the settings; missing or corrupt files yield the defaults.
    pub fn load() -> Settings {
        let Some(path) = Self::path() else { return Settings::default() };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                // Not normalized here: the codec list is only known once the
                // encoders have been enumerated; the app normalizes then.
                Ok(s) => s,
                Err(e) => {
                    log::warn!("settings: {} is not valid, using defaults: {e}", path.display());
                    Settings::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => {
                log::warn!("settings: cannot read {}: {e}", path.display());
                Settings::default()
            }
        }
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

        let mut f = FormatSettings { video_codec: VideoCodec::MfHevcHardware, ..Default::default() };
        f.normalize(&[]);
        assert_eq!(f.video_codec, VideoCodec::OpenH264);

        let mut f = FormatSettings { audio_bitrate_kbps: 150, audio_codec: AudioCodec::Aac, ..Default::default() };
        f.normalize(&[]);
        if FormatSettings::platform_has_mf() {
            assert_eq!(f.audio_bitrate_kbps, 160);
        } else {
            assert_eq!(f.audio_codec, AudioCodec::Mp3);
        }
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
        let s = Settings { file_prefix: "x".into(), ..Default::default() };
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
