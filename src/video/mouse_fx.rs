//! Mouse effects (cursor sprite, click ripples, highlight halo) painted
//! directly onto captured frames, plus the global mouse sampler that feeds them.

use std::time::{Duration, Instant};

use device_query::{DeviceQuery, DeviceState};

use super::{PixelFormat, RawFrame};

/// How long a click ripple stays visible.
pub const CLICK_DURATION: Duration = Duration::from_millis(600);
/// Base radius of the highlight halo at 100 %.
const HIGHLIGHT_RADIUS: f32 = 32.0;
/// Ripple radius range at 100 %.
const RIPPLE_START: f32 = 10.0;
const RIPPLE_END: f32 = 42.0;
const RIPPLE_THICKNESS: f32 = 3.0;

/// User-facing mouse effect settings (mirrors classic recorder options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseFx {
    pub show_cursor: bool,
    /// Cursor size in percent; 100 keeps the native cursor drawn by the capture API.
    pub cursor_size: u32,
    pub click_effect: bool,
    pub click_size: u32,
    pub left_color: [u8; 3],
    pub right_color: [u8; 3],
    pub highlight: bool,
    pub highlight_size: u32,
    pub highlight_color: [u8; 3],
    /// Highlight opacity in percent.
    pub highlight_opacity: u32,
}

impl Default for MouseFx {
    fn default() -> Self {
        Self {
            show_cursor: true,
            cursor_size: 100,
            click_effect: true,
            click_size: 100,
            left_color: [255, 0, 0],
            right_color: [0, 0, 255],
            highlight: true,
            highlight_size: 100,
            highlight_color: [255, 255, 0],
            highlight_opacity: 25,
        }
    }
}

impl MouseFx {
    /// Whether the capture API should draw the real cursor.
    pub fn native_cursor(&self) -> bool {
        self.show_cursor && self.cursor_size == 100
    }

    /// Whether the app draws its own (scaled) cursor sprite.
    pub fn draws_cursor(&self) -> bool {
        self.show_cursor && self.cursor_size != 100
    }

    /// Whether any per-frame overlay work is needed.
    pub fn any_overlay(&self) -> bool {
        self.draws_cursor() || self.click_effect || self.highlight
    }

    /// Paints the effects onto `frame`. `cursor` is in frame pixels, `clicks`
    /// are (frame position, age 0..1, is_right). `scale` scales effect sizes
    /// (0.5 for half-resolution frames).
    pub fn apply(&self, frame: &mut RawFrame, cursor: (i32, i32), clicks: &[FrameClick], scale: f32) {
        if self.highlight && self.highlight_opacity > 0 {
            let r = HIGHLIGHT_RADIUS * self.highlight_size as f32 / 100.0 * scale;
            let alpha = self.highlight_opacity.min(100) as f32 / 100.0;
            fill_disc(frame, cursor.0 as f32, cursor.1 as f32, r, self.highlight_color, alpha);
        }
        if self.click_effect {
            for &(x, y, age, right) in clicks {
                let t = age.clamp(0.0, 1.0);
                let k = self.click_size as f32 / 100.0 * scale;
                let r = (RIPPLE_START + (RIPPLE_END - RIPPLE_START) * t) * k;
                let color = if right { self.right_color } else { self.left_color };
                ring(frame, x as f32, y as f32, r, RIPPLE_THICKNESS * k.max(0.5), color, 0.9 * (1.0 - t));
            }
        }
        if self.draws_cursor() {
            draw_arrow(frame, cursor.0, cursor.1, self.cursor_size as f32 / 100.0 * scale);
        }
    }
}

/// A click mapped into frame space: (x, y, age 0..1, is_right).
pub type FrameClick = (i32, i32, f32, bool);

/// A recorded button press.
#[derive(Debug, Clone, Copy)]
pub struct Click {
    pub at: Instant,
    /// Global (virtual desktop) pixel position.
    pub pos: (i32, i32),
    pub right: bool,
}

/// Polls the global mouse state and turns button presses into clicks.
pub struct MouseSampler {
    state: DeviceState,
    was_left: bool,
    was_right: bool,
    clicks: Vec<Click>,
    pub pos: (i32, i32),
}

// SAFETY: the sampler is only ever used from the thread that owns it (the
// encode thread or the preview capture thread); it is moved there once.
unsafe impl Send for MouseSampler {}

#[cfg(target_os = "linux")]
const BTN_LEFT: usize = 1;
#[cfg(target_os = "linux")]
const BTN_RIGHT: usize = 3;
#[cfg(not(target_os = "linux"))]
const BTN_LEFT: usize = 1;
#[cfg(not(target_os = "linux"))]
const BTN_RIGHT: usize = 2;

impl Default for MouseSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl MouseSampler {
    pub fn new() -> Self {
        Self { state: DeviceState::new(), was_left: false, was_right: false, clicks: Vec::new(), pos: (0, 0) }
    }

    /// Reads the current position/buttons; returns the live clicks.
    pub fn sample(&mut self) -> &[Click] {
        let m = self.state.get_mouse();
        self.pos = m.coords;
        let now = Instant::now();
        let left = m.button_pressed.get(BTN_LEFT).copied().unwrap_or(false);
        let right = m.button_pressed.get(BTN_RIGHT).copied().unwrap_or(false);
        if left && !self.was_left {
            self.clicks.push(Click { at: now, pos: self.pos, right: false });
        }
        if right && !self.was_right {
            self.clicks.push(Click { at: now, pos: self.pos, right: true });
        }
        self.was_left = left;
        self.was_right = right;
        self.clicks.retain(|c| now.duration_since(c.at) < CLICK_DURATION);
        if self.clicks.len() > 8 {
            let drop = self.clicks.len() - 8;
            self.clicks.drain(..drop);
        }
        &self.clicks
    }

    /// Maps the sampled state into frame coordinates for [`MouseFx::apply`].
    pub fn mapped(&self, origin: (i32, i32), scale: f32) -> ((i32, i32), Vec<FrameClick>) {
        let now = Instant::now();
        let map = |p: (i32, i32)| (((p.0 - origin.0) as f32 * scale) as i32, ((p.1 - origin.1) as f32 * scale) as i32);
        let cursor = map(self.pos);
        let clicks = self
            .clicks
            .iter()
            .map(|c| {
                let (x, y) = map(c.pos);
                let age = now.duration_since(c.at).as_secs_f32() / CLICK_DURATION.as_secs_f32();
                (x, y, age, c.right)
            })
            .collect();
        (cursor, clicks)
    }
}

// ----- rasterisation ---------------------------------------------------------

#[inline]
fn blend(frame: &mut RawFrame, x: i32, y: i32, rgb: [u8; 3], alpha: f32) {
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 || alpha <= 0.0 {
        return;
    }
    let i = y as usize * frame.stride as usize + x as usize * 4;
    let Some(px) = frame.data.get_mut(i..i + 4) else { return };
    let (ri, gi, bi) = match frame.format {
        PixelFormat::Bgra => (2, 1, 0),
        PixelFormat::Rgba => (0, 1, 2),
    };
    let a = alpha.min(1.0);
    px[ri] = (px[ri] as f32 + (rgb[0] as f32 - px[ri] as f32) * a) as u8;
    px[gi] = (px[gi] as f32 + (rgb[1] as f32 - px[gi] as f32) * a) as u8;
    px[bi] = (px[bi] as f32 + (rgb[2] as f32 - px[bi] as f32) * a) as u8;
}

fn fill_disc(frame: &mut RawFrame, cx: f32, cy: f32, r: f32, rgb: [u8; 3], alpha: f32) {
    if r <= 0.0 {
        return;
    }
    let (x0, x1) = ((cx - r - 1.0).floor() as i32, (cx + r + 1.0).ceil() as i32);
    let (y0, y1) = ((cy - r - 1.0).floor() as i32, (cy + r + 1.0).ceil() as i32);
    for y in y0.max(0)..=y1.min(frame.height as i32 - 1) {
        for x in x0.max(0)..=x1.min(frame.width as i32 - 1) {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let cov = (r + 0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                blend(frame, x, y, rgb, alpha * cov);
            }
        }
    }
}

fn ring(frame: &mut RawFrame, cx: f32, cy: f32, r: f32, thickness: f32, rgb: [u8; 3], alpha: f32) {
    if r <= 0.0 || alpha <= 0.0 {
        return;
    }
    let ext = r + thickness + 1.0;
    let (x0, x1) = ((cx - ext).floor() as i32, (cx + ext).ceil() as i32);
    let (y0, y1) = ((cy - ext).floor() as i32, (cy + ext).ceil() as i32);
    for y in y0.max(0)..=y1.min(frame.height as i32 - 1) {
        for x in x0.max(0)..=x1.min(frame.width as i32 - 1) {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let cov = (thickness / 2.0 + 0.5 - (d - r).abs()).clamp(0.0, 1.0);
            if cov > 0.0 {
                blend(frame, x, y, rgb, alpha * cov);
            }
        }
    }
}

/// Classic arrow cursor, 12×19: `.` transparent, `X` black outline, `W` white fill.
pub const ARROW: [&str; 19] = [
    "X...........",
    "XX..........",
    "XWX.........",
    "XWWX........",
    "XWWWX.......",
    "XWWWWX......",
    "XWWWWWX.....",
    "XWWWWWWX....",
    "XWWWWWWWX...",
    "XWWWWWWWWX..",
    "XWWWWWWWWWX.",
    "XWWWWWWWWWWX",
    "XWWWWWWXXXXX",
    "XWWWXWWX....",
    "XWWX.XWWX...",
    "XWX...XWWX..",
    "XX....XWWX..",
    "X......XWWX.",
    "........XX..",
];

fn draw_arrow(frame: &mut RawFrame, x: i32, y: i32, scale: f32) {
    let scale = scale.max(0.25);
    let w = (12.0 * scale).round() as i32;
    let h = (19.0 * scale).round() as i32;
    for dy in 0..h {
        let sy = ((dy as f32 + 0.5) / scale) as usize;
        let row = ARROW[sy.min(18)].as_bytes();
        for dx in 0..w {
            let sx = ((dx as f32 + 0.5) / scale) as usize;
            match row.get(sx.min(11)) {
                Some(b'X') => blend(frame, x + dx, y + dy, [0, 0, 0], 1.0),
                Some(b'W') => blend(frame, x + dx, y + dy, [255, 255, 255], 1.0),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(w: u32, h: u32) -> RawFrame {
        RawFrame {
            data: vec![0u8; (w * h * 4) as usize],
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            pts: Duration::ZERO,
        }
    }

    #[test]
    fn highlight_tints_center_pixel() {
        let fx = MouseFx { click_effect: false, ..Default::default() };
        let mut f = frame(64, 64);
        fx.apply(&mut f, (32, 32), &[], 1.0);
        let i = (32 * 64 + 32) * 4;
        // BGRA: yellow at 25 % over black → B stays 0, G and R ≈ 64.
        assert_eq!(f.data[i], 0);
        assert!((60..=66).contains(&f.data[i + 1]), "g={}", f.data[i + 1]);
        assert!((60..=66).contains(&f.data[i + 2]), "r={}", f.data[i + 2]);
        // Far corner untouched.
        assert_eq!(&f.data[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn scaled_arrow_and_ripple_stay_in_bounds() {
        let fx = MouseFx { cursor_size: 200, highlight: false, ..Default::default() };
        let mut f = frame(40, 30);
        fx.apply(&mut f, (35, 25), &[(2, 2, 0.5, true)], 1.0);
        // Top-left of the arrow is black outline.
        let i = (25 * 40 + 35) * 4;
        assert_eq!(&f.data[i..i + 3], &[0, 0, 0]);
        assert!(fx.draws_cursor());
        assert!(!fx.native_cursor());
    }
}
