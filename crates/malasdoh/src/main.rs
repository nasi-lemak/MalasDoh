//! MalasDoh: SSH terminal, SFTP browser and safe remote editing in one window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;
mod browser;
mod compare;
mod editor;
mod find_ui;
mod forwards;
mod highlight;
mod journal_ui;
mod keys_ui;
mod local;
mod logview;
mod multiedit;
mod multirun;
mod patch_ui;
mod queue;
mod restore;
mod session;
mod settings;
mod snippets;
mod sudo;
mod sync_ui;
mod teleport_ui;
mod terminal;
mod textops;
mod themes;

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
            fatal(&format!("MalasDoh cannot start: {e:#}"));
            std::process::exit(1);
        }
    };
    let crash_log = paths.data_dir.join("crash.log");
    install_crash_log(crash_log.clone());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MalasDoh")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 420.0])
            .with_drag_and_drop(true)
            .with_app_id("malasdoh")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon-128.png")).unwrap_or_default(),
            ),
        ..Default::default()
    };
    let result = eframe::run_native("MalasDoh", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths)))));
    if let Err(e) = &result {
        // Usually no usable OpenGL (a VM, a remote desktop, an old driver).
        let msg = format!(
            "MalasDoh could not open its window: {e}\n\nIt needs OpenGL 2 or newer; updating the graphics driver usually helps."
        );
        append_log(&crash_log, &msg);
        fatal(&msg);
    }
    result
}

/// Tell the user, also when there is no console to print to (Windows).
fn fatal(msg: &str) {
    eprintln!("{msg}");
    rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("MalasDoh").set_description(msg).show();
}

fn append_log(path: &std::path::Path, text: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let when = malasdoh_core::journal::format_time(malasdoh_core::journal::now());
        let _ = writeln!(f, "=== {when} · MalasDoh {} ===\n{text}\n", env!("CARGO_PKG_VERSION"));
    }
}

/// Write panics, with a backtrace, to `crash.log`. One on the main thread
/// ends the program, so the user is told where the report is; one in a
/// background task only fails that task.
fn install_crash_log(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        append_log(&path, &format!("panic in thread '{thread}': {info}\n{bt}"));
        default(info);
        if thread == "main" {
            fatal(&format!("MalasDoh crashed: {info}\n\nDetails were written to {}", path.display()));
        }
    }));
}
