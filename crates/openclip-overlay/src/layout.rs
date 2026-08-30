//! Where a corner-anchored overlay sits, and how big it is.
//!
//! The watermark badge and the in-game FPS counter both anchor to a corner and
//! both scale with the frame so they look the same on a 720p region and a 4K
//! monitor. Only the constants differ, so the geometry lives here once and each
//! overlay supplies its own [`Layout`].

/// Which corner of the frame an overlay sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// Sizing and placement rules for one overlay.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Overlay height as a fraction of the frame height.
    pub height_ratio: f32,
    pub min_height: f32,
    pub max_height: f32,
    /// Distance from the frame edge, as a fraction of the overlay height.
    pub margin_ratio: f32,
    /// The overlay is skipped rather than swamping a very small frame.
    pub max_width_fraction: f32,
    pub max_height_fraction: f32,
}

impl Layout {
    /// Overlay height in pixels for a frame `frame_h` pixels tall, scaled by a
    /// user-chosen `size` percentage.
    pub fn height(&self, frame_h: u32, size: u32) -> u32 {
        let base = (frame_h as f32 * self.height_ratio).clamp(self.min_height, self.max_height);
        (base * size.clamp(10, 400) as f32 / 100.0).round().max(1.0) as u32
    }

    /// Top-left corner of a `sprite`-sized overlay in a `frame`-sized frame, or
    /// `None` when it would take too much of the frame to be tasteful.
    pub fn place(&self, corner: Corner, sprite: (u32, u32), frame: (u32, u32)) -> Option<(i32, i32)> {
        let ((sw, sh), (fw, fh)) = (sprite, frame);
        if sw == 0 || sh == 0 || fw == 0 || fh == 0 {
            return None;
        }
        if sw as f32 > fw as f32 * self.max_width_fraction || sh as f32 > fh as f32 * self.max_height_fraction {
            return None;
        }
        let m = (sh as f32 * self.margin_ratio).round() as i32;
        let (right, bottom) = (fw as i32 - sw as i32 - m, fh as i32 - sh as i32 - m);
        let (x, y) = match corner {
            Corner::TopLeft => (m, m),
            Corner::TopRight => (right, m),
            Corner::BottomLeft => (m, bottom),
            Corner::BottomRight => (right, bottom),
        };
        (x >= 0 && y >= 0).then_some((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Layout = Layout {
        height_ratio: 0.045,
        min_height: 22.0,
        max_height: 64.0,
        margin_ratio: 0.55,
        max_width_fraction: 0.45,
        max_height_fraction: 0.25,
    };

    #[test]
    fn height_clamps_and_scales() {
        assert_eq!(L.height(1080, 100), 49); // 1080 × 0.045 = 48.6
        // Tiny and huge frames stay inside the pixel bounds.
        assert_eq!(L.height(100, 100), 22);
        assert_eq!(L.height(4320, 100), 64);
        // The size percentage multiplies the clamped base.
        assert_eq!(L.height(1080, 50), 24);
        // Never zero, however small the frame or the percentage.
        assert!(L.height(1, 10) >= 1);
    }

    #[test]
    fn places_in_every_corner() {
        let (s, f) = ((100, 20), (1000, 500));
        let m = 11; // 20 * 0.55
        assert_eq!(L.place(Corner::TopLeft, s, f), Some((m, m)));
        assert_eq!(L.place(Corner::TopRight, s, f), Some((1000 - 100 - m, m)));
        assert_eq!(L.place(Corner::BottomLeft, s, f), Some((m, 500 - 20 - m)));
        assert_eq!(L.place(Corner::BottomRight, s, f), Some((1000 - 100 - m, 500 - 20 - m)));
    }

    #[test]
    fn skipped_when_it_would_swamp_the_frame() {
        assert_eq!(L.place(Corner::TopLeft, (100, 20), (150, 500)), None);
        assert_eq!(L.place(Corner::TopLeft, (100, 200), (1000, 500)), None);
        assert_eq!(L.place(Corner::TopLeft, (0, 0), (1000, 500)), None);
    }
}
