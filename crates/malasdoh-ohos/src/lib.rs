//! MalasDoh on HarmonyOS: the native module (`libmalasdoh_ohos.so`) an
//! ArkTS app loads into an XComponent. It runs the same app as the desktop
//! program (the `malasdoh` library) and gives it what eframe gives it there:
//! an OpenGL ES surface (EGL on the XComponent's native window), frames,
//! and input from touch, mouse, a hardware keyboard and the on-screen
//! keyboard.
//!
//! The ArkTS side is in `ohos/` at the top of the repository.

#![cfg(target_env = "ohos")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Event as Input, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
use khronos_egl as egl;
use openharmony_ability::ime::Direction;
use openharmony_ability::xcomponent::{Action, KeyCode, MouseAction, MouseButton, TouchEvent};
use openharmony_ability::{
    ArkUiInputEvent, Event, ImeEvent, InputEvent, KeyboardInputDelivery, OpenHarmonyApp, XComponentInputEvent,
};
use openharmony_ability_derive::ability;

type Egl = egl::Instance<egl::Static>;

// khronos-egl's "static" EGL, linked here (its pkg-config lookup does not
// work when cross-compiling).
#[link(name = "EGL")]
unsafe extern "C" {}

/// The entry point: HarmonyOS calls this once, when the ability starts.
#[ability]
fn openharmony_app(app: OpenHarmonyApp) {
    // Physical keys with their characters and modifiers (API 14+).
    if let Err(e) = app.set_keyboard_input_delivery(KeyboardInputDelivery::ArkUi) {
        tracing::warn!("keyboard input with characters is not available: {e}");
    }
    let handle = app.clone();
    let mut state = State::new();
    app.run_loop(move |event| state.handle(&handle, event));
}

/// OpenGL ES on the XComponent's window, with egui's painter.
struct Surface {
    egl: Egl,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,
    painter: egui_glow::Painter,
}

impl Surface {
    fn new(window: *mut std::ffi::c_void) -> Result<Self, String> {
        let egl = egl::Instance::new(egl::Static);
        let e = |what: &str, err: egl::Error| format!("{what}: {err}");
        // SAFETY: the default display is always a valid argument.
        let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }.ok_or("no EGL display")?;
        egl.initialize(display).map_err(|x| e("EGL initialize", x))?;
        let attrs = [
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::NONE,
        ];
        let config =
            egl.choose_first_config(display, &attrs).map_err(|x| e("EGL config", x))?.ok_or("no EGL config")?;
        egl.bind_api(egl::OPENGL_ES_API).map_err(|x| e("EGL bind API", x))?;
        let context = egl
            .create_context(display, config, None, &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE])
            .map_err(|x| e("EGL context", x))?;
        // SAFETY: `window` is the OHNativeWindow HarmonyOS gave this surface,
        // alive until SurfaceDestroy, when this is dropped.
        let surface = unsafe { egl.create_window_surface(display, config, window as egl::NativeWindowType, None) }
            .map_err(|x| e("EGL window surface", x))?;
        egl.make_current(display, Some(surface), Some(surface), Some(context)).map_err(|x| e("EGL make current", x))?;
        // SAFETY: the context is current on this thread, and the functions
        // come from its EGL.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
            })
        };
        let painter = egui_glow::Painter::new(Arc::new(gl), "", None, false).map_err(|x| x.to_string())?;
        Ok(Self { egl, display, context, surface, painter })
    }

    /// Its size in pixels.
    fn size(&self) -> [u32; 2] {
        let q = |what| self.egl.query_surface(self.display, self.surface, what).unwrap_or(0).max(1) as u32;
        [q(egl::WIDTH), q(egl::HEIGHT)]
    }

    fn paint(
        &mut self,
        ppp: f32,
        clear: egui::Color32,
        shapes: &[egui::ClippedPrimitive],
        mut tex: egui::TexturesDelta,
    ) {
        if self.egl.make_current(self.display, Some(self.surface), Some(self.surface), Some(self.context)).is_err() {
            return;
        }
        let size = self.size();
        self.painter.clear(size, egui::Rgba::from(clear).to_array());
        self.painter.paint_and_update_textures(size, ppp, shapes, &mut tex);
        if let Err(e) = self.egl.swap_buffers(self.display, self.surface) {
            tracing::warn!("swap buffers: {e}");
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.painter.destroy();
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
    }
}

struct State {
    ctx: egui::Context,
    app: Option<malasdoh::app::App>,
    surface: Option<Surface>,
    /// Input since the last frame.
    input: Vec<Input>,
    modifiers: Modifiers,
    started: Instant,
    /// When egui next wants a frame (set from any thread).
    wake: Arc<Mutex<Option<Instant>>>,
    keyboard_shown: bool,
}

impl State {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let wake = Arc::new(Mutex::new(Some(Instant::now())));
        let w = wake.clone();
        ctx.set_request_repaint_callback(move |info| {
            let at = Instant::now() + info.delay;
            let mut slot = w.lock().unwrap();
            if slot.is_none_or(|t| at < t) {
                *slot = Some(at);
            }
        });
        Self {
            ctx,
            app: None,
            surface: None,
            input: Vec::new(),
            modifiers: Modifiers::NONE,
            started: Instant::now(),
            wake,
            keyboard_shown: false,
        }
    }

    fn handle(&mut self, ohos: &OpenHarmonyApp, event: Event) {
        match event {
            Event::SurfaceCreate => {
                match ohos.native_window() {
                    Some(window) => match Surface::new(window.raw()) {
                        Ok(s) => self.surface = Some(s),
                        Err(e) => tracing::error!("cannot draw: {e}"),
                    },
                    None => tracing::error!("surface created without a native window"),
                }
                if self.app.is_none() {
                    self.app = Some(malasdoh::app::App::with_context(&self.ctx, paths(ohos)));
                }
                self.repaint_now();
            }
            Event::SurfaceDestroy => self.surface = None,
            Event::WindowRedraw(_) => self.frame(ohos),
            Event::WindowResize { .. } | Event::ContentRectChange(_) | Event::GainedFocus | Event::Resume(_) => {
                self.repaint_now()
            }
            Event::LostFocus => {
                self.input.push(Input::WindowFocused(false));
                self.repaint_now();
            }
            Event::Pause | Event::Stop | Event::SaveState(_) | Event::Destroy => {
                if let Some(app) = &mut self.app {
                    app.shutdown();
                }
            }
            Event::Input(input) => {
                self.input(ohos, input);
                self.repaint_now();
            }
            _ => {}
        }
    }

    fn repaint_now(&self) {
        *self.wake.lock().unwrap() = Some(Instant::now());
    }

    /// One frame, if egui asked for one.
    fn frame(&mut self, ohos: &OpenHarmonyApp) {
        let due = self.wake.lock().unwrap().is_some_and(|t| t <= Instant::now());
        if !due || self.app.is_none() || self.surface.is_none() {
            return;
        }
        *self.wake.lock().unwrap() = None;
        let ppp = scale(ohos);
        let size = self.surface.as_ref().map_or([1, 1], Surface::size);
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(size[0] as f32, size[1] as f32) / ppp)),
            time: Some(self.started.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.input),
            focused: true,
            ..Default::default()
        };
        self.ctx.set_pixels_per_point(ppp);
        let app = self.app.as_mut().unwrap();
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui));
        // The on-screen keyboard while a text box or the terminal has the keyboard.
        let wants_keys = self.ctx.memory(|m| m.focused().is_some());
        if wants_keys != self.keyboard_shown {
            if wants_keys {
                ohos.show_keyboard()
            } else {
                ohos.hide_keyboard()
            }
            self.keyboard_shown = wants_keys;
        }
        let shapes = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let clear = self.ctx.global_style().visuals.panel_fill;
        if let Some(s) = &mut self.surface {
            s.paint(out.pixels_per_point, clear, &shapes, out.textures_delta);
        }
        if let Some(v) = out.viewport_output.get(&egui::ViewportId::ROOT)
            && v.repaint_delay < Duration::from_secs(3600)
        {
            let at = Instant::now() + v.repaint_delay;
            let mut slot = self.wake.lock().unwrap();
            if slot.is_none_or(|t| at < t) {
                *slot = Some(at);
            }
        }
    }

    fn input(&mut self, ohos: &OpenHarmonyApp, input: InputEvent) {
        let ppp = scale(ohos);
        let at = |x: f32, y: f32| Pos2::new(x / ppp, y / ppp);
        match input {
            InputEvent::XComponent(XComponentInputEvent::Touch(t)) => {
                let pos = at(t.x, t.y);
                let press = |pressed| Input::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: self.modifiers,
                };
                match t.event_type {
                    TouchEvent::Down => {
                        self.input.push(Input::PointerMoved(pos));
                        self.input.push(press(true));
                    }
                    TouchEvent::Move => self.input.push(Input::PointerMoved(pos)),
                    TouchEvent::Up => {
                        self.input.push(press(false));
                        self.input.push(Input::PointerGone);
                    }
                    TouchEvent::Cancel => self.input.push(Input::PointerGone),
                    TouchEvent::Unknown => {}
                }
            }
            InputEvent::XComponent(XComponentInputEvent::Mouse(m)) => {
                let pos = at(m.x, m.y);
                let button = match m.button {
                    MouseButton::LeftButton => Some(PointerButton::Primary),
                    MouseButton::RightButton => Some(PointerButton::Secondary),
                    MouseButton::MiddleButton => Some(PointerButton::Middle),
                    MouseButton::BackButton => Some(PointerButton::Extra1),
                    MouseButton::ForwardButton => Some(PointerButton::Extra2),
                    MouseButton::NoneButton => None,
                };
                self.input.push(Input::PointerMoved(pos));
                let pressed = match m.action {
                    MouseAction::Press => Some(true),
                    MouseAction::Release => Some(false),
                    _ => None,
                };
                if let (Some(button), Some(pressed)) = (button, pressed) {
                    self.input.push(Input::PointerButton { pos, button, pressed, modifiers: self.modifiers });
                }
            }
            InputEvent::XComponent(XComponentInputEvent::Hover(false)) => self.input.push(Input::PointerGone),
            InputEvent::ArkUi(ArkUiInputEvent::Axis(a)) => {
                // Wheel and touchpad scrolling. The sign may need flipping once
                // tried on a device: egui scrolls content down for a positive y.
                self.input.push(Input::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(-a.delta_x as f32, -a.delta_y as f32) / ppp,
                    phase: egui::TouchPhase::Move,
                    modifiers: self.modifiers,
                });
            }
            InputEvent::ArkUi(ArkUiInputEvent::Key(k)) => {
                let pressed = k.action == Action::Down;
                if let Some(held) = &k.pressed_keys {
                    let has = |a: KeyCode, b: KeyCode| held.contains(&a) || held.contains(&b);
                    let ctrl = has(KeyCode::CtrlLeft, KeyCode::CtrlRight);
                    self.modifiers = Modifiers {
                        alt: has(KeyCode::AltLeft, KeyCode::AltRight),
                        ctrl,
                        shift: has(KeyCode::ShiftLeft, KeyCode::ShiftRight),
                        mac_cmd: false,
                        command: ctrl,
                    };
                }
                // A key the on-screen keyboard handled arrives as its text instead.
                if k.ime_consumed {
                    return;
                }
                if let Some(key) = key(k.code) {
                    self.input.push(Input::Key {
                        key,
                        physical_key: Some(key),
                        pressed,
                        repeat: false,
                        modifiers: self.modifiers,
                    });
                }
                let ch = char::from_u32(k.unicode).filter(|c| !c.is_control());
                if pressed
                    && let Some(c) = ch
                    && !self.modifiers.ctrl
                    && !self.modifiers.alt
                {
                    self.input.push(Input::Text(c.to_string()));
                }
            }
            InputEvent::Ime(ime) => self.ime(ime),
            _ => {}
        }
    }

    /// The on-screen keyboard: committed text, its edit keys, and text
    /// still being composed (pinyin and the like).
    fn ime(&mut self, ime: ImeEvent) {
        let tap = |input: &mut Vec<Input>, key: Key| {
            for pressed in [true, false] {
                input.push(Input::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE });
            }
        };
        match ime {
            ImeEvent::TextInputEvent(t) if t.text == "\n" => tap(&mut self.input, Key::Enter),
            ImeEvent::TextInputEvent(t) => self.input.push(Input::Text(t.text)),
            ImeEvent::BackspaceEvent(n) => (0..n.max(1)).for_each(|_| tap(&mut self.input, Key::Backspace)),
            ImeEvent::DeleteForwardEvent(n) => (0..n.max(1)).for_each(|_| tap(&mut self.input, Key::Delete)),
            ImeEvent::EnterEvent(_) => tap(&mut self.input, Key::Enter),
            ImeEvent::MoveCursorEvent(d) => {
                let key = match d {
                    Direction::Up => Key::ArrowUp,
                    Direction::Down => Key::ArrowDown,
                    Direction::Left => Key::ArrowLeft,
                    Direction::Right => Key::ArrowRight,
                    Direction::None => return,
                };
                tap(&mut self.input, key);
            }
            ImeEvent::PreviewTextEvent { text, .. } => {
                self.input.push(Input::Ime(egui::ImeEvent::Preedit { text, active_range_chars: None }));
            }
            ImeEvent::FinishPreviewEvent => {
                self.input.push(Input::Ime(egui::ImeEvent::Preedit { text: String::new(), active_range_chars: None }))
            }
            _ => {}
        }
    }
}

/// Points to pixels: the display's density.
fn scale(ohos: &OpenHarmonyApp) -> f32 {
    let s = ohos.scale();
    if s.is_finite() && s > 0.0 { s } else { 1.0 }
}

/// Where MalasDoh keeps its settings, connections and backups: the app's
/// own folder in its sandbox.
fn paths(ohos: &OpenHarmonyApp) -> malasdoh_core::AppPaths {
    let base = ohos.pref_path().or_else(|| ohos.base_path()).unwrap_or_else(|| "/data/storage/el2/base".into());
    malasdoh_core::AppPaths::rooted(std::path::Path::new(&base).join("malasdoh"))
}

/// A HarmonyOS key as egui's.
fn key(code: KeyCode) -> Option<Key> {
    use KeyCode as K;
    Some(match code {
        K::A => Key::A,
        K::B => Key::B,
        K::C => Key::C,
        K::D => Key::D,
        K::E => Key::E,
        K::F => Key::F,
        K::G => Key::G,
        K::H => Key::H,
        K::I => Key::I,
        K::J => Key::J,
        K::K => Key::K,
        K::L => Key::L,
        K::M => Key::M,
        K::N => Key::N,
        K::O => Key::O,
        K::P => Key::P,
        K::Q => Key::Q,
        K::R => Key::R,
        K::S => Key::S,
        K::T => Key::T,
        K::U => Key::U,
        K::V => Key::V,
        K::W => Key::W,
        K::X => Key::X,
        K::Y => Key::Y,
        K::Z => Key::Z,
        K::Key0 | K::Numpad0 => Key::Num0,
        K::Key1 | K::Numpad1 => Key::Num1,
        K::Key2 | K::Numpad2 => Key::Num2,
        K::Key3 | K::Numpad3 => Key::Num3,
        K::Key4 | K::Numpad4 => Key::Num4,
        K::Key5 | K::Numpad5 => Key::Num5,
        K::Key6 | K::Numpad6 => Key::Num6,
        K::Key7 | K::Numpad7 => Key::Num7,
        K::Key8 | K::Numpad8 => Key::Num8,
        K::Key9 | K::Numpad9 => Key::Num9,
        K::DpadUp => Key::ArrowUp,
        K::DpadDown => Key::ArrowDown,
        K::DpadLeft => Key::ArrowLeft,
        K::DpadRight => Key::ArrowRight,
        K::Enter | K::NumpadEnter => Key::Enter,
        K::Tab => Key::Tab,
        K::Space => Key::Space,
        K::Del => Key::Backspace,
        K::ForwardDel => Key::Delete,
        K::Escape => Key::Escape,
        K::Insert => Key::Insert,
        K::MoveHome => Key::Home,
        K::MoveEnd => Key::End,
        K::PageUp => Key::PageUp,
        K::PageDown => Key::PageDown,
        K::Minus | K::NumpadSubtract => Key::Minus,
        K::Equals | K::NumpadEquals => Key::Equals,
        K::Comma | K::NumpadComma => Key::Comma,
        K::Period | K::NumpadDot => Key::Period,
        K::Slash | K::NumpadDivide => Key::Slash,
        K::Backslash => Key::Backslash,
        K::Semicolon => Key::Semicolon,
        K::Apostrophe => Key::Quote,
        K::Grave => Key::Backtick,
        K::LeftBracket => Key::OpenBracket,
        K::RightBracket => Key::CloseBracket,
        K::F1 => Key::F1,
        K::F2 => Key::F2,
        K::F3 => Key::F3,
        K::F4 => Key::F4,
        K::F5 => Key::F5,
        K::F6 => Key::F6,
        K::F7 => Key::F7,
        K::F8 => Key::F8,
        K::F9 => Key::F9,
        K::F10 => Key::F10,
        K::F11 => Key::F11,
        K::F12 => Key::F12,
        K::Copy => Key::Copy,
        K::Cut => Key::Cut,
        K::Paste => Key::Paste,
        _ => return None,
    })
}
