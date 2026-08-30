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

/// [`make_preview`] reusing `buf` for the result, so a player converting thirty
/// frames a second does not allocate several megabytes each time.
pub fn make_preview_into(frame: &RawFrame, max_side: u32, buf: Vec<u8>) -> PreviewImage {
    Previewer::new(frame.width, frame.height, max_side).make_into(frame, buf)
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

    /// Full-resolution conversion: no downscale is wanted, so this is a plain
    /// row-by-row copy with the red/blue swap and an opaque alpha.
    ///
    /// The sampled path below walks two index maps and grows the buffer four
    /// bytes at a time, which costs tens of milliseconds on a 1080p frame — far
    /// too slow for the player, which converts every frame it decodes.
    fn convert_1to1(&self, frame: &RawFrame, mut rgba: Vec<u8>) -> PreviewImage {
        let (w, h) = self.dims;
        let row = w as usize * 4;
        let stride = frame.stride as usize;
        rgba.clear();
        rgba.resize(row * h as usize, 0);
        let swap = frame.format == PixelFormat::Bgra;
        for y in 0..h as usize {
            let src = &frame.data[y * stride..y * stride + row];
            let dst = &mut rgba[y * row..(y + 1) * row];
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0.iter()) {
                let (r, b) = if swap { (s[2], s[0]) } else { (s[0], s[2]) };
                d[0] = r;
                d[1] = s[1];
                d[2] = b;
                d[3] = 255;
            }
        }
        PreviewImage { width: w, height: h, rgba }
    }

    pub fn make(&self, frame: &RawFrame) -> PreviewImage {
        self.make_into(frame, Vec::new())
    }

    /// [`Previewer::make`] writing into `rgba` instead of a fresh allocation.
    pub fn make_into(&self, frame: &RawFrame, mut rgba: Vec<u8>) -> PreviewImage {
        if (frame.width, frame.height) != self.src {
            return Previewer::new(frame.width, frame.height, self.dims.0.max(self.dims.1))
                .make_into(frame, rgba);
        }
        let (w, h) = self.dims;
        if self.dims == self.src {
            return self.convert_1to1(frame, rgba);
        }
        rgba.clear();
        rgba.reserve((w * h * 4) as usize);
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
    fn full_size_conversion_matches_the_sampled_path() {
        let (w, h) = (17u32, 5u32);
        // Padded rows, so the stride is exercised too.
        let stride = w * 4 + 12;
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as u8;
                data.extend_from_slice(&[i, i.wrapping_add(70), i.wrapping_add(140), 3]);
            }
            data.extend_from_slice(&[0u8; 12]);
        }
        for format in [PixelFormat::Bgra, PixelFormat::Rgba] {
            let frame = RawFrame {
                data: data.clone(),
                width: w,
                height: h,
                stride,
                format,
                pts: Duration::ZERO,
                mouse: None,
            };
            // `max_side` at the long side means no downscale, so the fast path runs.
            let p = Previewer::new(w, h, w.max(h));
            assert_eq!(p.dims(), (w, h));
            let fast = p.make(&frame);
            // Same pixels the sampled loop would have produced.
            let mut want = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let s = y * stride as usize + x * 4;
                    let px = &frame.data[s..s + 4];
                    if format == PixelFormat::Bgra {
                        want.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    } else {
                        want.extend_from_slice(&[px[0], px[1], px[2], 255]);
                    }
                }
            }
            assert_eq!(fast.rgba, want, "{format:?}");
            // Alpha is forced opaque even though the source said 3.
            assert!(fast.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        }
    }

    #[test]
    fn make_into_reuses_the_buffer_it_is_given() {
        let (w, h) = (8u32, 4u32);
        let frame = RawFrame {
            data: vec![9u8; (w * h * 4) as usize],
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            pts: Duration::ZERO,
            mouse: None,
        };
        let p = Previewer::new(w, h, w.max(h));
        let recycled = Vec::with_capacity((w * h * 4) as usize);
        let ptr = recycled.as_ptr();
        let out = p.make_into(&frame, recycled);
        assert_eq!(out.rgba.as_ptr(), ptr, "the caller's allocation should be kept");
        assert_eq!(out.rgba.len(), (w * h * 4) as usize);
    }

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
