//! A straight-alpha RGBA buffer that overlays are composed into.
//!
//! Both sides of the overlay story build one of these: openclip composes the
//! watermark badge and blends it into a captured frame, and the injected hook
//! composes the FPS counter and uploads it to the game's GPU. Neither knows
//! about the other's pixel format, so this stays a plain buffer with one
//! source-over primitive.

/// Straight-alpha RGBA, row-major, 4 bytes per pixel.
#[derive(Debug, Clone, Default)]
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Sprite {
    /// A fully transparent sprite.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, rgba: vec![0; width as usize * height as usize * 4] }
    }

    /// Clears to fully transparent, keeping the allocation and resizing if the
    /// dimensions changed. Composing into a reused sprite avoids a per-frame
    /// allocation inside a game's present call.
    pub fn reset(&mut self, width: u32, height: u32) {
        let needed = width as usize * height as usize * 4;
        self.width = width;
        self.height = height;
        self.rgba.clear();
        self.rgba.resize(needed, 0);
    }

    /// Source-over of `rgb` at coverage `a` onto the straight-alpha buffer.
    pub fn put(&mut self, x: i32, y: i32, rgb: [u8; 3], a: f32) {
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

    /// The straight-alpha pixel at (`x`, `y`) as `(rgb, alpha)`.
    pub fn get(&self, x: u32, y: u32) -> ([u8; 3], f32) {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        ([self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]], self.rgba[i + 3] as f32 / 255.0)
    }

    /// Fills a rounded rectangle covering the whole sprite, plus an optional
    /// hairline just inside its edge — the pill both overlays sit on.
    pub fn fill_pill(&mut self, radius: f32, fill: ([u8; 3], f32), hairline: ([u8; 3], f32)) {
        let (w, h) = (self.width as f32, self.height as f32);
        for y in 0..self.height {
            for x in 0..self.width {
                let d = rounded_box_sdf(x as f32 + 0.5, y as f32 + 0.5, w, h, radius);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.put(x as i32, y as i32, fill.0, fill.1 * cov);
                }
                let edge = (1.0 - (d + 1.0).abs()).clamp(0.0, 1.0);
                if edge > 0.0 {
                    self.put(x as i32, y as i32, hairline.0, hairline.1 * edge);
                }
            }
        }
    }
}

/// Signed distance from (`px`, `py`) to a `w`×`h` box with corner radius `r`,
/// negative inside. Same coverage idiom as `fill_disc` in openclip's `mouse_fx`.
pub fn rounded_box_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (px - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (py - h / 2.0).abs() - (h / 2.0 - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_composes_source_over() {
        let mut s = Sprite::new(1, 1);
        s.put(0, 0, [255, 0, 0], 0.5);
        let (rgb, a) = s.get(0, 0);
        assert_eq!(rgb, [255, 0, 0]);
        assert!((a - 0.5).abs() < 0.01);
        // Opaque white over it wins outright.
        s.put(0, 0, [255, 255, 255], 1.0);
        let (rgb, a) = s.get(0, 0);
        assert_eq!((rgb, a), ([255, 255, 255], 1.0));
    }

    #[test]
    fn put_clips_and_ignores_nothing_coverage() {
        let mut s = Sprite::new(2, 2);
        s.put(-1, 0, [255, 255, 255], 1.0);
        s.put(0, 5, [255, 255, 255], 1.0);
        s.put(0, 0, [255, 255, 255], 0.0);
        assert!(s.rgba.iter().all(|&b| b == 0));
    }

    #[test]
    fn reset_resizes_and_clears() {
        let mut s = Sprite::new(2, 2);
        s.put(0, 0, [1, 2, 3], 1.0);
        s.reset(4, 3);
        assert_eq!((s.width, s.height), (4, 3));
        assert_eq!(s.rgba.len(), 4 * 3 * 4);
        assert!(s.rgba.iter().all(|&b| b == 0));
    }

    #[test]
    fn pill_is_solid_in_the_middle_and_clear_at_the_corners() {
        let mut s = Sprite::new(40, 20);
        s.fill_pill(10.0, ([0, 0, 0], 1.0), ([255, 255, 255], 0.0));
        assert_eq!(s.get(20, 10).1, 1.0);
        assert_eq!(s.get(0, 0).1, 0.0);
    }
}
