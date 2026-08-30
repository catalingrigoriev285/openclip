//! The openclip badge burned into recordings and snapshots.
//!
//! The badge is a translucent pill carrying the app icon and the "openclip"
//! wordmark, sized from the output height so it looks the same on a 720p region
//! and a 4K monitor. It is composed once into an RGBA sprite (straight alpha)
//! and then alpha-blended onto every frame with the same
//! [`save_patch`] / [`MouseFx::restore`](super::mouse_fx::MouseFx::restore)
//! mechanism the mouse effects use, so the reusable frame buffer stays clean.
//!
//! openclip is free and open source, so the badge is a default, not a lock:
//! [`Watermark::enabled`] turns it off.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use image::RgbaImage;
use serde::{Deserialize, Serialize};

use super::mouse_fx::{blend, save_patch, Patch};
use super::RawFrame;

/// App icon artwork, also used for the window icon and the About page.
pub const LOGO_PNG: &[u8] = include_bytes!("../../assets/android-chrome-192x192.png");
/// Inter SemiBold, also registered with egui by [`crate::ui`].
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// The wordmark, drawn as one run so kerning is applied across the split.
const WORDMARK: &str = "openclip";
/// Characters from here on take the accent colour ("open" | "clip").
const ACCENT_FROM: usize = 4;

/// systemOrange — the same accent as the icon's corner brackets and the region frame.
const ACCENT: [u8; 3] = [0xff, 0x9f, 0x0a];
const TEXT: [u8; 3] = [0xff, 0xff, 0xff];
const SCRIM: [u8; 3] = [0x00, 0x00, 0x00];
const SCRIM_ALPHA: f32 = 0.42;
/// Hairline just inside the pill edge, so the badge reads on dark content too.
const HAIRLINE_ALPHA: f32 = 0.10;
const TEXT_ALPHA: f32 = 0.96;

/// Badge height as a fraction of the output height, and its bounds in pixels.
const HEIGHT_RATIO: f32 = 0.045;
const MIN_HEIGHT: f32 = 22.0;
const MAX_HEIGHT: f32 = 64.0;
/// Distance from the frame edge, as a fraction of the badge height.
const MARGIN_RATIO: f32 = 0.55;
/// The badge is skipped rather than swamping a very small capture.
const MAX_WIDTH_FRACTION: f32 = 0.45;
const MAX_HEIGHT_FRACTION: f32 = 0.25;

/// Which corner of the frame the badge sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];
}

/// User-facing watermark settings. Percentages are integers so the struct can
/// derive `Eq` and the settings pages can diff it with `!=`, like [`super::mouse_fx::MouseFx`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    pub enabled: bool,
    pub position: Corner,
    /// Badge size in percent of its natural size.
    pub size: u32,
    /// Badge opacity in percent.
    pub opacity: u32,
}

impl Default for Watermark {
    fn default() -> Self {
        Self { enabled: true, position: Corner::default(), size: 100, opacity: 100 }
    }
}

impl Watermark {
    /// Whether anything would be painted at all.
    pub fn any_overlay(&self) -> bool {
        self.enabled && self.opacity > 0
    }

    /// Pill height in pixels for a frame `frame_h` pixels tall.
    pub fn badge_height(&self, frame_h: u32) -> u32 {
        let base = (frame_h as f32 * HEIGHT_RATIO).clamp(MIN_HEIGHT, MAX_HEIGHT);
        (base * self.size.clamp(10, 400) as f32 / 100.0).round().max(1.0) as u32
    }

    /// Top-left corner of a `sprite`-sized badge in a `frame`-sized frame, or
    /// `None` when the badge would take too much of the frame to be tasteful.
    pub fn place(&self, sprite: (u32, u32), frame: (u32, u32)) -> Option<(i32, i32)> {
        let ((sw, sh), (fw, fh)) = (sprite, frame);
        if sw == 0 || sh == 0 || fw == 0 || fh == 0 {
            return None;
        }
        if sw as f32 > fw as f32 * MAX_WIDTH_FRACTION || sh as f32 > fh as f32 * MAX_HEIGHT_FRACTION {
            return None;
        }
        let m = (sh as f32 * MARGIN_RATIO).round() as i32;
        let (right, bottom) = (fw as i32 - sw as i32 - m, fh as i32 - sh as i32 - m);
        let (x, y) = match self.position {
            Corner::TopLeft => (m, m),
            Corner::TopRight => (right, m),
            Corner::BottomLeft => (m, bottom),
            Corner::BottomRight => (right, bottom),
        };
        (x >= 0 && y >= 0).then_some((x, y))
    }
}

/// The composed badge: straight-alpha RGBA, row-major, 4 bytes per pixel.
#[derive(Debug, Clone)]
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Sprite {
    /// Source-over of `rgb` at coverage `a` onto the straight-alpha buffer.
    fn put(&mut self, x: i32, y: i32, rgb: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let sa = a.clamp(0.0, 1.0);
        if sa <= 0.0 {
            return;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        let da = self.rgba[i + 3] as f32 / 255.0;
        let out = sa + da * (1.0 - sa);
        if out <= 0.0 {
            return;
        }
        for (dst, &src) in self.rgba[i..i + 3].iter_mut().zip(rgb.iter()) {
            let over = src as f32 * sa + *dst as f32 * da * (1.0 - sa);
            *dst = (over / out).round().clamp(0.0, 255.0) as u8;
        }
        self.rgba[i + 3] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// Alpha-blends the badge onto `frame` with its top-left at (`x0`, `y0`).
    fn blit(&self, frame: &mut RawFrame, x0: i32, y0: i32, opacity: f32) {
        for y in 0..self.height {
            for x in 0..self.width {
                let i = (y as usize * self.width as usize + x as usize) * 4;
                let a = self.rgba[i + 3] as f32 / 255.0 * opacity;
                if a > 0.0 {
                    let rgb = [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]];
                    blend(frame, x0 + x as i32, y0 + y as i32, rgb, a);
                }
            }
        }
    }
}

/// Composes and caches the badge. Not part of [`Watermark`] so the settings stay
/// a plain value type; one renderer lives on the encode thread, one in the GUI.
pub struct WatermarkRenderer {
    font: FontRef<'static>,
    logo: RgbaImage,
    /// The badge composed for one pixel height; recomposed when it changes.
    cache: Option<(u32, Sprite)>,
}

impl WatermarkRenderer {
    /// `None` if the bundled font or icon cannot be parsed — the caller then
    /// carries on without a badge rather than failing the recording.
    pub fn new() -> Option<Self> {
        let font = FontRef::try_from_slice(INTER_SEMIBOLD)
            .map_err(|e| log::warn!("watermark: cannot parse the bundled font: {e}"))
            .ok()?;
        let logo = image::load_from_memory(LOGO_PNG)
            .map_err(|e| log::warn!("watermark: cannot decode the bundled icon: {e}"))
            .ok()?
            .to_rgba8();
        Some(Self { font, logo, cache: None })
    }

    /// The badge at `height` pixels tall, composing it if it is not cached.
    pub fn sprite(&mut self, height: u32) -> &Sprite {
        let height = height.max(1);
        if self.cache.as_ref().map(|(h, _)| *h) != Some(height) {
            let sprite = self.compose(height);
            self.cache = Some((height, sprite));
        }
        &self.cache.as_ref().expect("just composed").1
    }

    /// Paints the badge, recording the pixels under it so
    /// [`MouseFx::restore`](super::mouse_fx::MouseFx::restore) can undo it.
    pub fn paint(&mut self, wm: &Watermark, frame: &mut RawFrame, patches: &mut Vec<Patch>) {
        if !wm.any_overlay() {
            return;
        }
        let sprite = self.sprite(wm.badge_height(frame.height));
        let Some((x, y)) = wm.place((sprite.width, sprite.height), (frame.width, frame.height)) else {
            return;
        };
        let (x1, y1) = ((x + sprite.width as i32) as f32, (y + sprite.height as i32) as f32);
        save_patch(frame, x as f32, y as f32, x1, y1, patches);
        sprite.blit(frame, x, y, wm.opacity.min(100) as f32 / 100.0);
    }

    /// Like [`paint`](Self::paint) but without saving the pixels underneath,
    /// for frames that are thrown away after use (previews, snapshots).
    pub fn apply(&mut self, wm: &Watermark, frame: &mut RawFrame) {
        let mut patches = Vec::new();
        self.paint(wm, frame, &mut patches);
    }

    /// Advance width of the wordmark at `px`, kerning included.
    fn wordmark_width(&self, px: f32) -> f32 {
        let sf = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev: Option<GlyphId> = None;
        for c in WORDMARK.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w
    }

    fn compose(&self, height: u32) -> Sprite {
        let h = height as f32;
        let pad = h * 0.34;
        let gap = h * 0.30;
        let logo_px = (h * 0.62).round().clamp(1.0, h) as u32;
        let font_px = h * 0.44;
        let text_w = self.wordmark_width(font_px);
        let width = (pad + logo_px as f32 + gap + text_w + pad).ceil().max(1.0) as u32;
        let mut sprite = Sprite { width, height, rgba: vec![0; (width as usize * height as usize) * 4] };

        // The pill, plus a hairline just inside its edge.
        let radius = h / 2.0;
        for y in 0..height {
            for x in 0..width {
                let d = rounded_box_sdf(x as f32 + 0.5, y as f32 + 0.5, width as f32, h, radius);
                let fill = (0.5 - d).clamp(0.0, 1.0);
                if fill > 0.0 {
                    sprite.put(x as i32, y as i32, SCRIM, SCRIM_ALPHA * fill);
                }
                let edge = (1.0 - (d + 1.0).abs()).clamp(0.0, 1.0);
                if edge > 0.0 {
                    sprite.put(x as i32, y as i32, TEXT, HAIRLINE_ALPHA * edge);
                }
            }
        }

        // The icon.
        let logo = self.logo_at(logo_px);
        let lx = pad.round() as i32;
        let ly = ((h - logo_px as f32) / 2.0).round() as i32;
        for (px, py, p) in logo.enumerate_pixels() {
            let [r, g, b, a] = p.0;
            sprite.put(lx + px as i32, ly + py as i32, [r, g, b], a as f32 / 255.0);
        }

        // The wordmark, "open" in white and "clip" in the accent colour.
        let scale = PxScale::from(font_px);
        let sf = self.font.as_scaled(scale);
        let baseline = (h - (sf.ascent() - sf.descent())) / 2.0 + sf.ascent();
        let mut pen = pad + logo_px as f32 + gap;
        let mut prev: Option<GlyphId> = None;
        for (i, c) in WORDMARK.chars().enumerate() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                pen += sf.kern(p, id);
            }
            let color = if i < ACCENT_FROM { TEXT } else { ACCENT };
            if let Some(outline) = self.font.outline_glyph(id.with_scale_and_position(scale, point(pen, baseline))) {
                let bounds = outline.px_bounds();
                let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
                outline.draw(|gx, gy, cov| {
                    sprite.put(ox + gx as i32, oy + gy as i32, color, cov * TEXT_ALPHA);
                });
            }
            pen += sf.h_advance(id);
            prev = Some(id);
        }
        sprite
    }

    /// The icon resized to `px` square. Alpha is premultiplied around the
    /// resize, or the rounded corners fringe dark against the pill.
    fn logo_at(&self, px: u32) -> RgbaImage {
        let mut src = self.logo.clone();
        for p in src.pixels_mut() {
            let a = p.0[3] as u32;
            for c in p.0[..3].iter_mut() {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
        let mut out = image::imageops::resize(&src, px.max(1), px.max(1), image::imageops::FilterType::Lanczos3);
        for p in out.pixels_mut() {
            let a = p.0[3] as u32;
            for c in p.0[..3].iter_mut() {
                if let Some(v) = (*c as u32 * 255 + a / 2).checked_div(a) {
                    *c = v.min(255) as u8;
                }
            }
        }
        out
    }
}

/// Signed distance from (`px`, `py`) to a `w`×`h` box with corner radius `r`,
/// negative inside. Same coverage idiom as `fill_disc` in `mouse_fx`.
fn rounded_box_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (px - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (py - h / 2.0).abs() - (h / 2.0 - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::PixelFormat;
    use std::time::Duration;

    fn frame(w: u32, h: u32, format: PixelFormat) -> RawFrame {
        RawFrame {
            data: vec![0u8; (w * h * 4) as usize],
            width: w,
            height: h,
            stride: w * 4,
            format,
            pts: Duration::ZERO,
            mouse: None,
        }
    }

    fn renderer() -> WatermarkRenderer {
        WatermarkRenderer::new().expect("bundled font and icon must parse")
    }

    #[test]
    fn badge_scales_with_output_and_size() {
        let wm = Watermark::default();
        // Clamped at both ends, proportional in between.
        assert_eq!(wm.badge_height(240), MIN_HEIGHT as u32);
        assert_eq!(wm.badge_height(4320), MAX_HEIGHT as u32);
        assert_eq!(wm.badge_height(1080), 49);
        let big = Watermark { size: 200, ..wm };
        assert_eq!(big.badge_height(1080), 97);
    }

    #[test]
    fn sprite_is_cached_until_the_height_changes() {
        let mut r = renderer();
        let first = r.sprite(40).clone();
        let ptr = r.cache.as_ref().unwrap().1.rgba.as_ptr();
        assert_eq!(r.sprite(40).rgba.as_ptr(), ptr, "same height must reuse the composed badge");
        let taller = r.sprite(64).clone();
        assert_eq!(taller.height, 64);
        assert!(taller.width > first.width, "a taller badge is also wider");
        // The badge is a wide pill, never a square.
        assert!(first.width > first.height * 2);
    }

    #[test]
    fn badge_lands_in_the_requested_corner() {
        let mut r = renderer();
        let mut wm = Watermark::default();
        let (w, h) = (1280, 720);
        let sprite = r.sprite(wm.badge_height(h)).clone();
        for corner in Corner::ALL {
            wm.position = corner;
            let mut f = frame(w, h, PixelFormat::Bgra);
            r.apply(&wm, &mut f);
            let (x, y) = wm.place((sprite.width, sprite.height), (w, h)).expect("badge fits in 720p");
            let touched = |px: i32, py: i32| {
                let i = (py as usize * f.stride as usize) + px as usize * 4;
                f.data[i..i + 3] != [0, 0, 0]
            };
            let (cx, cy) = (x + sprite.width as i32 / 2, y + sprite.height as i32 / 2);
            assert!(touched(cx, cy), "{corner:?}: badge centre must be painted");
            assert!(!touched(w as i32 / 2, h as i32 / 2), "{corner:?}: frame centre must be untouched");
        }
    }

    #[test]
    fn paints_both_pixel_orders() {
        let mut r = renderer();
        let wm = Watermark { position: Corner::TopLeft, ..Default::default() };
        for format in [PixelFormat::Bgra, PixelFormat::Rgba] {
            let mut f = frame(640, 360, format);
            r.apply(&wm, &mut f);
            // The accent is orange, so on white-on-black the two orders differ
            // in which channel carries it; both must have painted something.
            assert!(f.data.iter().any(|&b| b != 0), "{format:?}: nothing was painted");
        }
    }

    #[test]
    fn paint_and_restore_round_trips() {
        let mut r = renderer();
        let mut f = frame(400, 240, PixelFormat::Bgra);
        for (i, b) in f.data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let clean = f.data.clone();
        let mut patches = Vec::new();
        for corner in Corner::ALL {
            let wm = Watermark { position: corner, ..Default::default() };
            r.paint(&wm, &mut f, &mut patches);
            assert!(!patches.is_empty(), "{corner:?}: nothing saved");
            assert_ne!(f.data, clean, "{corner:?}: nothing painted");
            crate::video::mouse_fx::MouseFx::restore(&mut f, &mut patches);
            assert_eq!(f.data, clean, "{corner:?}: restore must be byte-exact");
        }
    }

    #[test]
    fn skipped_when_disabled_transparent_or_too_small() {
        let mut r = renderer();
        let cases = [
            (Watermark { enabled: false, ..Default::default() }, 1280, 720),
            (Watermark { opacity: 0, ..Default::default() }, 1280, 720),
            // A tiny region: the badge would swamp it, so it is left out.
            (Watermark::default(), 120, 90),
        ];
        for (wm, w, h) in cases {
            let mut f = frame(w, h, PixelFormat::Bgra);
            let mut patches = Vec::new();
            r.paint(&wm, &mut f, &mut patches);
            assert!(patches.is_empty(), "{wm:?} at {w}x{h}: should not paint");
            assert!(f.data.iter().all(|&b| b == 0), "{wm:?} at {w}x{h}: frame must be untouched");
        }
    }

    #[test]
    fn settings_round_trip_json() {
        let wm = Watermark { enabled: false, position: Corner::TopLeft, size: 150, opacity: 60 };
        let text = serde_json::to_string(&wm).unwrap();
        assert_eq!(serde_json::from_str::<Watermark>(&text).unwrap(), wm);
        // Missing fields fall back to the defaults.
        let partial: Watermark = serde_json::from_str(r#"{"opacity":50}"#).unwrap();
        assert_eq!(partial, Watermark { opacity: 50, ..Default::default() });
    }
}
