//! The remote file browser. It only draws and collects intent; the session
//! carries out each [`Action`] on the connection.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::{Align, Layout, RichText, Sense};
use malasdoh_core::commands::Archive;
use malasdoh_core::sftp::{self, EntryKind, Progress, RemoteEntry, human_size, mode_string};

#[derive(Debug, Clone)]
pub enum Action {
    Navigate(String),
    Refresh,
    OpenSync,
    /// Copy to a folder on another connected server.
    CopyToServer(Vec<String>),
    /// Compare two files, or one with something the user picks.
    Compare(Vec<String>),
    /// Show this file's saves in the change journal.
    History(String),
    Edit {
        path: String,
        sudo: bool,
    },
    ExternalEdit(String),
    Download(Vec<String>),
    UploadPick,
    Upload(Vec<std::path::PathBuf>),
    Mkdir(String),
    NewFile(String),
    Rename {
        from: String,
        to: String,
    },
    Delete(Vec<String>),
    Chmod {
        path: String,
        mode: u32,
    },
    CdInTerminal(String),
    ShowBackups(String),
    Chown {
        paths: Vec<String>,
        owner: String,
        recursive: bool,
        sudo: bool,
    },
    Symlink {
        target: String,
        link: String,
    },
    Extract(String),
    Compress {
        names: Vec<String>,
        out: String,
        kind: Archive,
    },
    /// Follow a file as it grows.
    Follow {
        path: String,
        sudo: bool,
    },
    OpenFind,
    /// Add or remove the current folder from this connection's bookmarks.
    ToggleBookmark(String),
    /// Browse as root (SFTP through sudo), or back to your own user.
    ToggleRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
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
    Owner {
        paths: Vec<String>,
        owner: String,
        recursive: bool,
        sudo: bool,
        has_dir: bool,
    },
    Symlink {
        name: String,
        target: String,
    },
    Compress {
        names: Vec<String>,
        out: String,
        kind: Archive,
    },
}

#[derive(Default)]
pub struct Browser {
    pub path: String,
    path_input: String,
    pub entries: Vec<RemoteEntry>,
    pub loading: bool,
    pub error: Option<String>,
    /// Shown under the toolbar, e.g. that files go through shell commands.
    pub note: Option<String>,
    selected: Vec<String>,
    filter: String,
    show_hidden: bool,
    back: Vec<String>,
    /// Set while a "back" navigation is in flight, so it is not recorded again.
    going_back: Option<String>,
    dialog: Dialog,
    pub transfers: Vec<Transfer>,
    /// The owning session, for drag and drop.
    pub session: crate::bridge::SessionId,
    sort: SortBy,
    descending: bool,
    /// This connection's saved folders.
    pub bookmarks: Vec<String>,
    /// Select this entry once its folder is listed ("show in browser").
    pub reveal: Option<String>,
    /// Files are being handled as root; double-click edits as root too.
    pub root: bool,
    /// Switching to or from root is under way.
    pub root_pending: bool,
}

impl Browser {
    pub fn new(session: crate::bridge::SessionId) -> Self {
        Self { session, ..Default::default() }
    }

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
                if let Some(r) = self.reveal.take()
                    && self.entries.iter().any(|e| e.path == r)
                {
                    self.selected = vec![r];
                }
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
            let label = if self.root {
                RichText::new("root")
                    .strong()
                    .color(egui::Color32::WHITE)
                    .background_color(egui::Color32::from_rgb(0xa0, 0x30, 0x30))
            } else {
                RichText::new("root")
            };
            let tip = if self.root {
                "Browsing as root through sudo. Click to go back to your own user."
            } else {
                "Browse, transfer and edit as root (SFTP through sudo)"
            };
            if ui.add_enabled(!self.root_pending, egui::Button::new(label)).on_hover_text(tip).clicked() {
                actions.push(Action::ToggleRoot);
            }
            let r = ui
                .add(egui::TextEdit::singleline(&mut self.path_input).desired_width(f32::INFINITY).hint_text("/path"));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::Navigate(self.path_input.trim().to_string()));
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter").desired_width(110.0));
            ui.checkbox(&mut self.show_hidden, "Hidden");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("+ New", |ui| {
                    if ui.button("New file…").clicked() {
                        self.dialog = Dialog::NewFile(String::new());
                    }
                    if ui.button("New folder…").clicked() {
                        self.dialog = Dialog::NewFolder(String::new());
                    }
                    if ui.button("New symlink…").clicked() {
                        self.dialog = Dialog::Symlink { name: String::new(), target: String::new() };
                    }
                    if ui.button("Upload files…").clicked() {
                        actions.push(Action::UploadPick);
                    }
                    ui.separator();
                    if ui.button("Compare / sync with a local folder…").clicked() {
                        actions.push(Action::OpenSync);
                    }
                });
                if !self.selected.is_empty() && ui.button("⬇").on_hover_text("Download selected").clicked() {
                    actions.push(Action::Download(self.selected.clone()));
                }
                if ui.button("🔍").on_hover_text("Find files by name or contents").clicked() {
                    actions.push(Action::OpenFind);
                }
            });
        });
        ui.horizontal(|ui| {
            let here = self.path.clone();
            let marked = self.bookmarks.contains(&here);
            ui.menu_button("Bookmarks", |ui| {
                let label = if marked { "Remove this folder" } else { "Bookmark this folder" };
                if ui.add_enabled(!here.is_empty(), egui::Button::new(label)).clicked() {
                    actions.push(Action::ToggleBookmark(here.clone()));
                    ui.close();
                }
                if !self.bookmarks.is_empty() {
                    ui.separator();
                }
                for b in &self.bookmarks {
                    if ui.selectable_label(*b == here, b).clicked() {
                        actions.push(Action::Navigate(b.clone()));
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Saved folders for this connection");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let arrow = if self.descending { "⬇" } else { "⬆" };
                if ui.small_button(arrow).on_hover_text("Reverse the order").clicked() {
                    self.descending = !self.descending;
                }
                egui::ComboBox::from_id_salt(("sort", self.session))
                    .selected_text(match self.sort {
                        SortBy::Name => "Name",
                        SortBy::Size => "Size",
                        SortBy::Modified => "Modified",
                    })
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.sort, SortBy::Name, "Name");
                        ui.selectable_value(&mut self.sort, SortBy::Size, "Size");
                        ui.selectable_value(&mut self.sort, SortBy::Modified, "Modified");
                    });
                ui.label("Sort");
            });
        });
        if let Some(n) = &self.note {
            ui.label(RichText::new(n).weak().small());
        }
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
        let mut visible: Vec<RemoteEntry> = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        sort_entries(&mut visible, self.sort, self.descending);
        // The date column only when there is room for it.
        let show_time = ui.available_width() > 430.0 || self.sort == SortBy::Modified;

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let pane = egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            visible.len(),
            |ui, range| {
                for e in &visible[range] {
                    self.row_ui(ui, e, row_h, show_time, &mut actions);
                }
            },
        );
        // Local files dragged from the local pane upload into this folder.
        let drop = ui.interact(pane.inner_rect, egui::Id::new(("remote-drop", self.session)), Sense::hover());
        if drop.dnd_hover_payload::<crate::local::LocalDrag>().is_some() {
            ui.painter().rect_stroke(pane.inner_rect, 4.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
        }
        if let Some(p) = drop.dnd_release_payload::<crate::local::LocalDrag>() {
            actions.push(Action::Upload(p.0.clone()));
        }

        // Files dropped from the desktop upload into the current folder.
        let dropped: Vec<_> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            actions.push(Action::Upload(dropped));
        }

        self.dialog_ui(ui.ctx(), confirm_delete, &mut actions);
        actions
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, e: &RemoteEntry, row_h: f32, show_time: bool, actions: &mut Vec<Action>) {
        let selected = self.selected.contains(&e.path);
        let icon = match e.kind {
            EntryKind::Dir => "🗀",
            EntryKind::Symlink { to_dir: true } => "🗁",
            EntryKind::Symlink { to_dir: false } => "🔗",
            EntryKind::File => "🗋",
            EntryKind::Other => "?",
        };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click_and_drag());
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
        if show_time && let Some(t) = e.mtime {
            let r = painter.text(
                egui::pos2(right, inner.center().y),
                egui::Align2::RIGHT_CENTER,
                short_time(t),
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

        if resp.drag_started() && !selected {
            self.selected = vec![e.path.clone()];
        }
        let dragged = if selected { self.selected.clone() } else { vec![e.path.clone()] };
        resp.dnd_set_drag_payload(crate::local::RemoteDrag { session: self.session, paths: dragged });
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
                actions.push(Action::Edit { path: e.path.clone(), sudo: self.root });
            }
        }
        let tooltip = format!(
            "{}{}\n{}{}",
            e.path,
            e.mtime
                .map(|t| format!("\nModified {}", malasdoh_core::journal::format_time(t as i64)))
                .unwrap_or_default(),
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
                    actions.push(Action::Edit { path: e.path.clone(), sudo: self.root });
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
                if ui.button("Change history…").on_hover_text("Saves of this file in the change journal").clicked() {
                    actions.push(Action::History(e.path.clone()));
                    ui.close();
                }
                ui.menu_button("Follow (tail -f)", |ui| {
                    if ui.button("Follow").on_hover_text("Show new lines as they are written").clicked() {
                        actions.push(Action::Follow { path: e.path.clone(), sudo: false });
                        ui.close();
                    }
                    if ui.button("Follow as root (sudo)").clicked() {
                        actions.push(Action::Follow { path: e.path.clone(), sudo: true });
                        ui.close();
                    }
                });
                if let Some((_, stem)) = Archive::detect(&e.name) {
                    let what = if Archive::detect(&e.name).is_some_and(|(k, _)| k == Archive::Gz) {
                        format!("Unpack to {stem}")
                    } else {
                        format!("Unpack into {stem}/")
                    };
                    if ui.button(what).on_hover_text("Into a new folder next to it; nothing is overwritten").clicked() {
                        actions.push(Action::Extract(e.path.clone()));
                        ui.close();
                    }
                }
                ui.separator();
                if self.selected.len() == 2 {
                    if ui.button("Compare the two selected files").clicked() {
                        actions.push(Action::Compare(self.selected.clone()));
                        ui.close();
                    }
                } else if ui.button("Compare with…").on_hover_text("Another server, a local file or a backup").clicked()
                {
                    actions.push(Action::Compare(vec![e.path.clone()]));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Download").clicked() {
                actions.push(Action::Download(self.selected.clone()));
                ui.close();
            }
            if ui.button("Copy to another server…").clicked() {
                actions.push(Action::CopyToServer(self.selected.clone()));
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
            if ui.button("Owner…").on_hover_text("chown").clicked() {
                let has_dir = self.entries.iter().any(|x| self.selected.contains(&x.path) && x.is_dir_like());
                self.dialog = Dialog::Owner {
                    paths: self.selected.clone(),
                    owner: String::new(),
                    recursive: false,
                    sudo: true,
                    has_dir,
                };
                ui.close();
            }
            if ui.button("Compress…").on_hover_text("Pack the selection into a .tar.gz or .zip here").clicked() {
                let names: Vec<String> = self.selected.iter().map(|p| sftp::file_name(p).to_string()).collect();
                let base = if names.len() == 1 { names[0].clone() } else { sftp::file_name(&self.path).to_string() };
                let base = if base.is_empty() { "archive".to_string() } else { base };
                self.dialog = Dialog::Compress { names, out: format!("{base}.tar.gz"), kind: Archive::TarGz };
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
            // Errors wrap so the reason is readable; the rest stays one line.
            if let Some(Err(e)) = &t.result {
                ui.add(
                    egui::Label::new(RichText::new(format!("✖ {}: {e}", t.label)).color(ui.visuals().error_fg_color))
                        .wrap(),
                );
                continue;
            }
            ui.horizontal(|ui| {
                let total = t.progress.total_bytes.load(Ordering::Relaxed).max(1);
                let done = t.progress.done_bytes.load(Ordering::Relaxed);
                match &t.result {
                    None => {
                        ui.add(
                            egui::ProgressBar::new(done as f32 / total as f32).desired_width(120.0).show_percentage(),
                        );
                        if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                            t.progress.cancel.store(true, Ordering::Relaxed);
                        }
                        ui.add(egui::Label::new(&t.label).truncate());
                    }
                    Some(Ok(msg)) => {
                        ui.add(egui::Label::new(format!("✔ {msg}")).truncate()).on_hover_text(msg);
                    }
                    Some(Err(_)) => {}
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
            Dialog::Owner { paths, owner, recursive, sudo, has_dir } => {
                egui::Modal::new("owner-dialog".into()).show(ctx, |ui| {
                    ui.heading("Change owner");
                    for p in paths.iter().take(6) {
                        ui.monospace(p);
                    }
                    if paths.len() > 6 {
                        ui.label(format!("…and {} more", paths.len() - 6));
                    }
                    ui.horizontal(|ui| {
                        ui.label("Owner");
                        let r = ui.add(egui::TextEdit::singleline(owner).hint_text("user, user:group or :group"));
                        r.request_focus();
                    });
                    if *has_dir {
                        ui.checkbox(recursive, "Also everything inside the folders");
                    }
                    ui.checkbox(sudo, "As root (sudo)").on_hover_text("Changing the owner usually needs root");
                    let valid = malasdoh_core::commands::check_owner(owner.trim());
                    if let Err(e) = &valid
                        && !owner.trim().is_empty()
                    {
                        ui.colored_label(ui.visuals().warn_fg_color, e.to_string());
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid.is_ok(), egui::Button::new("Change")).clicked() {
                            actions.push(Action::Chown {
                                paths: std::mem::take(paths),
                                owner: owner.trim().to_string(),
                                recursive: *recursive,
                                sudo: *sudo,
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Symlink { name, target } => {
                egui::Modal::new("symlink-dialog".into()).show(ctx, |ui| {
                    ui.heading("New symlink");
                    egui::Grid::new("symlink-grid").num_columns(2).show(ui, |ui| {
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(name).hint_text("current"));
                        ui.end_row();
                        ui.label("Points to");
                        ui.add(egui::TextEdit::singleline(target).hint_text("releases/2026-10-10 or /etc/app.conf"));
                        ui.end_row();
                    });
                    ui.label(RichText::new(format!("Created in {dir}. An existing name is never replaced.")).weak());
                    ui.horizontal(|ui| {
                        let ok = !name.trim().is_empty() && !target.trim().is_empty() && !name.contains('/');
                        if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
                            actions.push(Action::Symlink {
                                target: target.trim().to_string(),
                                link: sftp::join(&dir, name.trim()),
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Compress { names, out, kind } => {
                egui::Modal::new("compress-dialog".into()).show(ctx, |ui| {
                    ui.heading("Compress");
                    ui.label(format!("{} item(s) from {dir}", names.len()));
                    ui.horizontal(|ui| {
                        ui.label("Archive");
                        ui.add(egui::TextEdit::singleline(out).desired_width(240.0));
                        let before = *kind;
                        egui::ComboBox::from_id_salt("archive-kind").selected_text(kind.suffix()).show_ui(ui, |ui| {
                            for k in [Archive::TarGz, Archive::TarXz, Archive::TarBz2, Archive::Tar, Archive::Zip] {
                                ui.selectable_value(kind, k, k.suffix());
                            }
                        });
                        // Keep the name's ending in step with the chosen type.
                        if *kind != before {
                            let stem = Archive::detect(out).map(|(_, s)| s.to_string()).unwrap_or_else(|| out.clone());
                            *out = format!("{stem}{}", kind.suffix());
                        }
                    });
                    ui.horizontal(|ui| {
                        let ok = !out.trim().is_empty() && !out.contains('/');
                        if ui.add_enabled(ok, egui::Button::new("Compress")).clicked() {
                            actions.push(Action::Compress {
                                names: std::mem::take(names),
                                out: out.trim().to_string(),
                                kind: *kind,
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                });
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
    }
}

/// Folders first, then by the chosen key (name breaks ties).
pub fn sort_entries(v: &mut [RemoteEntry], by: SortBy, descending: bool) {
    v.sort_by(|a, b| {
        let dirs = b.is_dir_like().cmp(&a.is_dir_like());
        let name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
        let key = match by {
            SortBy::Name => name(),
            SortBy::Size => a.size.cmp(&b.size).then_with(name),
            SortBy::Modified => a.mtime.cmp(&b.mtime).then_with(name),
        };
        dirs.then(if descending { key.reverse() } else { key })
    });
}

/// "11:23" today, "Oct 03" this year, "2024-05-01" before (UTC).
fn short_time(t: u32) -> String {
    let Ok(d) = time::OffsetDateTime::from_unix_timestamp(t as i64) else { return String::new() };
    let now = time::OffsetDateTime::now_utc();
    let fmt = if d.date() == now.date() {
        time::macros::format_description!("[hour]:[minute]")
    } else if d.year() == now.year() {
        time::macros::format_description!("[month repr:short] [day]")
    } else {
        time::macros::format_description!("[year]-[month]-[day]")
    };
    d.format(&fmt).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64, mtime: u32) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            path: format!("/x/{name}"),
            kind: if dir { EntryKind::Dir } else { EntryKind::File },
            size,
            mtime: Some(mtime),
            mode: None,
            uid: None,
            gid: None,
        }
    }

    #[test]
    fn sorting_keeps_folders_first() {
        let mut v = vec![entry("b.log", false, 10, 3), entry("z", true, 0, 1), entry("A.conf", false, 99, 2)];
        let names = |v: &[RemoteEntry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        sort_entries(&mut v, SortBy::Name, false);
        assert_eq!(names(&v), ["z", "A.conf", "b.log"]);
        sort_entries(&mut v, SortBy::Size, true);
        assert_eq!(names(&v), ["z", "A.conf", "b.log"]);
        sort_entries(&mut v, SortBy::Modified, true);
        assert_eq!(names(&v), ["z", "b.log", "A.conf"]);
    }
}
