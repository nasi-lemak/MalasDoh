//! The remote file browser. It only draws and collects intent; the session
//! carries out each [`Action`] on the connection.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::{Align, Layout, RichText, Sense};
use malasdoh_core::sftp::{self, EntryKind, Progress, RemoteEntry, human_size, mode_string};

#[derive(Debug, Clone)]
pub enum Action {
    Navigate(String),
    Refresh,
    Edit { path: String, sudo: bool },
    ExternalEdit(String),
    Download(Vec<String>),
    UploadPick,
    Upload(Vec<std::path::PathBuf>),
    Mkdir(String),
    NewFile(String),
    Rename { from: String, to: String },
    Delete(Vec<String>),
    Chmod { path: String, mode: u32 },
    CdInTerminal(String),
    ShowBackups(String),
}

pub struct Transfer {
    pub label: String,
    pub progress: Arc<Progress>,
    pub result: Option<Result<String, String>>,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    Rename {
        from: String,
        name: String,
    },
    NewFolder(String),
    NewFile(String),
    Chmod {
        path: String,
        mode: String,
    },
    ConfirmDelete(Vec<String>),
}

#[derive(Default)]
pub struct Browser {
    pub path: String,
    path_input: String,
    pub entries: Vec<RemoteEntry>,
    pub loading: bool,
    pub error: Option<String>,
    selected: Vec<String>,
    filter: String,
    show_hidden: bool,
    back: Vec<String>,
    /// Set while a "back" navigation is in flight, so it is not recorded again.
    going_back: Option<String>,
    dialog: Dialog,
    pub transfers: Vec<Transfer>,
}

impl Browser {
    pub fn set_listing(&mut self, path: String, result: Result<Vec<RemoteEntry>, String>) {
        self.loading = false;
        match result {
            Ok(entries) => {
                let going_back = self.going_back.take().is_some();
                if !going_back && path != self.path && !self.path.is_empty() {
                    self.back.push(std::mem::take(&mut self.path));
                    if self.back.len() > 50 {
                        self.back.remove(0);
                    }
                }
                self.entries = entries;
                self.path_input = path.clone();
                self.path = path;
                self.error = None;
                self.selected.retain(|s| self.entries.iter().any(|e| &e.path == s));
            }
            Err(e) => {
                if let Some(prev) = self.going_back.take() {
                    self.back.push(prev);
                }
                self.path_input = self.path.clone();
                self.error = Some(e);
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, confirm_delete: bool) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            if ui.add_enabled(!self.back.is_empty(), egui::Button::new("⬅")).on_hover_text("Back").clicked()
                && let Some(prev) = self.back.pop()
            {
                self.going_back = Some(prev.clone());
                actions.push(Action::Navigate(prev));
            }
            if ui.button("⬆").on_hover_text("Parent folder").clicked() && !self.path.is_empty() {
                actions.push(Action::Navigate(sftp::parent(&self.path)));
            }
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                actions.push(Action::Refresh);
            }
            let r = ui
                .add(egui::TextEdit::singleline(&mut self.path_input).desired_width(f32::INFINITY).hint_text("/path"));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::Navigate(self.path_input.trim().to_string()));
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter").desired_width(120.0));
            ui.checkbox(&mut self.show_hidden, "Hidden");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("+ New", |ui| {
                    if ui.button("New file…").clicked() {
                        self.dialog = Dialog::NewFile(String::new());
                    }
                    if ui.button("New folder…").clicked() {
                        self.dialog = Dialog::NewFolder(String::new());
                    }
                    if ui.button("Upload files…").clicked() {
                        actions.push(Action::UploadPick);
                    }
                });
                if !self.selected.is_empty() && ui.button("⬇").on_hover_text("Download selected").clicked() {
                    actions.push(Action::Download(self.selected.clone()));
                }
            });
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if self.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading…");
            });
        }
        ui.separator();

        self.transfers_ui(ui);

        let filter = self.filter.to_lowercase();
        let visible: Vec<RemoteEntry> = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, visible.len(), |ui, range| {
            for e in &visible[range] {
                self.row_ui(ui, e, row_h, &mut actions);
            }
        });

        // Files dropped from the desktop upload into the current folder.
        let dropped: Vec<_> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            actions.push(Action::Upload(dropped));
        }

        self.dialog_ui(ui.ctx(), confirm_delete, &mut actions);
        actions
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, e: &RemoteEntry, row_h: f32, actions: &mut Vec<Action>) {
        let selected = self.selected.contains(&e.path);
        let icon = match e.kind {
            EntryKind::Dir => "🗀",
            EntryKind::Symlink { to_dir: true } => "🗁",
            EntryKind::Symlink { to_dir: false } => "🔗",
            EntryKind::File => "🗋",
            EntryKind::Other => "?",
        };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
        if selected {
            ui.painter().rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        // Painted rather than laid out as labels, so the whole row takes clicks.
        let inner = rect.shrink2(egui::vec2(6.0, 0.0));
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::TextStyle::Monospace.resolve(ui.style());
        let weak = ui.visuals().weak_text_color();
        let painter = ui.painter_at(rect);
        let mut right = inner.right();
        if let Some(m) = e.mode {
            let r = painter.text(
                egui::pos2(right, inner.center().y),
                egui::Align2::RIGHT_CENTER,
                mode_string(m),
                small.clone(),
                weak,
            );
            right = r.left() - 10.0;
        }
        if !e.is_dir_like() {
            let r = painter.text(
                egui::pos2(right, inner.center().y),
                egui::Align2::RIGHT_CENTER,
                human_size(e.size),
                body.clone(),
                weak,
            );
            right = r.left() - 10.0;
        }
        let name_color =
            if e.is_dir_like() || selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
        let name_rect = egui::Rect::from_min_max(inner.min, egui::pos2(right, inner.max.y));
        ui.painter_at(name_rect).text(
            egui::pos2(inner.left(), inner.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{icon}  {}", e.name),
            body,
            name_color,
        );

        if resp.clicked() {
            let multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
            if multi {
                if selected {
                    self.selected.retain(|p| p != &e.path);
                } else {
                    self.selected.push(e.path.clone());
                }
            } else {
                self.selected = vec![e.path.clone()];
            }
        }
        if resp.double_clicked() {
            if e.is_dir_like() {
                actions.push(Action::Navigate(e.path.clone()));
            } else {
                actions.push(Action::Edit { path: e.path.clone(), sudo: false });
            }
        }
        let tooltip = format!(
            "{}\n{}{}",
            e.path,
            e.mode.map(|m| format!("{} ({:o})  ", mode_string(m), m)).unwrap_or_default(),
            match (e.uid, e.gid) {
                (Some(u), Some(g)) => format!("uid {u} gid {g}"),
                _ => String::new(),
            }
        );
        let resp = resp.on_hover_text(tooltip);
        resp.context_menu(|ui| {
            if !self.selected.contains(&e.path) {
                self.selected = vec![e.path.clone()];
            }
            if e.is_dir_like() {
                if ui.button("Open").clicked() {
                    actions.push(Action::Navigate(e.path.clone()));
                    ui.close();
                }
                if ui.button("cd here in terminal").clicked() {
                    actions.push(Action::CdInTerminal(e.path.clone()));
                    ui.close();
                }
            } else {
                if ui.button("Edit").clicked() {
                    actions.push(Action::Edit { path: e.path.clone(), sudo: false });
                    ui.close();
                }
                if ui.button("Edit as root (sudo)").clicked() {
                    actions.push(Action::Edit { path: e.path.clone(), sudo: true });
                    ui.close();
                }
                if ui.button("Open in external editor").clicked() {
                    actions.push(Action::ExternalEdit(e.path.clone()));
                    ui.close();
                }
                if ui.button("Local backups…").clicked() {
                    actions.push(Action::ShowBackups(e.path.clone()));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Download").clicked() {
                actions.push(Action::Download(self.selected.clone()));
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(e.path.clone());
                ui.close();
            }
            if ui.button("Rename…").clicked() {
                self.dialog = Dialog::Rename { from: e.path.clone(), name: e.name.clone() };
                ui.close();
            }
            if ui.button("Permissions…").clicked() {
                self.dialog = Dialog::Chmod { path: e.path.clone(), mode: format!("{:o}", e.mode.unwrap_or(0o644)) };
                ui.close();
            }
            ui.separator();
            let n = self.selected.len();
            if ui.button(if n > 1 { format!("Delete {n} items") } else { "Delete".into() }).clicked() {
                self.dialog = Dialog::ConfirmDelete(self.selected.clone());
                ui.close();
            }
        });
    }

    fn transfers_ui(&mut self, ui: &mut egui::Ui) {
        if self.transfers.is_empty() {
            return;
        }
        let mut clear = false;
        for t in &self.transfers {
            ui.horizontal(|ui| {
                let total = t.progress.total_bytes.load(Ordering::Relaxed).max(1);
                let done = t.progress.done_bytes.load(Ordering::Relaxed);
                match &t.result {
                    None => {
                        ui.add(
                            egui::ProgressBar::new(done as f32 / total as f32).desired_width(120.0).show_percentage(),
                        );
                        ui.label(&t.label);
                        if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                            t.progress.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Some(Ok(msg)) => {
                        ui.label(format!("✔ {msg}"));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("✖ {}: {e}", t.label));
                    }
                }
            });
        }
        if self.transfers.iter().all(|t| t.result.is_some()) && ui.small_button("Clear finished").clicked() {
            clear = true;
        }
        if clear {
            self.transfers.clear();
        }
        ui.separator();
    }

    fn dialog_ui(&mut self, ctx: &egui::Context, confirm_delete: bool, actions: &mut Vec<Action>) {
        let dir = self.path.clone();
        let mut close = false;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::ConfirmDelete(paths) if !confirm_delete => {
                actions.push(Action::Delete(std::mem::take(paths)));
                close = true;
            }
            Dialog::ConfirmDelete(paths) => {
                egui::Modal::new("confirm-delete".into()).show(ctx, |ui| {
                    ui.heading("Delete from the server?");
                    for p in paths.iter().take(10) {
                        ui.monospace(p);
                    }
                    if paths.len() > 10 {
                        ui.label(format!("…and {} more", paths.len() - 10));
                    }
                    ui.label("Folders are deleted with everything in them. This cannot be undone.");
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(ui.visuals().error_fg_color)).clicked() {
                            actions.push(Action::Delete(std::mem::take(paths)));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Rename { from, name } => {
                let from = from.clone();
                if let Some(ok) = name_dialog(ctx, "Rename", name) {
                    if ok && !name.is_empty() {
                        actions.push(Action::Rename { to: sftp::join(&sftp::parent(&from), name), from });
                    }
                    close = true;
                }
            }
            Dialog::NewFolder(name) => {
                if let Some(ok) = name_dialog(ctx, "New folder", name) {
                    if ok && !name.is_empty() {
                        actions.push(Action::Mkdir(sftp::join(&dir, name)));
                    }
                    close = true;
                }
            }
            Dialog::NewFile(name) => {
                if let Some(ok) = name_dialog(ctx, "New file", name) {
                    if ok && !name.is_empty() {
                        actions.push(Action::NewFile(sftp::join(&dir, name)));
                    }
                    close = true;
                }
            }
            Dialog::Chmod { path, mode } => {
                let path = path.clone();
                if let Some(ok) = name_dialog(ctx, "Permissions (octal, e.g. 644)", mode) {
                    if ok {
                        match u32::from_str_radix(mode.trim(), 8) {
                            Ok(m) if m <= 0o7777 => actions.push(Action::Chmod { path, mode: m }),
                            _ => self.error = Some(format!("“{mode}” is not an octal mode like 644 or 0755")),
                        }
                    }
                    close = true;
                }
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
    }
}

/// A one-field dialog. `Some(true)` = OK, `Some(false)` = cancelled.
fn name_dialog(ctx: &egui::Context, title: &str, value: &mut String) -> Option<bool> {
    let mut result = None;
    egui::Modal::new(egui::Id::new(("name-dialog", title))).show(ctx, |ui| {
        ui.heading(title);
        let r = ui.text_edit_singleline(value);
        r.request_focus();
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            result = Some(true);
        }
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                result = Some(true);
            }
            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                result = Some(false);
            }
        });
    });
    result
}
