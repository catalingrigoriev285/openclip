//! Produces small RGBA images for the live preview in the GUI.

use super::{PixelFormat, RawFrame};

/// A downscaled RGBA8 image ready for upload as a GUI texture.
#[derive(Debug, Clone)]
pub struct PreviewImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Nearest-neighbour downscale of `frame` so its longer side is at most
/// `max_side` pixels, converted to RGBA.
pub fn make_preview(frame: &RawFrame, max_side: u32) -> PreviewImage {
    Previewer::new(frame.width, frame.height, max_side).make(frame)
}

/// Reusable downscaler with precomputed sample positions (no per-pixel math).
pub struct Previewer {
    src: (u32, u32),
    dims: (u32, u32),
    col_map: Vec<usize>,
    row_map: Vec<usize>,
}

impl Previewer {
    pub fn new(src_w: u32, src_h: u32, max_side: u32) -> Self {
        let scale = (src_w.max(src_h) as f32 / max_side.max(1) as f32).max(1.0);
        let w = ((src_w as f32 / scale) as u32).max(1);
        let h = ((src_h as f32 / scale) as u32).max(1);
        let col_map =
            (0..w).map(|x| (((x as f32 + 0.5) * scale) as usize).min(src_w.max(1) as usize - 1) * 4).collect();
        let row_map = (0..h).map(|y| (((y as f32 + 0.5) * scale) as usize).min(src_h.max(1) as usize - 1)).collect();
        Self { src: (src_w, src_h), dims: (w, h), col_map, row_map }
    }

    pub fn dims(&self) -> (u32, u32) {
        self.dims
    }

    pub fn make(&self, frame: &RawFrame) -> PreviewImage {
        if (frame.width, frame.height) != self.src {
            return Previewer::new(frame.width, frame.height, self.dims.0.max(self.dims.1)).make(frame);
        }
        let (w, h) = self.dims;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let stride = frame.stride as usize;
        let swap = frame.format == PixelFormat::Bgra;
        for &sy in &self.row_map {
            let row = &frame.data[sy * stride..];
            for &sx in &self.col_map {
                let px = &row[sx..sx + 4];
                if swap {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        PreviewImage { width: w, height: h, rgba }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn previewer_matches_reference() {
        let (w, h) = (97u32, 53u32);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            data.extend_from_slice(&[(i % 251) as u8, (i % 239) as u8, (i % 227) as u8, 7]);
        }
        let frame =
            RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None };
        let p = Previewer::new(w, h, 40).make(&frame);
        assert_eq!((p.width, p.height), (40, 21));
        assert_eq!(p.rgba.len(), 40 * 21 * 4);
        // Reference: straightforward per-pixel nearest neighbour.
        let scale = 97.0f32 / 40.0;
        for y in 0..21u32 {
            for x in 0..40u32 {
                let sx = (((x as f32 + 0.5) * scale) as usize).min(96);
                let sy = (((y as f32 + 0.5) * scale) as usize).min(52);
                let s = sy * (w as usize * 4) + sx * 4;
                let d = ((y * 40 + x) * 4) as usize;
                assert_eq!(&p.rgba[d..d + 4], &[frame.data[s + 2], frame.data[s + 1], frame.data[s], 255]);
            }
        }
        assert_eq!(make_preview(&frame, 40).rgba, p.rgba);
    }
}
