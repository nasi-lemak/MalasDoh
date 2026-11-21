//! A connection on a phone: one row over whatever is shown, so the terminal
//! keeps the screen. The row has a pane picker (terminals, open files,
//! logs), Files and This device, a new terminal, and a "…" menu with the
//! rest.

use egui::RichText;
use std::time::Instant;

use crate::session::{CompactView as V, ConnState, Pane, Session, Shared};
use crate::settings::Settings;
use crate::snippets::{self, Snippet, SnippetStore, Use};

/// The row. Returns a snippet picked from the menu.
pub(super) fn bar(
    ui: &mut egui::Ui,
    s: &mut Session,
    shared: &Shared,
    settings: &Settings,
    snippet_store: &SnippetStore,
    last_local_dir: &mut Option<std::path::PathBuf>,
) -> Option<(Snippet, Use)> {
    let mut chosen = None;
    let panes = pane_list(s);
    let current = panes.iter().find(|(p, _)| Some(*p) == s.active).map(|(_, t)| t.clone());
    ui.horizontal(|ui| {
        // What is shown: a terminal, a file or a log; picking one shows it.
        let label = match (&current, s.compact_view) {
            (Some(t), V::Panes) => t.clone(),
            _ => "⌨ Terminals".to_string(),
        };
        let shown = s.compact_view == V::Panes;
        let picker = egui::ComboBox::from_id_salt(("pane", s.id))
            .selected_text(if shown { RichText::new(label).strong() } else { RichText::new(label) })
            .width(150.0)
            .show_ui(ui, |ui| {
                for (p, title) in &panes {
                    if ui.selectable_label(shown && s.active == Some(*p), title).clicked() {
                        s.active = Some(*p);
                        s.compact_view = V::Panes;
                    }
                }
                if panes.is_empty() {
                    ui.label(RichText::new("Nothing open yet").weak());
                }
            });
        // A tap on the picker while files are shown goes back to the panes.
        if picker.response.clicked() && !panes.is_empty() && !shown {
            s.compact_view = V::Panes;
        }
        ui.selectable_value(&mut s.compact_view, V::Files, "📁").on_hover_text("The server's files");
        if s.local.is_some() {
            ui.selectable_value(&mut s.compact_view, V::Local, "🖥").on_hover_text("This device's files");
        }
        if s.state == ConnState::Connected && ui.button("+").on_hover_text("New terminal").clicked() {
            s.open_terminal(shared, settings);
            s.compact_view = V::Panes;
        }
        ui.menu_button("…", |ui| {
            menu(ui, s, shared, settings, snippet_store, last_local_dir, &mut chosen);
        });
    });
    chosen
}

/// Terminals, then logs, then open files, with their titles.
fn pane_list(s: &Session) -> Vec<(Pane, String)> {
    let mut v = Vec::new();
    for (n, t) in s.terms.iter().enumerate() {
        let icon = if t.lasting.is_some() { "⟳" } else { "⌨" };
        v.push((Pane::Term(t.id), format!("{icon} Terminal {}", n + 1)));
    }
    for l in &s.logs {
        v.push((Pane::Log(l.id), l.title()));
    }
    for e in &s.editors {
        let dot = if e.is_modified() { " •" } else { "" };
        v.push((Pane::Editor(e.id), format!("📝 {}{dot}", e.title())));
    }
    v
}

#[allow(clippy::too_many_arguments)]
fn menu(
    ui: &mut egui::Ui,
    s: &mut Session,
    shared: &Shared,
    settings: &Settings,
    snippet_store: &SnippetStore,
    last_local_dir: &mut Option<std::path::PathBuf>,
    chosen: &mut Option<(Snippet, Use)>,
) {
    // The terminal shown: find in it, log it.
    if s.compact_view == V::Panes
        && let Some(Pane::Term(id)) = s.active
        && let Some(t) = s.terms.iter_mut().find(|t| t.id == id)
    {
        if ui.button("🔍 Find in the scrollback").clicked() {
            t.term.open_search();
            ui.close();
        }
        match t.term.log_path().map(|p| p.display().to_string()) {
            Some(p) => {
                if ui.button("Stop logging").on_hover_text(p).clicked() {
                    t.term.stop_log();
                    ui.close();
                }
            }
            None => {
                if ui.button("Log this terminal to a file").clicked() {
                    t.term.request_log();
                    ui.close();
                }
            }
        }
        ui.separator();
    }
    if s.state == ConnState::Connected && ui.button("⟳ New lasting terminal").clicked() {
        // Keeps running on the server if the connection drops.
        s.open_lasting_terminal(None, shared, settings);
        s.compact_view = V::Panes;
        ui.close();
    }
    let device = if s.local.is_some() { "Hide this device's files" } else { "🖥 This device's files" };
    if ui.button(device).clicked() {
        match s.local.take() {
            Some(l) => *last_local_dir = Some(l.path),
            None => {
                s.local = Some(crate::local::LocalBrowser::new(last_local_dir.clone()));
                s.compact_view = V::Local;
            }
        }
        ui.close();
    }
    let fw = format!("🔌 Port forwards ({}/{})", s.forwards.running(), s.forwards.len());
    if ui.button(fw).clicked() {
        s.forwards.open = !s.forwards.open;
        ui.close();
    }
    ui.menu_button("☰ Snippets", |ui| {
        let can_type = s.terms.iter().any(|t| t.is_live());
        if let Some(pick) = snippets::menu(ui, snippet_store, can_type) {
            *chosen = Some(pick);
        }
    });
    // Close what is shown.
    if s.compact_view == V::Panes {
        let close = match s.active {
            Some(Pane::Term(_)) => Some("✖ Close this terminal"),
            Some(Pane::Editor(_)) => Some("✖ Close this file"),
            Some(Pane::Log(_)) => Some("✖ Stop following this log"),
            None => None,
        };
        if let Some(label) = close {
            ui.separator();
            if ui.button(label).clicked() {
                match s.active {
                    Some(Pane::Term(id)) => s.close_terminal(id),
                    Some(Pane::Log(id)) => s.close_log(id),
                    Some(Pane::Editor(id)) => {
                        if s.editors.iter().any(|e| e.id == id && e.is_modified()) {
                            s.toast = Some((
                                "This file has unsaved changes. Save or reload it before closing.".into(),
                                true,
                                Instant::now(),
                            ));
                        } else {
                            s.close_editor(id);
                        }
                    }
                    None => {}
                }
                ui.close();
            }
        }
    }
}
