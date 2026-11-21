use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use block2::RcBlock;
use malasdoh::app::App;
use objc2::DefinedClass as _;
use objc2::rc::Retained;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue, NSString, NSValue};
use objc2_ui_kit::{
    NSValueUIGeometryExtensions, UIDocumentPickerDelegate, UIKeyboardFrameEndUserInfoKey,
    UIKeyboardWillChangeFrameNotification, UIKeyboardWillHideNotification, UIPasteboard,
};

pub fn main() {
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

    malasdoh::dialogs::set_platform(malasdoh::dialogs::Platform {
        pick: Box::new(pick_files),
        // Downloads and saved files: Documents, which the Files app shows
        // (UIFileSharingEnabled in Info.plist).
        documents: home.join("Documents"),
    });
    malasdoh::share::set_sharer(share_file);
    malasdoh::clipboard::set_reader(|| {
        // SAFETY: UIPasteboard may be used from any thread.
        unsafe { UIPasteboard::generalPasteboard().string() }.map(|s| s.to_string())
    });
    watch_keyboard();

    let mut options = eframe::NativeOptions { renderer: eframe::Renderer::Wgpu, ..Default::default() };
    options.wgpu_options.wgpu_setup = wgpu_setup();
    // On iOS this only returns if the window could not be made at all.
    let result = eframe::run_native(
        "MalasDoh",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = App::with_context(&cc.egui_ctx, paths);
            app.set_touch(&cc.egui_ctx, true);
            Ok(Box::new(Mobile {
                app,
                focused: true,
                insets: egui::SafeAreaInsets::default(),
                keyboard_up: false,
                hardware_keyboard: false,
            }))
        }),
    );
    if let Err(e) = result {
        eprintln!("MalasDoh could not start: {e}");
    }
}

/// egui-wgpu asks for WebGPU's default limits, which the iOS Simulator's GPU
/// falls just short of (15 shader variables between stages, not 16), so
/// eframe gives up and the app quits. egui needs far less: ask for what the
/// GPU has.
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

struct Mobile {
    app: App,
    focused: bool,
    /// The notch and home-indicator insets, as egui-winit last gave them.
    insets: egui::SafeAreaInsets,
    /// Whether the keyboard is on screen, as last told to the app.
    keyboard_up: bool,
    /// Whether a hardware keyboard is attached, as last told to the app.
    hardware_keyboard: bool,
}

impl eframe::App for Mobile {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Keep the app above the on-screen keyboard, which iOS draws over it.
        if let Some(insets) = raw.safe_area_insets {
            self.insets = insets;
        }
        let height = raw.screen_rect.unwrap_or_else(|| ctx.viewport_rect()).height();
        let top = f32::from_bits(KEYBOARD_TOP.load(Ordering::Relaxed));
        let covered = if top.is_finite() { (height - top).max(0.0) } else { 0.0 };
        let mut insets = self.insets;
        insets.0.bottom = insets.0.bottom.max(covered);
        raw.safe_area_insets = Some(insets);
        let up = covered > 0.0;
        if up != self.keyboard_up {
            self.keyboard_up = up;
            self.app.set_keyboard_up(ctx, up);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // iOS may end a backgrounded app without warning, so save what is
        // open (and unsaved edits as drafts) whenever it is left.
        let ctx = ui.ctx().clone();
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if self.focused && !focused {
            self.app.shutdown();
        }
        if focused && !self.focused {
            // Back from the background, where iOS stops apps' connections:
            // connect again.
            self.app.resumed();
        }
        self.focused = focused;
        // A hardware keyboard (an iPad's, say) has the keys the terminal's
        // row would add.
        // SAFETY: a class property, read on the main thread.
        let attached = unsafe { objc2_game_controller::GCKeyboard::coalescedKeyboard() }.is_some();
        if attached != self.hardware_keyboard {
            self.hardware_keyboard = attached;
            self.app.set_hardware_keyboard(&ctx, attached);
        }
        self.app.frame_in_safe_area(ui);
        copy_to_clipboard(&ctx);
    }

    fn on_exit(&mut self) {
        self.app.shutdown();
    }
}

/// What egui copied, onto the iOS clipboard too. (egui keeps its own copy,
/// which a hardware keyboard's ⌘V pastes: winit gives that key to egui, not
/// to the app.)
fn copy_to_clipboard(ctx: &egui::Context) {
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
        // SAFETY: UIPasteboard may be used from any thread.
        unsafe { UIPasteboard::generalPasteboard().setString(Some(&NSString::from_str(&text))) };
    }
}

/// The top of the on-screen keyboard, in points from the top of the screen
/// (f32 bits); infinity while it is hidden.
static KEYBOARD_TOP: AtomicU32 = AtomicU32::new(0x7f80_0000);
/// To redraw when the keyboard moves.
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// Follow the keyboard as it shows, hides and changes size.
fn watch_keyboard() {
    let moved = RcBlock::new(|note: NonNull<NSNotification>| {
        // SAFETY: the notification centre passes a valid notification.
        let note = unsafe { note.as_ref() };
        let frame = note
            .userInfo()
            // SAFETY: a UIKit constant, valid for the whole run.
            .and_then(|info| info.objectForKey(unsafe { UIKeyboardFrameEndUserInfoKey }))
            .and_then(|v| v.downcast::<NSValue>().ok())
            // SAFETY: this key's value is an NSValue holding a CGRect.
            .map(|v: Retained<NSValue>| unsafe { v.CGRectValue() });
        if let Some(frame) = frame {
            KEYBOARD_TOP.store((frame.origin.y as f32).to_bits(), Ordering::Relaxed);
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
    });
    let hidden = RcBlock::new(|_note: NonNull<NSNotification>| {
        KEYBOARD_TOP.store(f32::INFINITY.to_bits(), Ordering::Relaxed);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
    });
    let center = NSNotificationCenter::defaultCenter();
    let queue = NSOperationQueue::mainQueue();
    // SAFETY: UIKit constants; the blocks only touch atomics and egui's
    // thread-safe context.
    unsafe {
        let a = center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillChangeFrameNotification),
            None,
            Some(&queue),
            &moved,
        );
        let b = center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillHideNotification),
            None,
            Some(&queue),
            &hidden,
        );
        // Observed for as long as the app runs.
        std::mem::forget((a, b));
    }
}

// ---------------------------------------------------------------------------
// The file picker.

/// What the picker was opened for.
struct PickerIvars {
    id: std::cell::RefCell<&'static str>,
}

objc2::define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Picker does not
    // implement Drop.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "MalasDohPicker"]
    #[ivars = PickerIvars]
    struct Picker;

    unsafe impl NSObjectProtocol for Picker {}

    unsafe impl UIDocumentPickerDelegate for Picker {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(
            &self,
            _picker: &objc2_ui_kit::UIDocumentPickerViewController,
            urls: &objc2_foundation::NSArray<objc2_foundation::NSURL>,
        ) {
            // Copies (asCopy), so plain files the app may read.
            let paths = urls.iter().filter_map(|u| u.path()).map(|p| std::path::PathBuf::from(p.to_string())).collect();
            picked(*self.ivars().id.borrow(), paths);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn cancelled(&self, _picker: &objc2_ui_kit::UIDocumentPickerViewController) {
            picked(*self.ivars().id.borrow(), Vec::new());
        }
    }
);

thread_local! {
    /// The picker's delegate, kept alive while it is open (UIKit holds it weakly).
    static DELEGATE: std::cell::RefCell<Option<Retained<Picker>>> = const { std::cell::RefCell::new(None) };
}

fn picked(id: &'static str, paths: Vec<std::path::PathBuf>) {
    malasdoh::dialogs::deliver(id, paths);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Opens UIKit's document picker over the app (called on the main thread).
fn pick_files(id: &'static str, multiple: bool) {
    use objc2::{MainThreadOnly as _, msg_send};
    use objc2_ui_kit::{UIApplication, UIDocumentPickerViewController};

    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let delegate: Retained<Picker> = {
        let this = Picker::alloc(mtm).set_ivars(PickerIvars { id: std::cell::RefCell::new(id) });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    };
    // SAFETY: a UIKit constant, valid for the whole run.
    let types = objc2_foundation::NSArray::from_slice(&[unsafe { objc2_uniform_type_identifiers::UTTypeItem }]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
        true,
    );
    picker.setAllowsMultipleSelection(multiple);
    picker.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*delegate)));
    #[allow(deprecated)] // One window: the key window is the app's.
    let root = UIApplication::sharedApplication(mtm).keyWindow().and_then(|w| w.rootViewController());
    let Some(root) = root else {
        picked(id, Vec::new());
        return;
    };
    root.presentViewController_animated_completion(&picker, true, None);
    DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));
}

/// UIKit's share sheet for a file (called on the main thread).
fn share_file(path: &std::path::Path) -> Result<(), String> {
    use objc2::MainThreadOnly as _;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_ui_kit::{UIActivityViewController, UIApplication};

    let mtm = objc2::MainThreadMarker::new().ok_or("not on the main thread")?;
    #[allow(deprecated)] // One window: the key window is the app's.
    let root = UIApplication::sharedApplication(mtm)
        .keyWindow()
        .and_then(|w| w.rootViewController())
        .ok_or("no window to show it over")?;
    let url = objc2_foundation::NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let items = objc2_foundation::NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(url))]);
    // SAFETY: an array of one file URL, which the share sheet accepts.
    let sheet = unsafe {
        UIActivityViewController::initWithActivityItems_applicationActivities(
            UIActivityViewController::alloc(mtm),
            &items,
            None,
        )
    };
    // On an iPad it opens as a popover, which needs somewhere to point to.
    if let Some(popover) = sheet.popoverPresentationController()
        && let Some(view) = root.view()
    {
        let b = view.bounds();
        popover.setSourceView(Some(&view));
        popover.setSourceRect(CGRect {
            origin: CGPoint { x: b.size.width / 2.0, y: b.size.height / 2.0 },
            size: CGSize { width: 1.0, height: 1.0 },
        });
    }
    root.presentViewController_animated_completion(&sheet, true, None);
    Ok(())
}
