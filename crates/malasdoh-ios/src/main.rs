//! MalasDoh on iOS and iPadOS: the same app as on the desktop, in eframe's
//! iOS window. egui keeps clear of the notch and the home indicator, and
//! winit shows the on-screen keyboard while a text field has focus.

#[cfg(target_os = "ios")]
fn main() {
    use malasdoh::app::App;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Warnings from everything: eframe reports why it could not
                // start (and then quits) only in its log.
                .unwrap_or_else(|_| "warn,winit=error,malasdoh=info,malasdoh_core=info".into()),
        )
        .init();

    // The app's sandbox: Library/Application Support is kept, backed up
    // with the device, and not shown in the Files app.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let paths = malasdoh_core::AppPaths::rooted(home.join("Library/Application Support/MalasDoh"));

    let mut options = eframe::NativeOptions { renderer: eframe::Renderer::Wgpu, ..Default::default() };
    options.wgpu_options.wgpu_setup = wgpu_setup();
    // On iOS this only returns if the window could not be made at all.
    let result = eframe::run_native(
        "MalasDoh",
        options,
        Box::new(move |cc| Ok(Box::new(Mobile { app: App::with_context(&cc.egui_ctx, paths), focused: true }))),
    );
    if let Err(e) = result {
        eprintln!("MalasDoh could not start: {e}");
    }
}

/// egui-wgpu asks for WebGPU's default limits, which the iOS Simulator's GPU
/// falls just short of (15 shader variables between stages, not 16), so
/// eframe gives up and the app quits. egui needs far less: ask for what the
/// GPU has.
#[cfg(target_os = "ios")]
fn wgpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew, wgpu};

    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = std::sync::Arc::new(|adapter| wgpu::DeviceDescriptor {
        label: Some("MalasDoh"),
        required_limits: adapter.limits(),
        ..Default::default()
    });
    WgpuSetup::CreateNew(setup)
}

#[cfg(target_os = "ios")]
struct Mobile {
    app: malasdoh::app::App,
    focused: bool,
}

#[cfg(target_os = "ios")]
impl eframe::App for Mobile {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // iOS may end a backgrounded app without warning, so save what is
        // open (and unsaved edits as drafts) whenever it is left.
        let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
        if self.focused && !focused {
            self.app.shutdown();
        }
        self.focused = focused;
        self.app.frame(ui);
    }

    fn on_exit(&mut self) {
        self.app.shutdown();
    }
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("This is MalasDoh's iOS program; build it on a Mac with ios/build-app.sh.");
}
