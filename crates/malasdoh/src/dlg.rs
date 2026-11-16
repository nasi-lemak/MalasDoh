//! Keys for dialogs: Enter presses the main button, Escape cancels. A key
//! that does something is used up, so one Escape cancels a form inside a
//! window without also closing the window.

use egui::{Key, Modifiers};

/// Enter (or Ctrl+Enter) for the dialog's main button. Not while a
/// drop-down is open, where Enter picks an item. With `multiline`, the
/// dialog has a multi-line text box where Enter types a new line, so only
/// Ctrl+Enter counts.
pub fn enter(ui: &egui::Ui, multiline: bool) -> bool {
    !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| {
            i.consume_key(Modifiers::COMMAND, Key::Enter) || (!multiline && i.consume_key(Modifiers::NONE, Key::Enter))
        })
}

/// Escape to cancel; not while a drop-down is open (Escape closes that).
pub fn escape(ui: &egui::Ui) -> bool {
    escape_ctx(ui.ctx())
}

fn escape_ctx(ctx: &egui::Context) -> bool {
    !egui::Popup::is_any_open(ctx) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
}

/// Is the user working in this window (`layer`): its text box has the
/// keyboard, or nothing has and the pointer is over it (or over no other
/// window, and it is the top one)? Keeps a window's
/// Enter from firing while typing in the terminal behind it.
fn ours(ctx: &egui::Context, layer: egui::LayerId) -> bool {
    let focused = ctx.memory(|m| m.focused());
    let inside = focused.is_some_and(|id| ctx.read_response(id).is_some_and(|r| r.layer_id == layer));
    // Enter and Escape take the focus away from a text box before this
    // runs, so a box in this window that had it last frame counts too.
    let key = egui::Id::new(("dlg-focus-inside", layer));
    let pass = ctx.cumulative_pass_nr();
    let (seen, now, before): (u64, bool, bool) = ctx.data(|d| d.get_temp(key)).unwrap_or_default();
    let (now, before) = if seen == pass { (now || inside, before) } else { (inside, now) };
    ctx.data_mut(|d| d.insert_temp(key, (pass, now, before)));
    // With nothing holding the keyboard: the window under the pointer, or
    // else the one on top (the last opened or clicked).
    now || before
        || (focused.is_none()
            && match ctx.pointer_hover_pos().and_then(|p| ctx.layer_id_at(p)) {
                Some(l) if l.order == egui::Order::Middle => l == layer,
                _ => ctx.memory(|m| m.areas().top_layer_id(egui::Order::Middle)) == Some(layer),
            })
}

/// [`enter`] for a window (not a modal dialog): only when it is the one
/// being used.
pub fn window_enter(ui: &egui::Ui, multiline: bool) -> bool {
    ours(ui.ctx(), ui.layer_id()) && enter(ui, multiline)
}

/// [`escape`] for a window.
pub fn window_escape(ui: &egui::Ui) -> bool {
    ours(ui.ctx(), ui.layer_id()) && escape(ui)
}

/// After a window was shown: should Escape close it? Only when it is the
/// window being used, and nothing inside it took the Escape already (a
/// form it cancelled, a menu it closed).
pub fn closes<R>(ctx: &egui::Context, shown: &Option<egui::InnerResponse<R>>) -> bool {
    shown.as_ref().is_some_and(|s| ours(ctx, s.response.layer_id)) && escape_ctx(ctx)
}
