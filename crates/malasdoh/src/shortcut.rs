//! Keyboard helpers shared by the app's windows and panes.

use egui::text::{CCursor, CCursorRange};

/// Put the keyboard in a text box and select what is in it (`len`
/// characters), so typing replaces it.
pub fn focus_select(ctx: &egui::Context, id: egui::Id, len: usize) {
    ctx.memory_mut(|m| m.request_focus(id));
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(len))));
    state.store(ctx, id);
}

/// Is the keyboard in a text box, the terminal or another widget? Plain
/// Ctrl+letter shortcuts then belong to it (Ctrl+K and Ctrl+N mean
/// something in a shell).
pub fn typing(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.focused().is_some())
}

/// A dialog is up, or a menu is open: app-wide shortcuts wait.
pub fn blocked(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.top_modal_layer().is_some()) || egui::Popup::is_any_open(ctx)
}

/// Like `consume_key`, but Shift and Alt must be exactly as given: Ctrl+N
/// should not also take Ctrl+Shift+N.
pub fn consume(i: &mut egui::InputState, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    let before = i.events.len();
    i.events.retain(|e| {
        !matches!(e, egui::Event::Key { key: k, pressed: true, modifiers: m, .. }
            if *k == key && m.matches_exact(modifiers))
    });
    i.events.len() != before
}
