//! MalasDoh: SSH terminal, SFTP browser and safe remote editing in one window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;
mod browser;
mod editor;
mod forwards;
mod highlight;
mod session;
mod settings;
mod terminal;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "malasdoh=info,malasdoh_core=info".into()),
        )
        .init();
    let paths = match malasdoh_core::AppPaths::detect() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("MalasDoh cannot start: {e:#}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MalasDoh")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("MalasDoh", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths)))))
}
