#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod input;
mod sessions;
mod ssh;
mod terminal;

use app::RusttyApp;

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Rustty - SSH client")
        .with_inner_size([1120.0, 720.0])
        .with_min_inner_size([780.0, 500.0]);

    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "Rustty",
        options,
        Box::new(|cc| Ok(Box::new(RusttyApp::new(cc)))),
    )
}
