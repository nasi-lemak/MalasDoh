//! MalasDoh on Android: the same app as on the desktop, in eframe's Android
//! window (a `NativeActivity`, so the app needs no Java or Kotlin code).
//!
//! The on-screen keyboard comes up by itself while a text field has focus:
//! egui asks winit for text input, and winit's Android backend shows it.
#![cfg(target_os = "android")]

use malasdoh::app::App;
use winit::platform::android::activity::AndroidApp;

/// Called by `android-activity` on the app's own thread once the activity
/// has started.
#[unsafe(no_mangle)]
fn android_main(android: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("malasdoh"),
    );
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}\n{}", std::backtrace::Backtrace::force_capture());
        default(info);
    }));

    // The app's private storage: removed with the app, unreadable by others.
    let root = android.internal_data_path().unwrap_or_else(|| "/data/local/tmp".into()).join("malasdoh");
    let paths = malasdoh_core::AppPaths::rooted(root);

    let options = eframe::NativeOptions {
        android_app: Some(android.clone()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let result = eframe::run_native(
        "MalasDoh",
        options,
        Box::new(move |cc| {
            Ok(Box::new(Mobile { app: App::with_context(&cc.egui_ctx, paths), android, focused: true }))
        }),
    );
    if let Err(e) = result {
        log::error!("MalasDoh could not start: {e}");
    }
}

struct Mobile {
    app: App,
    android: AndroidApp,
    focused: bool,
}

impl eframe::App for Mobile {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Android draws the status bar, the navigation bar and the on-screen
        // keyboard over the window; keep the app in the part left visible.
        let visible = self.android.content_rect();
        let ppp = ctx.pixels_per_point();
        let screen = raw.screen_rect.unwrap_or_else(|| ctx.viewport_rect());
        if visible.right > visible.left && visible.bottom > visible.top && ppp > 0.0 {
            let (w, h) = (screen.width() * ppp, screen.height() * ppp);
            raw.safe_area_insets = Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
                left: visible.left.max(0) as f32 / ppp,
                top: visible.top.max(0) as f32 / ppp,
                right: (w - visible.right as f32).max(0.0) / ppp,
                bottom: (h - visible.bottom as f32).max(0.0) / ppp,
            }));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Android may end a backgrounded app without warning, so save what
        // is open (and unsaved edits as drafts) whenever it is left.
        let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
        if self.focused && !focused {
            self.app.shutdown();
        }
        self.focused = focused;
        self.app.frame(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.shutdown();
    }
}
