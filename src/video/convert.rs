//! Raw frame representation and BGRA/RGBA → I420 / NV12 conversion.

use std::time::Duration;

use anyhow::{bail, Result};
use openh264::formats::YUVSlices;

use super::encoder::{FrameInput, InputLayout};
use super::mouse_fx::MouseSnapshot;
use yuv::{
    bgra_to_yuv420, bgra_to_yuv_nv12, rgba_to_yuv420, rgba_to_yuv_nv12, BufferStoreMut, YuvBiPlanarImageMut,
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
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
    /// Pointer state sampled when the frame was captured (global pixels).
    pub mouse: Option<MouseSnapshot>,
}

impl RawFrame {
    /// An empty frame whose buffer is reused by the `*_into` scalers.
    pub fn empty(format: PixelFormat) -> RawFrame {
        RawFrame { data: Vec::new(), width: 0, height: 0, stride: 0, format, pts: Duration::ZERO, mouse: None }
    }

    /// Returns a copy at half resolution using a 2×2 box filter.
    pub fn downscale_half(&self) -> RawFrame {
        let mut out = RawFrame::empty(self.format);
        self.downscale_half_into(&mut out);
        out
    }

    /// Half-resolution 2×2 box filter into `dst`, reusing its buffer.
    pub fn downscale_half_into(&self, dst: &mut RawFrame) {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let s = self.stride as usize;
        let sw = self.width as usize;
        let sh = self.height as usize;
        dst.data.clear();
        dst.data.reserve((w * h * 4) as usize);
        for y in 0..h as usize {
            let r0 = &self.data[y * 2 * s..y * 2 * s + sw * 4];
            let r1 = &self.data[(y * 2 + 1).min(sh - 1) * s..(y * 2 + 1).min(sh - 1) * s + sw * 4];
            let (p0, rest0) = r0.as_chunks::<8>();
            let (p1, _) = r1.as_chunks::<8>();
            // Pairs of source pixels (8 bytes) → one destination pixel.
            for (a, b) in p0.iter().zip(p1) {
                for ch in 0..4 {
                    let sum = a[ch] as u32 + a[ch + 4] as u32 + b[ch] as u32 + b[ch + 4] as u32;
                    dst.data.push(((sum + 2) / 4) as u8);
                }
            }
            if p0.len() < w as usize {
                // Odd source width: the last destination pixel repeats the last column.
                let a = &rest0[..4];
                let b = &r1[r1.len() - 4..];
                for ch in 0..4 {
                    dst.data.push((a[ch] as u32 + b[ch] as u32).div_ceil(2) as u8);
                }
            }
        }
        dst.width = w;
        dst.height = h;
        dst.stride = w * 4;
        dst.format = self.format;
        dst.pts = self.pts;
        dst.mouse = self.mouse.clone();
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
        RawFrame { data, width: w, height: h, stride: w * 4, format: self.format, pts: self.pts, mouse: self.mouse.clone() }
    }
}

/// Reusable BGRA/RGBA → I420 or NV12 converter. Output dimensions are forced
/// even (4:2:0 requires it); odd source frames lose their last column/row.
pub struct Converter {
    layout: InputLayout,
    i420: Option<YuvPlanarImageMut<'static, u8>>,
    nv12: Option<YuvBiPlanarImageMut<'static, u8>>,
    dims: (u32, u32),
}

impl Converter {
    pub fn new(width: u32, height: u32, layout: InputLayout) -> Result<Self> {
        let (w, h) = even_dims(width, height);
        if w == 0 || h == 0 {
            bail!("frame too small to encode: {width}x{height}");
        }
        let (i420, nv12) = match layout {
            InputLayout::I420 => (Some(YuvPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420)), None),
            InputLayout::Nv12 => (None, Some(YuvBiPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420))),
        };
        Ok(Self { layout, i420, nv12, dims: (w, h) })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dims
    }

    pub fn layout(&self) -> InputLayout {
        self.layout
    }

    /// Converts `frame` into the internal buffers. The frame must match the
    /// converter dimensions after even rounding.
    pub fn convert(&mut self, frame: &RawFrame) -> Result<()> {
        let (w, h) = even_dims(frame.width, frame.height);
        if (w, h) != self.dims {
            bail!("frame size {}x{} does not match converter {}x{}", frame.width, frame.height, self.dims.0, self.dims.1);
        }
        let needed = frame.stride as usize * (h as usize - 1) + w as usize * 4;
        if frame.data.len() < needed {
            bail!("frame buffer too small: {} < {}", frame.data.len(), needed);
        }
        let data = &frame.data[..needed];
        let (range, matrix, mode) = (YuvRange::Limited, YuvStandardMatrix::Bt709, YuvConversionMode::Balanced);
        let res = match (self.layout, frame.format) {
            (InputLayout::I420, PixelFormat::Bgra) => {
                bgra_to_yuv420(self.i420.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::I420, PixelFormat::Rgba) => {
                rgba_to_yuv420(self.i420.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::Nv12, PixelFormat::Bgra) => {
                bgra_to_yuv_nv12(self.nv12.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::Nv12, PixelFormat::Rgba) => {
                rgba_to_yuv_nv12(self.nv12.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
        };
        res.map_err(|e| anyhow::anyhow!("yuv conversion failed: {e}"))?;
        Ok(())
    }

    /// Zero-copy view of the converted planes for the encoder.
    pub fn frame(&self) -> FrameInput<'_> {
        match self.layout {
            InputLayout::I420 => {
                let p = self.i420.as_ref().unwrap();
                FrameInput::I420 {
                    y: plane(&p.y_plane),
                    u: plane(&p.u_plane),
                    v: plane(&p.v_plane),
                    strides: (p.y_stride as usize, p.u_stride as usize, p.v_stride as usize),
                    dims: self.dims,
                }
            }
            InputLayout::Nv12 => {
                let p = self.nv12.as_ref().unwrap();
                FrameInput::Nv12 {
                    y: plane(&p.y_plane),
                    uv: plane(&p.uv_plane),
                    strides: (p.y_stride as usize, p.uv_stride as usize),
                    dims: self.dims,
                }
            }
        }
    }

    /// I420 planes as OpenH264 slices (I420 converters only).
    pub fn yuv(&self) -> YUVSlices<'_> {
        match self.frame() {
            FrameInput::I420 { y, u, v, strides, dims } => {
                YUVSlices::new((y, u, v), (dims.0 as usize, dims.1 as usize), strides)
            }
            FrameInput::Nv12 { .. } => panic!("yuv() called on an NV12 converter"),
        }
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
        RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None }
    }

    #[test]
    fn converts_solid_white_to_limited_range_y() {
        let frame = solid(16, 8, [255, 255, 255, 255]);
        let mut c = Converter::new(16, 8, InputLayout::I420).unwrap();
        c.convert(&frame).unwrap();
        let yuv = c.yuv();
        assert_eq!(yuv.dimensions(), (16, 8));
        assert!(yuv.y().iter().all(|&v| (230..=240).contains(&v)), "y={}", yuv.y()[0]);
        assert!(yuv.u().iter().all(|&v| (126..=130).contains(&v)));
    }

    #[test]
    fn converts_to_nv12() {
        let frame = solid(16, 8, [0, 0, 255, 255]); // pure red (BGRA)
        let mut c = Converter::new(16, 8, InputLayout::Nv12).unwrap();
        c.convert(&frame).unwrap();
        let FrameInput::Nv12 { y, uv, strides, dims } = c.frame() else { panic!("nv12") };
        assert_eq!(dims, (16, 8));
        assert_eq!(strides, (16, 16));
        assert_eq!(y.len(), 16 * 8);
        assert_eq!(uv.len(), 16 * 4);
        // Red: low luma, low Cb, high Cr.
        assert!(y[0] < 90, "y={}", y[0]);
        assert!(uv[0] < 128 && uv[1] > 200, "uv={:?}", &uv[..2]);
    }

    #[test]
    fn odd_sizes_round_down() {
        let frame = solid(15, 9, [0, 0, 0, 255]);
        let mut c = Converter::new(15, 9, InputLayout::I420).unwrap();
        assert_eq!(c.dimensions(), (14, 8));
        c.convert(&frame).unwrap();
    }

    #[test]
    fn downscale_and_crop() {
        let frame = solid(8, 6, [10, 20, 30, 255]);
        let half = frame.downscale_half();
        assert_eq!((half.width, half.height), (4, 3));
        assert_eq!(&half.data[..4], &[10, 20, 30, 255]);
        assert_eq!(half.data.len(), 4 * 3 * 4);
        // Odd sizes: 7×5 → 3×2, averaging 2×2 blocks.
        let mut odd = solid(7, 5, [0, 0, 0, 255]);
        odd.data[0] = 100; // B of pixel (0,0)
        odd.data[4] = 100; // B of pixel (1,0)
        let half = odd.downscale_half();
        assert_eq!((half.width, half.height), (3, 2));
        assert_eq!(half.data[0], 50);
        assert_eq!(half.data.len(), 3 * 2 * 4);
        let cropped = frame.crop(2, 2, 4, 2);
        assert_eq!((cropped.width, cropped.height), (4, 2));
        assert_eq!(cropped.data.len(), 32);
    }
}
