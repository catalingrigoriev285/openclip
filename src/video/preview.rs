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
    let scale = (frame.width.max(frame.height) as f32 / max_side as f32).max(1.0);
    let w = ((frame.width as f32 / scale) as u32).max(1);
    let h = ((frame.height as f32 / scale) as u32).max(1);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let stride = frame.stride as usize;
    for y in 0..h {
        let sy = ((y as f32 + 0.5) * scale) as usize;
        let sy = sy.min(frame.height as usize - 1);
        for x in 0..w {
            let sx = ((x as f32 + 0.5) * scale) as usize;
            let sx = sx.min(frame.width as usize - 1);
            let s = sy * stride + sx * 4;
            let d = ((y * w + x) * 4) as usize;
            let px = &frame.data[s..s + 4];
            match frame.format {
                PixelFormat::Bgra => {
                    rgba[d] = px[2];
                    rgba[d + 1] = px[1];
                    rgba[d + 2] = px[0];
                }
                PixelFormat::Rgba => {
                    rgba[d] = px[0];
                    rgba[d + 1] = px[1];
                    rgba[d + 2] = px[2];
                }
            }
            rgba[d + 3] = 255;
        }
    }
    PreviewImage { width: w, height: h, rgba }
}
