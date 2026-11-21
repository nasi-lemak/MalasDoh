//! MalasDoh on Android: the same app as on the desktop, in eframe's Android
//! window. The activity is Android's `NativeActivity` with a little Java on
//! top (`android/java/…/MainActivity.java`) for what it lacks: a real
//! connection to the on-screen keyboard (suggestions, swipe typing,
//! composing input methods) and the clipboard.
#![cfg(target_os = "android")]

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::jint;
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

    let reader = android.clone();
    malasdoh::clipboard::set_reader(move || {
        with_activity(&reader, |env, activity| {
            let text = env.call_method(activity, "getClipboard", "()Ljava/lang/String;", &[])?.l()?;
            if text.is_null() {
                return Ok(None);
            }
            Ok(Some(env.get_string(&JString::from(text))?.into()))
        })
        .unwrap_or_else(|e| {
            log::warn!("could not read the clipboard: {e}");
            None
        })
    });

    let options = eframe::NativeOptions {
        android_app: Some(android.clone()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let result = eframe::run_native(
        "MalasDoh",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = App::with_context(&cc.egui_ctx, paths);
            app.set_touch(&cc.egui_ctx, true);
            Ok(Box::new(Mobile {
                app,
                android,
                focused: true,
                insets: Insets::default(),
                keyboard: None,
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

/// The keyboard modes of `MainActivity.showKeyboard`.
const TEXT: i32 = 0;
const PASSWORD: i32 = 1;
const TERMINAL: i32 = 2;

struct Mobile {
    app: App,
    android: AndroidApp,
    focused: bool,
    /// The covered edges last seen.
    insets: Insets,
    /// The on-screen keyboard as last asked for: up, in which mode.
    keyboard: Option<i32>,
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
        // What the on-screen keyboard typed.
        raw.events.append(&mut TYPED.lock().unwrap_or_else(|e| e.into_inner()));
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

        self.copy_to_clipboard(&ctx);
        self.keyboard(&ctx);

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

impl Mobile {
    /// Show the on-screen keyboard while something wants text, in the mode
    /// that suits it, and hide it after. NativeActivity's own keyboard (which
    /// winit would show) only sends key presses, so egui's request for it is
    /// taken here instead.
    fn keyboard(&mut self, ctx: &egui::Context) {
        let wanted = ctx.output_mut(|o| o.ime.take()).map(|ime| match ime.purpose {
            egui::IMEPurpose::Password => PASSWORD,
            egui::IMEPurpose::Terminal => TERMINAL,
            egui::IMEPurpose::Normal => TEXT,
        });
        // A tap brings it back after the Back key put it away.
        let tapped = ctx.input(|i| i.pointer.any_pressed());
        if wanted != self.keyboard || (wanted.is_some() && tapped) {
            if wanted.is_some() != self.keyboard.is_some() {
                self.settle_until = Instant::now() + SETTLE;
            }
            self.keyboard = wanted;
            let shown = with_activity(&self.android, |env, activity| {
                let (show, mode) = (wanted.is_some(), wanted.unwrap_or(TEXT));
                env.call_method(activity, "showKeyboard", "(ZI)V", &[JValue::Bool(show.into()), JValue::Int(mode)])?;
                Ok(())
            });
            if let Err(e) = shown {
                log::warn!("could not show or hide the keyboard: {e}");
            }
        }
    }

    /// What egui copied, onto Android's clipboard too. (egui keeps its own
    /// copy, which a hardware keyboard's Ctrl+V pastes: winit gives that key
    /// to egui, not to the app.)
    fn copy_to_clipboard(&self, ctx: &egui::Context) {
        let copied: Vec<String> = ctx.output(|o| {
            o.commands
                .iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::CopyText(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        });
        for text in copied {
            let done = with_activity(&self.android, |env, activity| {
                let text = env.new_string(&text)?;
                env.call_method(activity, "setClipboard", "(Ljava/lang/String;)V", &[JValue::Object(&text)])?;
                Ok(())
            });
            if let Err(e) = done {
                log::warn!("could not copy to the clipboard: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// From the Java side: what the on-screen keyboard types.

/// Events from the keyboard, for the next frame.
static TYPED: Mutex<Vec<egui::Event>> = Mutex::new(Vec::new());
/// To wake the app up when the keyboard typed something.
static CTX: OnceLock<egui::Context> = OnceLock::new();

fn typed(events: impl IntoIterator<Item = egui::Event>) {
    TYPED.lock().unwrap_or_else(|e| e.into_inner()).extend(events);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

fn key_tap(key: egui::Key) -> [egui::Event; 2] {
    let ev = |pressed| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    [ev(true), ev(false)]
}

/// Text the keyboard settled on: a key, a word, a suggestion, pasted text.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_nasi_1lemak_malasdoh_MainActivity_nativeCommit(
    mut env: JNIEnv,
    _class: JClass,
    text: JString,
) {
    let Ok(text) = env.get_string(&text).map(String::from) else { return };
    match text.as_str() {
        // Enter, as text: the key, which text fields and terminals expect.
        "\n" | "\r" => typed(key_tap(egui::Key::Enter)),
        _ => typed([egui::Event::Ime(egui::ImeEvent::Commit(text))]),
    }
}

/// A word being typed or swiped, not settled yet (shown underlined).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_nasi_1lemak_malasdoh_MainActivity_nativeComposing(
    mut env: JNIEnv,
    _class: JClass,
    text: JString,
) {
    let Ok(text) = env.get_string(&text).map(String::from) else { return };
    typed([egui::Event::Ime(egui::ImeEvent::Preedit { text, active_range_chars: None })]);
}

/// A key the keyboard sent as a key (Enter, Backspace…), by Android key code.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_nasi_1lemak_malasdoh_MainActivity_nativeKey(
    _env: JNIEnv,
    _class: JClass,
    code: jint,
) {
    use egui::Key as K;
    let key = match code {
        66 => K::Enter,
        67 => K::Backspace,
        112 => K::Delete,
        61 => K::Tab,
        111 => K::Escape,
        19 => K::ArrowUp,
        20 => K::ArrowDown,
        21 => K::ArrowLeft,
        22 => K::ArrowRight,
        _ => return,
    };
    typed(key_tap(key));
}

// ---------------------------------------------------------------------------
// Calling Java.

/// Runs `f` with the activity (`MainActivity`) and a JNI environment for this
/// thread, freeing the local references it made and clearing any Java
/// exception it caused.
fn with_activity<T>(
    android: &AndroidApp,
    f: impl FnOnce(&mut JNIEnv, &JObject) -> Result<T, jni::errors::Error>,
) -> Result<T, jni::errors::Error> {
    // SAFETY: android-activity keeps the VM and the activity alive for as long
    // as the app runs; the activity is a global reference we only borrow.
    let vm = unsafe { jni::JavaVM::from_raw(android.vm_as_ptr().cast()) }?;
    let mut env = vm.attach_current_thread_permanently()?;
    let activity = unsafe { JObject::from_raw(android.activity_as_ptr().cast()) };
    let result = env.with_local_frame(16, |env| f(env, &activity));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result
}

/// The window's insets, from Java: `getWindow().getDecorView()
/// .getRootWindowInsets()`, for the system bars, the display cutout and the
/// keyboard (Android 11+), or the older system-window insets (Android 8-10).
fn insets(android: &AndroidApp) -> Result<Insets, jni::errors::Error> {
    with_activity(android, |env, activity| {
        let window = env.call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
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
    })
}
