//! Glyph rasterisation into a [`Sprite`].
//!
//! One rasteriser for both overlays: the watermark badge burned into recordings
//! and the FPS counter drawn inside a game. The hook DLL cannot reach egui, so
//! this is the only text path it has — and keeping it shared means the two
//! overlays cannot drift apart in metrics or appearance.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::sprite::Sprite;

/// Inter SemiBold, also registered with egui by openclip's `ui::theme`.
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");

/// A parsed font, ready to rasterise runs into a sprite.
pub struct TextRenderer {
    font: FontRef<'static>,
}

impl TextRenderer {
    /// `None` if the bundled font cannot be parsed — every caller carries on
    /// without an overlay rather than failing the recording.
    pub fn new() -> Option<Self> {
        Self::from_slice(INTER_SEMIBOLD)
    }

    pub fn from_slice(bytes: &'static [u8]) -> Option<Self> {
        FontRef::try_from_slice(bytes).ok().map(|font| Self { font })
    }

    /// Advance width of `text` at `px`, kerning included.
    pub fn width(&self, text: &str, px: f32) -> f32 {
        let sf = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev: Option<GlyphId> = None;
        for c in text.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Baseline that vertically centres `px`-tall text in a `box_h`-tall box.
    pub fn baseline(&self, px: f32, box_h: f32) -> f32 {
        let sf = self.font.as_scaled(PxScale::from(px));
        (box_h - (sf.ascent() - sf.descent())) / 2.0 + sf.ascent()
    }

    /// Draws `text` into `sprite` with its pen starting at `origin`, taking each
    /// character's colour from `color`. The per-character colour is what lets
    /// the watermark split "open" from "clip" and the FPS counter tint its
    /// number differently from its unit — in one kerned run either way.
    pub fn draw_run(
        &self,
        sprite: &mut Sprite,
        text: &str,
        px: f32,
        origin: (f32, f32),
        alpha: f32,
        color: impl Fn(usize) -> [u8; 3],
    ) {
        let scale = PxScale::from(px);
        let sf = self.font.as_scaled(scale);
        let (mut pen, baseline) = origin;
        let mut prev: Option<GlyphId> = None;
        for (i, c) in text.chars().enumerate() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                pen += sf.kern(p, id);
            }
            let rgb = color(i);
            if let Some(outline) = self.font.outline_glyph(id.with_scale_and_position(scale, point(pen, baseline))) {
                let bounds = outline.px_bounds();
                let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
                outline.draw(|gx, gy, cov| {
                    sprite.put(ox + gx as i32, oy + gy as i32, rgb, cov * alpha);
                });
            }
            pen += sf.h_advance(id);
            prev = Some(id);
        }
    }

    /// [`draw_run`](Self::draw_run) in a single colour.
    pub fn draw(&self, sprite: &mut Sprite, text: &str, px: f32, origin: (f32, f32), rgb: [u8; 3], alpha: f32) {
        self.draw_run(sprite, text, px, origin, alpha, |_| rgb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_parses() {
        assert!(TextRenderer::new().is_some());
    }

    #[test]
    fn width_grows_with_size_and_length() {
        let t = TextRenderer::new().unwrap();
        assert!(t.width("120", 32.0) > t.width("120", 16.0));
        assert!(t.width("1200", 16.0) > t.width("120", 16.0));
        assert_eq!(t.width("", 16.0), 0.0);
    }

    #[test]
    fn draw_marks_pixels_inside_the_sprite() {
        let t = TextRenderer::new().unwrap();
        let mut s = Sprite::new(64, 24);
        let baseline = t.baseline(16.0, 24.0);
        t.draw(&mut s, "60", 16.0, (2.0, baseline), [255, 255, 255], 1.0);
        assert!(s.rgba.iter().skip(3).step_by(4).any(|&a| a > 0), "no glyph coverage was written");
    }

    #[test]
    fn draw_run_colours_each_character() {
        let t = TextRenderer::new().unwrap();
        let mut s = Sprite::new(64, 24);
        let baseline = t.baseline(16.0, 24.0);
        t.draw_run(&mut s, "AB", 16.0, (2.0, baseline), 1.0, |i| if i == 0 { [255, 0, 0] } else { [0, 255, 0] });
        let reds = (0..s.height).flat_map(|y| (0..s.width).map(move |x| (x, y))).filter(|&(x, y)| {
            let (rgb, a) = s.get(x, y);
            a > 0.5 && rgb[0] > rgb[1]
        });
        assert!(reds.count() > 0, "the first character kept its own colour");
    }
}
