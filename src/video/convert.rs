//! Raw frame representation and BGRA/RGBA → I420 conversion.

use std::time::Duration;

use anyhow::{bail, Result};
use openh264::formats::YUVSlices;
use yuv::{
    bgra_to_yuv420, rgba_to_yuv420, BufferStoreMut, YuvChromaSubsampling, YuvConversionMode,
    YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes per pixel: B, G, R, A (Windows.Graphics.Capture, DXGI).
    Bgra,
    /// 4 bytes per pixel: R, G, B, A (xcap, image crate).
    Rgba,
}

/// A captured frame in a packed 32-bit pixel format.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Bytes per row (>= width * 4).
    pub stride: u32,
    pub format: PixelFormat,
    /// Capture time relative to the recording epoch.
    pub pts: Duration,
}

impl RawFrame {
    /// Returns a copy at half resolution using a 2×2 box filter.
    pub fn downscale_half(&self) -> RawFrame {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let mut data = vec![0u8; (w * h * 4) as usize];
        let src = &self.data;
        let s = self.stride as usize;
        for y in 0..h as usize {
            let r0 = y * 2 * s;
            let r1 = (y * 2 + 1).min(self.height as usize - 1) * s;
            let dst_row = &mut data[y * w as usize * 4..(y + 1) * w as usize * 4];
            for x in 0..w as usize {
                let c0 = x * 8;
                let c1 = (x * 2 + 1).min(self.width as usize - 1) * 4;
                for ch in 0..4 {
                    let sum = src[r0 + c0 + ch] as u32
                        + src[r0 + c1 + ch] as u32
                        + src[r1 + c0 + ch] as u32
                        + src[r1 + c1 + ch] as u32;
                    dst_row[x * 4 + ch] = ((sum + 2) / 4) as u8;
                }
            }
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: self.format, pts: self.pts }
    }

    /// Returns a copy cropped to the given rectangle (clamped to the frame bounds).
    pub fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> RawFrame {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let w = width.min(self.width - x).max(1);
        let h = height.min(self.height - y).max(1);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let start = (row * self.stride + x * 4) as usize;
            data.extend_from_slice(&self.data[start..start + (w * 4) as usize]);
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: self.format, pts: self.pts }
    }
}

/// Reusable BGRA/RGBA → I420 converter. Output dimensions are forced even
/// (4:2:0 requires it); odd source frames lose their last column/row.
pub struct Converter {
    planar: YuvPlanarImageMut<'static, u8>,
}

impl Converter {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let (w, h) = even_dims(width, height);
        if w == 0 || h == 0 {
            bail!("frame too small to encode: {width}x{height}");
        }
        Ok(Self { planar: YuvPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420) })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.planar.width, self.planar.height)
    }

    /// Converts `frame` into the internal I420 buffers. The frame must match
    /// the converter dimensions after even rounding.
    pub fn convert(&mut self, frame: &RawFrame) -> Result<()> {
        let (w, h) = even_dims(frame.width, frame.height);
        if (w, h) != self.dimensions() {
            bail!(
                "frame size {}x{} does not match converter {}x{}",
                frame.width,
                frame.height,
                self.planar.width,
                self.planar.height
            );
        }
        let needed = frame.stride as usize * (h as usize - 1) + w as usize * 4;
        if frame.data.len() < needed {
            bail!("frame buffer too small: {} < {}", frame.data.len(), needed);
        }
        let data = &frame.data[..needed];
        let res = match frame.format {
            PixelFormat::Bgra => bgra_to_yuv420(
                &mut self.planar,
                data,
                frame.stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            ),
            PixelFormat::Rgba => rgba_to_yuv420(
                &mut self.planar,
                data,
                frame.stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            ),
        };
        res.map_err(|e| anyhow::anyhow!("yuv conversion failed: {e}"))?;
        Ok(())
    }

    /// Zero-copy view of the converted planes for the encoder.
    pub fn yuv(&self) -> YUVSlices<'_> {
        let p = &self.planar;
        YUVSlices::new(
            (plane(&p.y_plane), plane(&p.u_plane), plane(&p.v_plane)),
            (p.width as usize, p.height as usize),
            (p.y_stride as usize, p.u_stride as usize, p.v_stride as usize),
        )
    }
}

fn plane<'a>(store: &'a BufferStoreMut<'_, u8>) -> &'a [u8] {
    store.borrow()
}

/// Rounds dimensions down to even values.
pub fn even_dims(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> RawFrame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            data.extend_from_slice(&bgra);
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO }
    }

    #[test]
    fn converts_solid_white_to_limited_range_y() {
        let frame = solid(16, 8, [255, 255, 255, 255]);
        let mut c = Converter::new(16, 8).unwrap();
        c.convert(&frame).unwrap();
        let yuv = c.yuv();
        assert_eq!(yuv.dimensions(), (16, 8));
        assert!(yuv.y().iter().all(|&v| (230..=240).contains(&v)), "y={}", yuv.y()[0]);
        assert!(yuv.u().iter().all(|&v| (126..=130).contains(&v)));
    }

    #[test]
    fn odd_sizes_round_down() {
        let frame = solid(15, 9, [0, 0, 0, 255]);
        let mut c = Converter::new(15, 9).unwrap();
        assert_eq!(c.dimensions(), (14, 8));
        c.convert(&frame).unwrap();
    }

    #[test]
    fn downscale_and_crop() {
        let frame = solid(8, 6, [10, 20, 30, 255]);
        let half = frame.downscale_half();
        assert_eq!((half.width, half.height), (4, 3));
        assert_eq!(&half.data[..4], &[10, 20, 30, 255]);
        let cropped = frame.crop(2, 2, 4, 2);
        assert_eq!((cropped.width, cropped.height), (4, 2));
        assert_eq!(cropped.data.len(), 32);
    }
}
