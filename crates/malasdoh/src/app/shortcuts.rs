//! App-wide keys: switching tabs, new and closing terminals, a new
//! connection, and finding one. Those with Ctrl+Shift work everywhere; plain
//! Ctrl ones (Ctrl+N, Ctrl+K) only while you are not typing in a terminal or
//! a text box, where the shell or the box has a use for them.

use egui::{Key, Modifiers};
use malasdoh_core::profile::Profile;

use super::{App, ProfileForm};
use crate::session::ConnState;

/// The connection list's search box.
pub const SEARCH_ID: &str = "connection-search";

impl App {
    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if crate::shortcut::blocked(ctx) {
            return;
        }
        let typing = crate::shortcut::typing(ctx);
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let (next_tab, prev_tab, next_pane, prev_pane, new_term, close, find, new_conn, help) = ctx.input_mut(|i| {
            let prev_tab = crate::shortcut::consume(i, ctrl_shift, Key::Tab);
            let next_tab = crate::shortcut::consume(i, Modifiers::CTRL, Key::Tab);
            let prev_pane = crate::shortcut::consume(i, Modifiers::CTRL, Key::PageUp);
            let next_pane = crate::shortcut::consume(i, Modifiers::CTRL, Key::PageDown);
            let new_term = crate::shortcut::consume(i, Modifiers::COMMAND | Modifiers::SHIFT, Key::T);
            let close = crate::shortcut::consume(i, Modifiers::COMMAND | Modifiers::SHIFT, Key::W);
            let find = crate::shortcut::consume(i, Modifiers::COMMAND | Modifiers::SHIFT, Key::P)
                || (!typing && crate::shortcut::consume(i, Modifiers::COMMAND, Key::K));
            let new_conn = !typing && crate::shortcut::consume(i, Modifiers::COMMAND, Key::N);
            // F1 means help to programs in the terminal too.
            let help = !typing && i.consume_key(Modifiers::NONE, Key::F1);
            (next_tab, prev_tab, next_pane, prev_pane, new_term, close, find, new_conn, help)
        });
        if help {
            self.show_keys_help = !self.show_keys_help;
        }
        if next_tab || prev_tab {
            self.cycle_session(if next_tab { 1 } else { -1 });
        }
        if new_conn {
            self.form = Some(ProfileForm::new(Profile::default(), true));
        }
        if find {
            self.show_sidebar = true;
            crate::shortcut::focus_select(ctx, egui::Id::new(SEARCH_ID), self.filter.chars().count());
        }
        let Some(s) = self.active.and_then(|id| self.sessions.iter_mut().find(|s| s.id == id)) else { return };
        if next_pane || prev_pane {
            s.cycle_pane(if next_pane { 1 } else { -1 });
        }
        if new_term && s.state == ConnState::Connected {
            s.open_terminal(&self.shared, &self.settings);
        }
        if close {
            s.close_active();
        }
    }

    /// Show the next (or previous) connection tab.
    fn cycle_session(&mut self, step: isize) {
        let n = self.sessions.len() as isize;
        if n == 0 {
            return;
        }
        let here = self.active.and_then(|id| self.sessions.iter().position(|s| s.id == id)).unwrap_or(0) as isize;
        let s = &mut self.sessions[(here + step).rem_euclid(n) as usize];
        self.active = Some(s.id);
        s.focus_active();
    }
}

/// Every shortcut, by where it works.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Anywhere",
        &[
            ("Ctrl+Tab / Ctrl+Shift+Tab", "Next / previous connection"),
            ("Ctrl+PgDn / Ctrl+PgUp", "Next / previous terminal or file tab"),
            ("Ctrl+Shift+T", "New terminal"),
            ("Ctrl+Shift+W", "Close the terminal or file tab (not with unsaved changes)"),
            ("Ctrl+Shift+P", "Find a connection"),
            ("Ctrl+N, Ctrl+K", "New connection, find a connection (not while typing in a terminal or box)"),
            ("F1", "This list (not while in a terminal)"),
        ],
    ),
    (
        "Dialogs and windows",
        &[
            ("Enter", "The main button (OK, Save, Rename, Delete…)"),
            ("Ctrl+Enter", "The main button where Enter types a new line"),
            ("Esc", "Cancel, or close the window"),
        ],
    ),
    (
        "Connection list (after a click, or Down from the search box)",
        &[
            ("↑ ↓, letters", "Move; jump to a name"),
            ("Enter", "Connect"),
            ("F2", "Edit"),
            ("Delete", "Delete (asks first)"),
            ("Shift+F10", "Its menu"),
            ("Enter in the search box", "Connect to the first match"),
        ],
    ),
    (
        "File lists (after a click)",
        &[
            ("↑ ↓ Home End PgUp PgDn", "Move (Shift extends)"),
            ("Letters", "Jump to a name"),
            ("Ctrl+A, Esc", "Select all, select none"),
            ("Enter", "Open"),
            ("F2, Delete", "Rename, delete"),
            ("Ctrl+C", "Copy the paths"),
            ("Ctrl+Shift+N", "New folder"),
            ("Backspace, Alt+↑", "Folder above"),
            ("Alt+← / Alt+→", "Back / forward"),
            ("F5", "Refresh"),
            ("Ctrl+L, Ctrl+F", "The path box, the filter box"),
            ("Shift+F10", "Its menu"),
        ],
    ),
    (
        "Other lists",
        &[
            ("Transfers: Delete, Space", "Cancel, pause or continue the selected"),
            ("Hosts to run on: Space, Ctrl+A", "Tick, tick all"),
            ("Find files results: Enter", "Open in the editor"),
        ],
    ),
    ("Editor", &[("Ctrl+S", "Save"), ("Ctrl+F, Ctrl+H, F3", "Find, replace, find next"), ("Ctrl+G", "Go to line")]),
    ("Terminal", &[("Ctrl+Shift+F", "Search the scrollback")]),
];

impl App {
    pub(super) fn keys_help_ui(&mut self, ctx: &egui::Context) {
        if !self.show_keys_help {
            return;
        }
        let mut open = true;
        let shown = egui::Window::new("Keyboard shortcuts").open(&mut open).default_width(560.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (place, keys) in HELP {
                    ui.label(egui::RichText::new(*place).strong());
                    egui::Grid::new(("keys-help", *place)).num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                        for (k, what) in *keys {
                            ui.monospace(*k);
                            ui.label(*what);
                            ui.end_row();
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.show_keys_help = open;
    }
}
