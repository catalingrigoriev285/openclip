//! The in-game frame-rate counter.
//!
//! The number is the game's own present rate, measured inside the hook, and its
//! colour says what openclip is doing with it: **green** while armed and ready
//! to record, **red** while recording. That is the convention, RTSS and
//! the other in-game recorders use, and it is the only feedback a player gets
//! without alt-tabbing out of a fullscreen game.

use crate::layout::{Corner, Layout};
use crate::sprite::Sprite;
use crate::text::TextRenderer;

/// Hooked and ready to record — openclip's systemGreen.
pub const READY: [u8; 3] = [0x30, 0xd1, 0x58];
/// Recording — openclip's systemRed.
pub const RECORDING: [u8; 3] = [0xff, 0x45, 0x3a];

const SCRIM: [u8; 3] = [0x00, 0x00, 0x00];
const SCRIM_ALPHA: f32 = 0.42;
const HAIRLINE: [u8; 3] = [0xff, 0xff, 0xff];
const HAIRLINE_ALPHA: f32 = 0.10;
const TEXT_ALPHA: f32 = 1.0;

/// Smaller and tighter to the corner than the watermark: it sits on top of
/// someone's game, so it stays out of the way.
pub const LAYOUT: Layout = Layout {
    height_ratio: 0.034,
    min_height: 18.0,
    max_height: 52.0,
    margin_ratio: 0.6,
    max_width_fraction: 0.35,
    max_height_fraction: 0.18,
};

/// What openclip is doing, which is what the colour encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookState {
    /// Hooked, counter showing, not recording.
    Ready,
    Recording,
}

impl HookState {
    pub fn color(self) -> [u8; 3] {
        match self {
            HookState::Ready => READY,
            HookState::Recording => RECORDING,
        }
    }

    pub fn from_u32(v: u32) -> Self {
        if v == 1 { HookState::Recording } else { HookState::Ready }
    }

    pub fn as_u32(self) -> u32 {
        match self {
            HookState::Ready => 0,
            HookState::Recording => 1,
        }
    }
}

/// User-facing counter settings. Percentages are integers so the struct derives
/// `Eq` and the settings pages can diff it with `!=`, like openclip's
/// `Watermark` and `MouseFx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct FpsOverlay {
    pub enabled: bool,
    pub position: Corner,
    /// Counter size in percent of its natural size.
    pub size: u32,
    /// Counter opacity in percent.
    pub opacity: u32,
    /// Also burn the counter into the recording. Off by default: the counter is
    /// there to tell *you* what is happening, and most people do not want a
    /// number baked into the footage they publish.
    pub in_recording: bool,
}

impl Default for FpsOverlay {
    fn default() -> Self {
        // Top-left is where every other in-game counter puts itself, so it is
        // the corner players already look at — and the one least likely to sit
        // under a crosshair, minimap or ammo counter.
        Self { enabled: true, position: Corner::TopLeft, size: 100, opacity: 100, in_recording: false }
    }
}

impl FpsOverlay {
    /// Whether anything would be drawn at all.
    pub fn any_overlay(&self) -> bool {
        self.enabled && self.opacity > 0
    }

    pub fn badge_height(&self, frame_h: u32) -> u32 {
        LAYOUT.height(frame_h, self.size)
    }

    pub fn place(&self, sprite: (u32, u32), frame: (u32, u32)) -> Option<(i32, i32)> {
        LAYOUT.place(self.position, sprite, frame)
    }
}

/// Composes and caches the counter badge.
///
/// The cache key is the whole appearance — height, text and colour — because
/// this runs inside a game's present call, where recomposing a sprite every
/// frame would be the overlay's largest cost. The number only changes a few
/// times a second, so in practice this composes about that often.
pub struct FpsBadge {
    text: TextRenderer,
    cache: Option<(u32, String, [u8; 3], Sprite)>,
}

impl FpsBadge {
    /// `None` if the bundled font cannot be parsed; the caller then runs
    /// without a counter rather than failing.
    pub fn new() -> Option<Self> {
        Some(Self { text: TextRenderer::new()?, cache: None })
    }

    /// The counter reading `fps`, at `height` pixels tall, in `rgb`.
    pub fn sprite(&mut self, height: u32, fps: f32, rgb: [u8; 3]) -> &Sprite {
        self.sprite_for(height, &format_fps(fps), rgb)
    }

    /// The counter showing an arbitrary string — used by the settings preview,
    /// which has no live game to read a rate from.
    pub fn sprite_for(&mut self, height: u32, text: &str, rgb: [u8; 3]) -> &Sprite {
        let height = height.max(1);
        let stale = match &self.cache {
            Some((h, t, c, _)) => *h != height || t != text || *c != rgb,
            None => true,
        };
        if stale {
            let sprite = self.compose(height, text, rgb);
            self.cache = Some((height, text.to_string(), rgb, sprite));
        }
        &self.cache.as_ref().expect("just composed").3
    }

    fn compose(&self, height: u32, text: &str, rgb: [u8; 3]) -> Sprite {
        let h = height as f32;
        let pad = h * 0.36;
        let font_px = h * 0.62;
        // Width is measured from a digit-count-stable sample, not from `text`
        // itself: sizing to the live string makes the badge twitch every time
        // the rate crosses 100, and a moving overlay is worse than a wide one.
        let sample = "0".repeat(text.chars().count().max(2));
        let text_w = self.text.width(&sample, font_px).max(self.text.width(text, font_px));
        let width = (pad + text_w + pad).ceil().max(1.0) as u32;

        let mut sprite = Sprite::new(width, height);
        sprite.fill_pill(h / 2.0, (SCRIM, SCRIM_ALPHA), (HAIRLINE, HAIRLINE_ALPHA));

        // Centred, so a two- and three-digit reading look equally deliberate.
        let pen = (width as f32 - self.text.width(text, font_px)) / 2.0;
        let baseline = self.text.baseline(font_px, h);
        self.text.draw(&mut sprite, text, font_px, (pen, baseline), rgb, TEXT_ALPHA);
        sprite
    }
}

/// The reading as a whole number, the way every in-game counter shows it.
pub fn format_fps(fps: f32) -> String {
    if !fps.is_finite() || fps <= 0.0 {
        return "--".into();
    }
    format!("{}", fps.round().clamp(0.0, 9999.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_colours_round_trip() {
        assert_eq!(HookState::Ready.color(), READY);
        assert_eq!(HookState::Recording.color(), RECORDING);
        for s in [HookState::Ready, HookState::Recording] {
            assert_eq!(HookState::from_u32(s.as_u32()), s);
        }
    }

    #[test]
    fn formats_a_reading() {
        assert_eq!(format_fps(59.6), "60");
        assert_eq!(format_fps(0.0), "--");
        assert_eq!(format_fps(f32::NAN), "--");
        assert_eq!(format_fps(-3.0), "--");
        assert_eq!(format_fps(1e9), "9999");
    }

    #[test]
    fn badge_is_cached_until_the_reading_changes() {
        let mut b = FpsBadge::new().unwrap();
        let first = b.sprite(32, 60.0, READY).clone();
        // Same inputs: the cached sprite is returned unchanged.
        assert_eq!(b.sprite(32, 60.2, READY).rgba, first.rgba);
        // A different colour recomposes.
        assert_ne!(b.sprite(32, 60.0, RECORDING).rgba, first.rgba);
    }

    #[test]
    fn badge_width_does_not_twitch_between_two_and_three_digits() {
        let mut b = FpsBadge::new().unwrap();
        let two = b.sprite(32, 60.0, READY).width;
        let also_two = b.sprite(32, 99.0, READY).width;
        assert_eq!(two, also_two);
        // Three digits are allowed to be wider, but never narrower.
        assert!(b.sprite(32, 144.0, READY).width >= two);
    }

    #[test]
    fn badge_draws_the_state_colour() {
        let mut b = FpsBadge::new().unwrap();
        let s = b.sprite(40, 120.0, RECORDING);
        let reddish = (0..s.height)
            .flat_map(|y| (0..s.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let (rgb, a) = s.get(x, y);
                a > 0.8 && rgb[0] > 200 && rgb[1] < 120
            })
            .count();
        assert!(reddish > 0, "the counter should be painted in the recording colour");
    }

    #[test]
    fn defaults_are_on_in_the_top_left_and_out_of_the_recording() {
        let d = FpsOverlay::default();
        assert!(d.any_overlay());
        assert_eq!(d.position, Corner::TopLeft);
        assert!(!d.in_recording);
    }
}
