// No console window on Windows in release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod auth;
mod backend;
mod demo;
mod model;
mod soloist;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Spotimy")
            .with_inner_size([1080.0, 700.0])
            .with_min_inner_size([760.0, 480.0]),
        // No multisampling and no depth buffer: smaller framebuffers.
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };

    let mut backend = None;
    let result = eframe::run_native(
        "Spotimy",
        options,
        Box::new(|cc| {
            let (tx, rx, handle) = backend::spawn(cc.egui_ctx.clone());
            backend = Some(handle);
            Ok(Box::new(ui::App::new(cc, tx, rx)))
        }),
    );
    // The UI is closed. Wait for the backend: it stops the Soloist process.
    if let Some(handle) = backend {
        let _ = handle.join();
    }
    result
}
