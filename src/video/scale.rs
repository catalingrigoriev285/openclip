//! Arbitrary-ratio frame scaling (presets / percentages) with `fast_image_resize`.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use super::convert::RawFrame;

pub struct Scaler {
    resizer: Resizer,
    options: ResizeOptions,
    dst: (u32, u32),
    tight: Vec<u8>,
}

impl Scaler {
    /// Scales `src`-sized frames to `dst`. Bilinear for mild downscaling keeps
    /// screen text crisp; a box filter avoids aliasing beyond 2×.
    pub fn new(src: (u32, u32), dst: (u32, u32)) -> Self {
        let ratio = (src.0 as f32 / dst.0.max(1) as f32).max(src.1 as f32 / dst.1.max(1) as f32);
        let filter = if ratio > 2.0 { FilterType::Box } else { FilterType::Bilinear };
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)).use_alpha(false);
        Self { resizer: Resizer::new(), options, dst, tight: Vec::new() }
    }

    pub fn dst(&self) -> (u32, u32) {
        self.dst
    }

    pub fn scale(&mut self, frame: &RawFrame) -> RawFrame {
        let (w, h) = (frame.width, frame.height);
        if (w, h) == self.dst {
            return frame.clone();
        }
        let row = (w * 4) as usize;
        let src_bytes: &[u8] = if frame.stride as usize == row && frame.data.len() >= row * h as usize {
            &frame.data[..row * h as usize]
        } else {
            self.tight.clear();
            self.tight.reserve(row * h as usize);
            for y in 0..h as usize {
                let start = y * frame.stride as usize;
                self.tight.extend_from_slice(&frame.data[start..start + row]);
            }
            &self.tight
        };
        let mut out = RawFrame {
            data: Vec::new(),
            width: self.dst.0,
            height: self.dst.1,
            stride: self.dst.0 * 4,
            format: frame.format,
            pts: frame.pts,
            mouse: frame.mouse.clone(),
        };
        match ImageRef::new(w, h, src_bytes, PixelType::U8x4) {
            Ok(src) => {
                let mut dst = Image::new(self.dst.0, self.dst.1, PixelType::U8x4);
                if let Err(e) = self.resizer.resize(&src, &mut dst, &self.options) {
                    log::warn!("resize failed: {e}");
                    return frame.clone();
                }
                out.data = dst.into_vec();
                out
            }
            Err(e) => {
                log::warn!("resize: bad source buffer: {e}");
                frame.clone()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::PixelFormat;
    use std::time::Duration;

    #[test]
    fn scales_to_target_size() {
        let (w, h) = (64, 48);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.as_chunks_mut::<4>().0 {
            *px = [10, 20, 30, 255];
        }
        let frame = RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None };
        let mut s = Scaler::new((w, h), (32, 24));
        let out = s.scale(&frame);
        assert_eq!((out.width, out.height, out.stride), (32, 24, 128));
        assert_eq!(out.data.len(), 32 * 24 * 4);
        assert_eq!(&out.data[..4], &[10, 20, 30, 255]);
        // Padded stride path.
        let mut padded = frame.clone();
        padded.stride = w * 4 + 16;
        let mut d = Vec::new();
        for y in 0..h as usize {
            d.extend_from_slice(&frame.data[y * (w * 4) as usize..(y + 1) * (w * 4) as usize]);
            d.extend_from_slice(&[0; 16]);
        }
        padded.data = d;
        let out2 = Scaler::new((w, h), (20, 10)).scale(&padded);
        assert_eq!((out2.width, out2.height), (20, 10));
        assert_eq!(&out2.data[..4], &[10, 20, 30, 255]);
    }
}
