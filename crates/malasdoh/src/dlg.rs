//! Keys for dialogs: Enter presses the main button, Escape cancels.

use egui::Key;

/// Enter (or Ctrl+Enter) for the dialog's main button. Not while a
/// drop-down is open, where Enter picks an item. With `multiline`, the
/// dialog has a multi-line text box where Enter types a new line, so only
/// Ctrl+Enter counts.
pub fn enter(ui: &egui::Ui, multiline: bool) -> bool {
    !ui.ctx().any_popup_open() && ui.input(|i| i.key_pressed(Key::Enter) && (!multiline || i.modifiers.command))
}

/// Escape to cancel; not while a drop-down is open (Escape closes that).
pub fn escape(ui: &egui::Ui) -> bool {
    !ui.ctx().any_popup_open() && ui.input(|i| i.key_pressed(Key::Escape))
}

/// Is the user working in this window: its text box has the keyboard, or
/// nothing has and the pointer is over it? Keeps a window's Enter from
/// firing while typing in the terminal behind it.
fn ours(ui: &egui::Ui) -> bool {
    let ctx = ui.ctx();
    let layer = ui.layer_id();
    let focused = ctx.memory(|m| m.focused());
    let inside = focused.is_some_and(|id| ctx.read_response(id).is_some_and(|r| r.layer_id == layer));
    // Enter and Escape take the focus away from a text box before this
    // runs, so a box in this window that had it last frame counts too.
    let key = egui::Id::new(("dlg-focus-inside", layer));
    let pass = ctx.cumulative_pass_nr();
    let (seen, now, before): (u64, bool, bool) = ctx.data(|d| d.get_temp(key)).unwrap_or_default();
    let (now, before) = if seen == pass { (now || inside, before) } else { (inside, now) };
    ctx.data_mut(|d| d.insert_temp(key, (pass, now, before)));
    now || before || (focused.is_none() && ctx.pointer_hover_pos().is_some_and(|p| ctx.layer_id_at(p) == Some(layer)))
}

/// [`enter`] for a window (not a modal dialog): only when it is the one
/// being used.
pub fn window_enter(ui: &egui::Ui, multiline: bool) -> bool {
    ours(ui) && enter(ui, multiline)
}

/// [`escape`] for a window.
pub fn window_escape(ui: &egui::Ui) -> bool {
    ours(ui) && escape(ui)
}
