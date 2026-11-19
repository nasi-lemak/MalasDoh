//! The remote file browser. It only draws and collects intent; the session
//! carries out each [`Action`] on the connection.

use crate::listsel::KeyAction;
use egui::{Align, Layout, RichText, Sense};
use malasdoh_core::commands::Archive;
use malasdoh_core::sftp::{self, EntryKind, RemoteEntry, human_size, mode_string};

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
    /// Change this file on several servers at once.
    EditMany(String),
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
    OpenPatch,
    /// Pause, resume or cancel a row of the transfer queue.
    Queue(crate::queue::Control),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
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
    sel: crate::listsel::Selection<String>,
    /// The list's scroll offset and height last frame, for keeping the
    /// keyboard's row in view.
    list_view: (f32, f32),
    filter: String,
    show_hidden: bool,
    back: Vec<String>,
    /// Where Back came from, for Forward.
    forward: Vec<String>,
    /// A Back (`true`) or Forward navigation in flight, and its target, so it
    /// is not recorded as a new visit (and is put back if it fails).
    going: Option<(bool, String)>,
    dialog: Dialog,
    pub transfers: crate::queue::Queue,
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
                if path != self.path && !self.path.is_empty() {
                    let here = std::mem::take(&mut self.path);
                    match self.going.take() {
                        Some((true, _)) => self.forward.push(here),
                        Some((false, _)) => self.back.push(here),
                        None => {
                            self.back.push(here);
                            self.forward.clear();
                        }
                    }
                    if self.back.len() > 50 {
                        self.back.remove(0);
                    }
                }
                self.going = None;
                self.entries = entries;
                self.path_input = path.clone();
                self.path = path;
                self.error = None;
                let entries = &self.entries;
                self.sel.retain(|s| entries.iter().any(|e| &e.path == s));
                if let Some(r) = self.reveal.take()
                    && self.entries.iter().any(|e| e.path == r)
                {
                    self.sel.only(&r);
                }
            }
            Err(e) => {
                match self.going.take() {
                    Some((true, to)) => self.back.push(to),
                    Some((false, to)) => self.forward.push(to),
                    None => {}
                }
                self.path_input = self.path.clone();
                self.error = Some(e);
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, confirm_delete: bool) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            if ui.add_enabled(!self.back.is_empty(), egui::Button::new("⬅")).on_hover_text("Back (Alt+Left)").clicked()
            {
                self.history(false, &mut actions);
            }
            if !self.forward.is_empty() && ui.button("➡").on_hover_text("Forward (Alt+Right)").clicked() {
                self.history(true, &mut actions);
            }
            if ui.button("⬆").on_hover_text("Parent folder (Alt+Up or Backspace)").clicked() && !self.path.is_empty()
            {
                actions.push(Action::Navigate(sftp::parent(&self.path)));
            }
            if ui.button("⟳").on_hover_text("Refresh (F5)").clicked() {
                actions.push(Action::Refresh);
            }
            let label = if self.root {
                RichText::new("root")
                    .strong()
                    .color(crate::look::sem(ui.ctx()).on_danger)
                    .background_color(crate::look::sem(ui.ctx()).danger)
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
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.path_input)
                    .id(egui::Id::new(("path", self.session)))
                    .desired_width(f32::INFINITY)
                    .hint_text("/path"),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::Navigate(self.path_input.trim().to_string()));
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .id(egui::Id::new(("filter", self.session)))
                    .hint_text("Filter (Ctrl+F)")
                    .desired_width(110.0),
            );
            ui.checkbox(&mut self.show_hidden, "Hidden");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("+ New", |ui| {
                    if ui.button("New file…").clicked() {
                        self.dialog = Dialog::NewFile(String::new());
                    }
                    if ui.add(egui::Button::new("New folder…").shortcut_text("Ctrl+Shift+N")).clicked() {
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
                    if ui
                        .button("Apply a patch…")
                        .on_hover_text("diff -u or git diff, checked before anything is written")
                        .clicked()
                    {
                        actions.push(Action::OpenPatch);
                    }
                });
                if !self.sel.selected.is_empty() && ui.button("⬇").on_hover_text("Download selected").clicked() {
                    actions.push(Action::Download(self.sel.selected.clone()));
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

        for c in self.transfers.ui(ui, self.session) {
            actions.push(Action::Queue(c));
        }

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
        // The list's keys (once it was clicked): Ctrl+A, arrows, Enter, Delete, F2, Backspace.
        let order: Vec<String> = visible.iter().map(|e| e.path.clone()).collect();
        let (key, scroll_to) = self.sel.keys(ui.ctx(), &order, |p| sftp::file_name(p).to_string());
        match key {
            Some(KeyAction::Open(p)) => match visible.iter().find(|e| e.path == p) {
                Some(e) if e.is_dir_like() => actions.push(Action::Navigate(p)),
                Some(_) => actions.push(Action::Edit { path: p, sudo: self.root }),
                None => {}
            },
            Some(KeyAction::Delete) => self.dialog = Dialog::ConfirmDelete(self.sel.selected.clone()),
            Some(KeyAction::Rename(p)) => {
                self.dialog = Dialog::Rename { name: sftp::file_name(&p).to_string(), from: p };
            }
            Some(KeyAction::Parent) if self.path != "/" && !self.path.is_empty() => {
                actions.push(Action::Navigate(sftp::parent(&self.path)));
            }
            Some(KeyAction::Back) => self.history(false, &mut actions),
            Some(KeyAction::Forward) => self.history(true, &mut actions),
            Some(KeyAction::Refresh) => actions.push(Action::Refresh),
            Some(KeyAction::FocusPath) => {
                self.sel.focused = false;
                crate::shortcut::focus_select(
                    ui.ctx(),
                    egui::Id::new(("path", self.session)),
                    self.path_input.chars().count(),
                );
            }
            Some(KeyAction::FocusFilter) => {
                self.sel.focused = false;
                crate::shortcut::focus_select(
                    ui.ctx(),
                    egui::Id::new(("filter", self.session)),
                    self.filter.chars().count(),
                );
            }
            Some(KeyAction::NewFolder) => self.dialog = Dialog::NewFolder(String::new()),
            Some(KeyAction::Copy) => ui.ctx().copy_text(self.sel.selected.join("\n")),
            _ => {}
        }
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(i) = scroll_to
            && let Some(y) = crate::listsel::scroll_for(i, row_h, self.list_view.0, self.list_view.1)
        {
            area = area.vertical_scroll_offset(y);
        }
        let pane = area.show_rows(ui, row_h, visible.len(), |ui, range| {
            for e in &visible[range] {
                self.row_ui(ui, e, &order, row_h, show_time, &mut actions);
            }
        });
        self.list_view = (pane.state.offset.y, pane.inner_rect.height());
        self.sel.track_focus(ui, pane.inner_rect);
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

    /// Back (or Forward, with `forward`) through the folders visited.
    fn history(&mut self, forward: bool, actions: &mut Vec<Action>) {
        let to = if forward { self.forward.pop() } else { self.back.pop() };
        if let Some(to) = to {
            self.going = Some((!forward, to.clone()));
            actions.push(Action::Navigate(to));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row_ui(
        &mut self,
        ui: &mut egui::Ui,
        e: &RemoteEntry,
        order: &[String],
        row_h: f32,
        show_time: bool,
        actions: &mut Vec<Action>,
    ) {
        let selected = self.sel.selected.contains(&e.path);
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
            self.sel.only(&e.path);
        }
        let dragged = if selected { self.sel.selected.clone() } else { vec![e.path.clone()] };
        resp.dnd_set_drag_payload(crate::local::RemoteDrag { session: self.session, paths: dragged });
        if resp.clicked() {
            let (ctrl, shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));
            self.sel.click(order, &e.path, ctrl, shift);
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
        self.sel.menu_popup(&resp, &e.path).show(|ui| {
            if !self.sel.contains(&e.path) {
                self.sel.only(&e.path);
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
                if ui
                    .button("Edit on many servers…")
                    .on_hover_text("Make the same change to this file on other servers too")
                    .clicked()
                {
                    actions.push(Action::EditMany(e.path.clone()));
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
                if self.sel.selected.len() == 2 {
                    if ui.button("Compare the two selected files").clicked() {
                        actions.push(Action::Compare(self.sel.selected.clone()));
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
                actions.push(Action::Download(self.sel.selected.clone()));
                ui.close();
            }
            if ui.button("Copy to another server…").clicked() {
                actions.push(Action::CopyToServer(self.sel.selected.clone()));
                ui.close();
            }
            if ui.add(egui::Button::new("Copy path").shortcut_text("Ctrl+C")).clicked() {
                ui.ctx().copy_text(self.sel.selected.join("\n"));
                ui.close();
            }
            if ui.add(egui::Button::new("Rename…").shortcut_text("F2")).clicked() {
                self.dialog = Dialog::Rename { from: e.path.clone(), name: e.name.clone() };
                ui.close();
            }
            if ui.button("Permissions…").clicked() {
                self.dialog = Dialog::Chmod { path: e.path.clone(), mode: format!("{:o}", e.mode.unwrap_or(0o644)) };
                ui.close();
            }
            if ui.button("Owner…").on_hover_text("chown").clicked() {
                let has_dir = self.entries.iter().any(|x| self.sel.selected.contains(&x.path) && x.is_dir_like());
                self.dialog = Dialog::Owner {
                    paths: self.sel.selected.clone(),
                    owner: String::new(),
                    recursive: false,
                    sudo: true,
                    has_dir,
                };
                ui.close();
            }
            if ui.button("Compress…").on_hover_text("Pack the selection into a .tar.gz or .zip here").clicked() {
                let names: Vec<String> = self.sel.selected.iter().map(|p| sftp::file_name(p).to_string()).collect();
                let base = if names.len() == 1 { names[0].clone() } else { sftp::file_name(&self.path).to_string() };
                let base = if base.is_empty() { "archive".to_string() } else { base };
                self.dialog = Dialog::Compress { names, out: format!("{base}.tar.gz"), kind: Archive::TarGz };
                ui.close();
            }
            ui.separator();
            let n = self.sel.selected.len();
            let label = if n > 1 { format!("Delete {n} items") } else { "Delete".into() };
            if ui.add(egui::Button::new(label).shortcut_text("Del")).clicked() {
                self.dialog = Dialog::ConfirmDelete(self.sel.selected.clone());
                ui.close();
            }
        });
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
                        if ui.add(crate::look::danger(ui, "Delete")).clicked() || crate::dlg::enter(ui, false) {
                            actions.push(Action::Delete(std::mem::take(paths)));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::escape(ui) {
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
                        if ui.add_enabled(valid.is_ok(), egui::Button::new("Change")).clicked()
                            || (valid.is_ok() && crate::dlg::enter(ui, false))
                        {
                            actions.push(Action::Chown {
                                paths: std::mem::take(paths),
                                owner: owner.trim().to_string(),
                                recursive: *recursive,
                                sudo: *sudo,
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::escape(ui) {
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
                        if ui.add_enabled(ok, egui::Button::new("Create")).clicked()
                            || (ok && crate::dlg::enter(ui, false))
                        {
                            actions.push(Action::Symlink {
                                target: target.trim().to_string(),
                                link: sftp::join(&dir, name.trim()),
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::escape(ui) {
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
                        if ui.add_enabled(ok, egui::Button::new("Compress")).clicked()
                            || (ok && crate::dlg::enter(ui, false))
                        {
                            actions.push(Action::Compress {
                                names: std::mem::take(names),
                                out: out.trim().to_string(),
                                kind: *kind,
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::escape(ui) {
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
pub(crate) fn name_dialog(ctx: &egui::Context, title: &str, value: &mut String) -> Option<bool> {
    let mut result = None;
    egui::Modal::new(egui::Id::new(("name-dialog", title))).show(ctx, |ui| {
        ui.heading(title);
        let r = ui.text_edit_singleline(value);
        r.request_focus();
        // Not `lost_focus`: the box takes the focus back above.
        if crate::dlg::enter(ui, false) {
            result = Some(true);
        }
        ui.horizontal(|ui| {
            if ui.add(crate::look::primary(ui, "OK")).clicked() {
                result = Some(true);
            }
            if ui.button("Cancel").clicked() || crate::dlg::escape(ui) {
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
