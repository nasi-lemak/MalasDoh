//! MalasDoh on Android: the same app as on the desktop, in eframe's Android
//! window (a `NativeActivity`, so the app needs no Java or Kotlin code).
//!
//! The on-screen keyboard comes up by itself while a text field has focus:
//! egui asks winit for text input, and winit's Android backend shows it.
#![cfg(target_os = "android")]

use malasdoh::app::App;
use std::time::{Duration, Instant};

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
            let mut app = App::with_context(&cc.egui_ctx, paths);
            app.set_touch(&cc.egui_ctx, true);
            Ok(Box::new(Mobile {
                app,
                android,
                focused: true,
                insets: Insets::default(),
                keyboard: false,
                settle_until: Instant::now() + SETTLE,
            }))
        }),
    );
    if let Err(e) = result {
        log::error!("MalasDoh could not start: {e}");
    }
}

/// How long to keep looking for new insets after something that moves them
/// (start, coming back, the keyboard showing or hiding).
const SETTLE: Duration = Duration::from_secs(3);

/// Window edges covered by the status bar, the navigation bar, a display
/// cutout or the on-screen keyboard, in pixels: left, top, right, bottom.
type Insets = [i32; 4];

struct Mobile {
    app: App,
    android: AndroidApp,
    focused: bool,
    /// The covered edges last seen.
    insets: Insets,
    /// Whether a text field had focus (the on-screen keyboard is up).
    keyboard: bool,
    /// Until when to look at the insets again every quarter second: Android
    /// changes them without an event that winit passes on.
    settle_until: Instant,
}

impl eframe::App for Mobile {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Android draws its bars and the keyboard over the window (always,
        // from Android 15); keep the app in the part left visible.
        let ppp = ctx.pixels_per_point();
        if ppp > 0.0 {
            let [left, top, right, bottom] = self.insets.map(|px| px.max(0) as f32 / ppp);
            raw.safe_area_insets = Some(egui::SafeAreaInsets(egui::epaint::MarginF32 { left, right, top, bottom }));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Android may end a backgrounded app without warning, so save what
        // is open (and unsaved edits as drafts) whenever it is left.
        let ctx = ui.ctx().clone();
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if self.focused && !focused {
            self.app.shutdown();
        }
        if focused && !self.focused {
            self.settle_until = Instant::now() + SETTLE;
        }
        self.focused = focused;

        self.app.frame_in_safe_area(ui);

        let keyboard = ctx.egui_wants_keyboard_input();
        if keyboard != self.keyboard {
            self.keyboard = keyboard;
            self.settle_until = Instant::now() + SETTLE;
        }
        if Instant::now() < self.settle_until {
            match insets(&self.android) {
                Ok(insets) if insets != self.insets => {
                    log::info!("insets (left, top, right, bottom): {insets:?}");
                    self.insets = insets;
                    ctx.request_repaint();
                }
                Ok(_) => ctx.request_repaint_after(Duration::from_millis(250)),
                Err(e) => {
                    log::warn!("could not read the window insets: {e}");
                    self.settle_until = Instant::now();
                }
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.shutdown();
    }
}

/// The window's insets, from Java: `getWindow().getDecorView()
/// .getRootWindowInsets()`, for the system bars, the display cutout and the
/// keyboard (Android 11+), or the older system-window insets (Android 8-10).
fn insets(android: &AndroidApp) -> Result<Insets, jni::errors::Error> {
    use jni::objects::{JObject, JValue};

    // SAFETY: android-activity keeps the VM and the activity alive for as long
    // as the app runs; the activity is a global reference we only borrow.
    let vm = unsafe { jni::JavaVM::from_raw(android.vm_as_ptr().cast()) }?;
    let mut env = vm.attach_current_thread_permanently()?;
    let activity = unsafe { JObject::from_raw(android.activity_as_ptr().cast()) };
    let result = env.with_local_frame(16, |env| -> Result<Insets, jni::errors::Error> {
        let window = env.call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
        let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
        let all = env.call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?.l()?;
        if all.is_null() {
            // Not attached to a window yet.
            return Ok(Insets::default());
        }
        if AndroidApp::sdk_version() >= 30 {
            let mut types = 0;
            for t in ["systemBars", "displayCutout", "ime"] {
                types |= env.call_static_method("android/view/WindowInsets$Type", t, "()I", &[])?.i()?;
            }
            let i = env.call_method(&all, "getInsets", "(I)Landroid/graphics/Insets;", &[JValue::Int(types)])?.l()?;
            let mut edge = |name: &str| env.get_field(&i, name, "I").and_then(|v| v.i());
            Ok([edge("left")?, edge("top")?, edge("right")?, edge("bottom")?])
        } else {
            let mut edge = |name: &str| env.call_method(&all, name, "()I", &[]).and_then(|v| v.i());
            Ok([
                edge("getSystemWindowInsetLeft")?,
                edge("getSystemWindowInsetTop")?,
                edge("getSystemWindowInsetRight")?,
                edge("getSystemWindowInsetBottom")?,
            ])
        }
    });
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result
}
