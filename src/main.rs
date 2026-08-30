#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

/// Application icon (same artwork as `assets/favicon.ico`), decoded at startup
/// for the window title bar / taskbar / dock.
const APP_ICON_PNG: &[u8] = openclip::video::watermark::LOGO_PNG;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("openclip")
            .with_app_id("openclip")
            .with_inner_size(openclip::ui::WINDOW_SIZE)
            .with_min_inner_size(openclip::ui::WINDOW_SIZE)
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native("openclip", options, Box::new(|cc| Ok(Box::new(openclip::ui::App::new(cc)))))
}

fn app_icon() -> egui::IconData {
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => icon,
        Err(e) => {
            log::warn!("failed to decode app icon: {e}");
            egui::IconData::default()
        }
    }
}
