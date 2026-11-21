//! Saved commands. `{{name}}` placeholders are asked for when a snippet is used.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use egui::RichText;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    pub name: String,
    /// Shown as a submenu; empty means top level.
    pub folder: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnippetStore {
    #[serde(default, rename = "snippet")]
    pub snippets: Vec<Snippet>,
}

fn s(folder: &str, name: &str, command: &str) -> Snippet {
    Snippet { name: name.into(), folder: folder.into(), command: command.into() }
}

impl Default for SnippetStore {
    /// A starter set; users edit or delete them freely.
    fn default() -> Self {
        Self {
            snippets: vec![
                s("System", "Disk usage", "df -h"),
                s("System", "Biggest folders here", "du -xh --max-depth=1 . 2>/dev/null | sort -h | tail -20"),
                s("System", "Memory", "free -h"),
                s("System", "Listening ports", "ss -tlnp"),
                s("System", "Uptime and load", "uptime"),
                s("Services", "Status", "systemctl status {{service}} --no-pager"),
                s("Services", "Restart", "sudo systemctl restart {{service}}"),
                s("Services", "Follow log", "journalctl -u {{service}} -f"),
                s("Services", "Failed units", "systemctl --failed --no-pager"),
                s("nginx", "Test config", "sudo nginx -t"),
                s("nginx", "Reload", "sudo nginx -t && sudo systemctl reload nginx"),
                s("nginx", "Recent errors", "sudo tail -n 100 /var/log/nginx/error.log"),
            ],
        }
    }
}

impl SnippetStore {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding snippets")?;
        malasdoh_core::paths::write_atomic(path, text.as_bytes())
    }

    pub fn folders(&self) -> Vec<String> {
        let mut f: Vec<String> = self.snippets.iter().map(|s| s.folder.clone()).collect();
        f.sort();
        f.dedup();
        f
    }
}

/// Placeholder names in order of first appearance.
pub fn placeholders(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = cmd;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim().to_string();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[end + 2..];
    }
    out
}

pub fn fill(cmd: &str, values: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push_str(&rest[..start]);
        let name = after[..end].trim();
        match values.get(name) {
            Some(v) => out.push_str(v),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// What to do with a chosen snippet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Type it into the terminal, without pressing Enter.
    Insert,
    /// Type it and press Enter.
    Run,
    /// Open "Run on many hosts" with it.
    Multi,
}

/// A snippet menu. Returns the chosen snippet and how to use it.
pub fn menu(ui: &mut egui::Ui, store: &SnippetStore, allow_terminal: bool) -> Option<(Snippet, Use)> {
    let mut chosen = None;
    let mut row = |ui: &mut egui::Ui, sn: &Snippet| {
        ui.horizontal(|ui| {
            ui.label(&sn.name).on_hover_text(RichText::new(&sn.command).monospace());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Many hosts…").clicked() {
                    chosen = Some((sn.clone(), Use::Multi));
                }
                if allow_terminal {
                    if ui.small_button("Run").on_hover_text("Type it and press Enter").clicked() {
                        chosen = Some((sn.clone(), Use::Run));
                    }
                    if ui.small_button("Insert").on_hover_text("Type it without pressing Enter").clicked() {
                        chosen = Some((sn.clone(), Use::Insert));
                    }
                }
            });
        });
    };
    ui.set_min_width(340.0);
    for folder in store.folders() {
        let items: Vec<&Snippet> = store.snippets.iter().filter(|s| s.folder == folder).collect();
        if folder.is_empty() {
            for sn in items {
                row(ui, sn);
            }
        } else {
            ui.menu_button(&folder, |ui| {
                ui.set_min_width(340.0);
                for sn in items {
                    row(ui, sn);
                }
            });
        }
    }
    if chosen.is_some() {
        ui.close();
    }
    chosen
}

/// The snippet manager. Returns true when something changed.
pub fn manager_ui(ui: &mut egui::Ui, store: &mut SnippetStore) -> bool {
    let before = store.clone();
    ui.label("Use {{name}} for a value asked for each time, e.g. systemctl restart {{service}}.");
    let mut remove = None;
    let narrow = crate::dlg::narrow(ui.ctx());
    egui::ScrollArea::vertical().max_height(if narrow { f32::INFINITY } else { 420.0 }).show(ui, |ui| {
        if narrow {
            // A phone: each snippet's fields one under the other.
            for (i, sn) in store.snippets.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut sn.name)
                                .hint_text("Name")
                                .desired_width(crate::dlg::field_width(ui, 400.0)),
                        );
                        if ui.small_button("✖").on_hover_text("Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut sn.folder)
                            .hint_text("Folder")
                            .desired_width(crate::dlg::field_width(ui, 400.0)),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut sn.command)
                            .hint_text("Command")
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                });
            }
            return;
        }
        egui::Grid::new("snippet-grid").striped(true).num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
            for h in ["Folder", "Name", "Command", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for (i, sn) in store.snippets.iter_mut().enumerate() {
                ui.add(egui::TextEdit::singleline(&mut sn.folder).desired_width(100.0));
                ui.add(egui::TextEdit::singleline(&mut sn.name).desired_width(160.0));
                ui.add(
                    egui::TextEdit::singleline(&mut sn.command)
                        .desired_width(crate::dlg::field_width(ui, 380.0))
                        .font(egui::TextStyle::Monospace),
                );
                if ui.small_button("✖").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    });
    if let Some(i) = remove {
        store.snippets.remove(i);
    }
    ui.horizontal(|ui| {
        if ui.button("+ Add snippet").clicked() {
            store.snippets.push(Snippet { name: "New snippet".into(), ..Default::default() });
        }
        if ui.button("Restore the starter set").clicked() {
            let defaults = SnippetStore::default();
            for d in defaults.snippets {
                if !store.snippets.iter().any(|s| s.name == d.name && s.folder == d.folder) {
                    store.snippets.push(d);
                }
            }
        }
    });
    *store != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_fill() {
        let cmd = "journalctl -u {{service}} --since '{{ since }}' | grep {{service}}";
        assert_eq!(placeholders(cmd), ["service", "since"]);
        let v: HashMap<String, String> = [("service".into(), "nginx".into()), ("since".into(), "1h ago".into())].into();
        assert_eq!(fill(cmd, &v), "journalctl -u nginx --since '1h ago' | grep nginx");
        // Unknown and unterminated placeholders are left alone.
        assert_eq!(fill("a {{x}} b {{y", &HashMap::new()), "a {{x}} b {{y");
        assert!(placeholders("echo }} {{").is_empty());
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("snippets.toml");
        let store = SnippetStore::default();
        store.save(&p).unwrap();
        assert_eq!(SnippetStore::load(&p), store);
    }
}
