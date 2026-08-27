#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("openclip")
            .with_app_id("openclip")
            .with_inner_size([800.0, 640.0])
            .with_min_inner_size([760.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("openclip", options, Box::new(|cc| Ok(Box::new(openclip::ui::App::new(cc)))))
}
