//! Monitor / window enumeration and one-shot screenshots via `xcap`.
//!
//! Used on every platform for the source list, the region picker background
//! and idle thumbnails. Coordinates and sizes are physical pixels.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use xcap::{Monitor, Window};

use crate::video::{PixelFormat, RawFrame};

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

impl MonitorInfo {
    pub fn label(&self) -> String {
        format!(
            "{}{} ({}×{})",
            self.name,
            if self.is_primary { " ★" } else { "" },
            self.width,
            self.height
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowInfo {
    pub fn label(&self) -> String {
        let mut title = self.title.clone();
        if title.chars().count() > 60 {
            title = title.chars().take(57).collect::<String>() + "…";
        }
        if self.app_name.is_empty() {
            title
        } else {
            format!("{title} — {}", self.app_name)
        }
    }
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    for (i, m) in Monitor::all().context("enumerating monitors")?.into_iter().enumerate() {
        let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_default();
        let name = if name.trim().is_empty() || name.starts_with("Unknown") {
            format!("Display {}", i + 1)
        } else {
            name
        };
        let info = MonitorInfo {
            id: m.id()?,
            name,
            x: m.x()?,
            y: m.y()?,
            width: m.width()?,
            height: m.height()?,
            scale_factor: m.scale_factor().unwrap_or(1.0),
            is_primary: m.is_primary().unwrap_or(false),
        };
        if info.width > 0 && info.height > 0 {
            out.push(info);
        }
    }
    out.sort_by_key(|m| (!m.is_primary, m.x, m.y));
    Ok(out)
}

/// Lists visible, titled, non-minimized top-level windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let mut out = Vec::new();
    for w in Window::all().context("enumerating windows")? {
        if w.is_minimized().unwrap_or(false) {
            continue;
        }
        let title = w.title().unwrap_or_default();
        let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
        if title.trim().is_empty() || width < 32 || height < 32 {
            continue;
        }
        out.push(WindowInfo {
            id: w.id()?,
            title,
            app_name: w.app_name().unwrap_or_default(),
            x: w.x().unwrap_or(0),
            y: w.y().unwrap_or(0),
            width,
            height,
        });
    }
    Ok(out)
}

fn find_monitor(id: u32) -> Result<Monitor> {
    Monitor::all()?
        .into_iter()
        .find(|m| m.id().map(|i| i == id).unwrap_or(false))
        .ok_or_else(|| anyhow!("monitor {id} not found"))
}

fn find_window(id: u32) -> Result<Window> {
    Window::all()?
        .into_iter()
        .find(|w| w.id().map(|i| i == id).unwrap_or(false))
        .ok_or_else(|| anyhow!("window {id} not found"))
}

fn image_to_frame(img: xcap::image::RgbaImage) -> RawFrame {
    let (w, h) = (img.width(), img.height());
    RawFrame { data: img.into_raw(), width: w, height: h, stride: w * 4, format: PixelFormat::Rgba, pts: Duration::ZERO, mouse: None }
}

/// Takes a screenshot of a monitor (RGBA).
pub fn screenshot_monitor(id: u32) -> Result<RawFrame> {
    let m = find_monitor(id)?;
    Ok(image_to_frame(m.capture_image().context("monitor screenshot")?))
}

/// Takes a screenshot of a window (RGBA).
pub fn screenshot_window(id: u32) -> Result<RawFrame> {
    let w = find_window(id)?;
    Ok(image_to_frame(w.capture_image().context("window screenshot")?))
}

/// Takes a screenshot of a monitor sub-rectangle (RGBA).
pub fn screenshot_region(monitor_id: u32, rect: super::Rect) -> Result<RawFrame> {
    let m = find_monitor(monitor_id)?;
    Ok(image_to_frame(
        m.capture_region(rect.x, rect.y, rect.width, rect.height).context("region screenshot")?,
    ))
}

/// Top-left corner of a source's frame in global (virtual desktop) physical
/// pixels, used to map the mouse position into frame coordinates.
pub fn source_origin(source: &super::Source) -> Result<(i32, i32)> {
    match source {
        super::Source::Monitor { id } => {
            let m = find_monitor(*id)?;
            Ok((m.x()?, m.y()?))
        }
        super::Source::Region { monitor_id, rect } => {
            let m = find_monitor(*monitor_id)?;
            Ok((m.x()? + rect.x as i32, m.y()? + rect.y as i32))
        }
        super::Source::Window { id } => {
            let w = find_window(*id)?;
            Ok((w.x()?, w.y()?))
        }
        // A game frame comes from its own back buffer, not from anywhere on the
        // desktop, so there is no origin to map global mouse coordinates through.
        super::Source::Game { .. } => Ok((0, 0)),
    }
}

/// Screenshot of any [`super::Source`], used for idle thumbnails.
pub fn screenshot_source(source: &super::Source) -> Result<RawFrame> {
    match source {
        super::Source::Monitor { id } => screenshot_monitor(*id),
        super::Source::Window { id } => screenshot_window(*id),
        super::Source::Region { monitor_id, rect } => screenshot_region(*monitor_id, *rect),
        // There is no way to sample a game's back buffer from outside; a preview
        // or snapshot has to come from the hook's own frames instead.
        super::Source::Game { .. } => Err(anyhow!("a game cannot be captured as a still")),
    }
}
