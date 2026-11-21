//! The local file pane: this computer's files next to the server's, for
//! WinSCP-style transfers by button or drag and drop.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use egui::{Align, Layout, RichText, Sense};

use crate::listsel::KeyAction;
use malasdoh_core::sftp::human_size;

/// Dragged from the local pane.
#[derive(Clone)]
pub struct LocalDrag(pub Vec<PathBuf>);

/// Dragged from a remote pane.
#[derive(Clone)]
pub struct RemoteDrag {
    pub session: crate::bridge::SessionId,
    pub paths: Vec<String>,
}

pub enum LocalAction {
    Upload(Vec<PathBuf>),
    /// Remote paths dropped here: download them into `dir`.
    Download {
        paths: Vec<String>,
        dir: PathBuf,
    },
}

/// What a delete in the background is doing; `Some` once it is over, with
/// the errors (empty: all gone).
type DeleteSlot = std::sync::Arc<std::sync::Mutex<Option<Vec<String>>>>;

/// A question the pane is asking.
enum Dialog {
    None,
    Rename {
        from: PathBuf,
        name: String,
    },
    NewFolder(String),
    /// Delete these; `error` says why moving them to the bin failed.
    Delete {
        paths: Vec<PathBuf>,
        error: Option<String>,
    },
}

/// Move `paths` to the bin; what went wrong, if anything.
#[cfg(feature = "desktop")]
fn to_bin(paths: &[PathBuf]) -> Vec<String> {
    match trash::delete_all(paths) {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!("Cannot move to the {BIN}: {e}")],
    }
}

/// No bin here: the dialog then offers to delete for good.
#[cfg(not(feature = "desktop"))]
fn to_bin(_paths: &[PathBuf]) -> Vec<String> {
    vec![format!("There is no {BIN} on this system")]
}

/// Where deleted files go, in this system's words.
#[cfg(windows)]
const BIN: &str = "Recycle Bin";
#[cfg(not(windows))]
const BIN: &str = "Trash";

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

pub struct LocalBrowser {
    pub path: PathBuf,
    input: String,
    entries: Vec<Entry>,
    error: Option<String>,
    sel: crate::listsel::Selection<PathBuf>,
    /// The list's scroll offset and height last frame.
    list_view: (f32, f32),
    filter: String,
    show_hidden: bool,
    dialog: Dialog,
    /// Folders visited, for Back, and left by Back, for Forward.
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// A delete running in the background, and what it deletes.
    deleting: Option<(DeleteSlot, Vec<PathBuf>)>,
}

/// On Windows, `canonicalize` gives `\\?\C:\...` (a "verbatim" path) and
/// `\\?\UNC\server\share`. Show and keep the ordinary forms people type.
fn without_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        // Only drive paths: other verbatim paths have no ordinary form.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

/// Where browsing starts: the home folder, or on a phone the app's own
/// documents folder (all it may see).
fn home() -> PathBuf {
    crate::dialogs::documents().unwrap_or_else(|| malasdoh_core::paths::expand_tilde("~"))
}

impl LocalBrowser {
    pub fn new(start: Option<PathBuf>) -> Self {
        let path = start.filter(|p| p.is_dir()).unwrap_or_else(home);
        let mut b = Self {
            input: path.display().to_string(),
            path,
            entries: Vec::new(),
            error: None,
            sel: Default::default(),
            list_view: (0.0, 0.0),
            filter: String::new(),
            show_hidden: false,
            dialog: Dialog::None,
            back: Vec::new(),
            forward: Vec::new(),
            deleting: None,
        };
        b.refresh();
        b
    }

    pub fn selection(&self) -> &[PathBuf] {
        &self.sel.selected
    }

    pub fn navigate(&mut self, to: PathBuf) {
        let here = self.path.clone();
        if self.go(to) && self.path != here {
            self.back.push(here);
            if self.back.len() > 50 {
                self.back.remove(0);
            }
            self.forward.clear();
        }
    }

    /// Back (or Forward, with `forward`) through the folders visited.
    fn history(&mut self, forward: bool) {
        let to = if forward { self.forward.pop() } else { self.back.pop() };
        let Some(to) = to else { return };
        let here = self.path.clone();
        if self.go(to.clone()) {
            if forward { self.back.push(here) } else { self.forward.push(here) }
        } else if forward {
            self.forward.push(to);
        } else {
            self.back.push(to);
        }
    }

    /// Show `to`; false if it cannot be read.
    fn go(&mut self, to: PathBuf) -> bool {
        match std::fs::read_dir(&to) {
            Ok(_) => {
                self.path = to.canonicalize().map(without_verbatim).unwrap_or(to);
                self.input = self.path.display().to_string();
                self.sel.clear();
                self.refresh();
                true
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", to.display()));
                false
            }
        }
    }

    pub fn refresh(&mut self) {
        self.error = None;
        let rd = match std::fs::read_dir(&self.path) {
            Ok(rd) => rd,
            Err(e) => {
                self.error = Some(format!("{}: {e}", self.path.display()));
                return;
            }
        };
        let mut entries: Vec<Entry> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some(Entry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    path: e.path(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                    modified: meta.modified().ok(),
                })
            })
            .collect();
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        self.entries = entries;
        self.sel.retain(|p| p.exists());
    }

    fn start_rename(&mut self, from: PathBuf) {
        let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.dialog = Dialog::Rename { from, name };
    }

    fn start_delete(&mut self) {
        if !self.sel.selected.is_empty() && self.deleting.is_none() {
            self.dialog = Dialog::Delete { paths: self.sel.selected.clone(), error: None };
        }
    }

    fn rename(&mut self, from: &Path, name: &str) -> Result<(), String> {
        let name = name.trim();
        check_name(name)?;
        let to = from.with_file_name(name);
        if to == from {
            return Ok(());
        }
        // Changing only the letter case finds the file itself on Windows and macOS.
        if to.symlink_metadata().is_ok() && !same_file(from, &to) {
            return Err(format!("{name} is already there"));
        }
        std::fs::rename(from, &to).map_err(|e| format!("Cannot rename {}: {e}", from.display()))?;
        self.refresh();
        self.sel.only(&to);
        Ok(())
    }

    fn new_folder(&mut self, name: &str) -> Result<(), String> {
        let name = name.trim();
        check_name(name)?;
        let to = self.path.join(name);
        std::fs::create_dir(&to).map_err(|e| format!("Cannot make {}: {e}", to.display()))?;
        self.refresh();
        self.sel.only(&to);
        Ok(())
    }

    /// Delete in the background: to the bin, or for good with `permanent`.
    fn delete(&mut self, paths: Vec<PathBuf>, permanent: bool) {
        let slot = DeleteSlot::default();
        self.deleting = Some((slot.clone(), paths.clone()));
        std::thread::spawn(move || {
            let errors = if permanent {
                paths
                    .iter()
                    .filter_map(|p| {
                        let r = if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                            std::fs::remove_dir_all(p)
                        } else {
                            std::fs::remove_file(p)
                        };
                        r.err().map(|e| format!("{}: {e}", p.display()))
                    })
                    .collect()
            } else {
                to_bin(&paths)
            };
            *slot.lock().unwrap() = Some(errors);
        });
    }

    fn poll_delete(&mut self, ctx: &egui::Context) {
        let Some((slot, paths)) = &self.deleting else { return };
        let Some(errors) = slot.lock().unwrap().take() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        // What is left can still go for good, from the same dialog.
        let left: Vec<PathBuf> = paths.iter().filter(|p| p.symlink_metadata().is_ok()).cloned().collect();
        self.deleting = None;
        self.refresh();
        if !errors.is_empty() {
            if left.is_empty() {
                self.error = Some(errors.join("\n"));
            } else {
                self.dialog = Dialog::Delete { paths: left, error: Some(errors.join("\n")) };
            }
        }
    }

    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut rename = None;
        let mut new_folder = None;
        let mut delete = None;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Rename { from, name } => {
                if let Some(ok) = crate::browser::name_dialog(ctx, "Rename", name) {
                    if ok {
                        rename = Some((from.clone(), name.clone()));
                    }
                    close = true;
                }
            }
            Dialog::NewFolder(name) => {
                if let Some(ok) = crate::browser::name_dialog(ctx, "New folder", name) {
                    if ok {
                        new_folder = Some(name.clone());
                    }
                    close = true;
                }
            }
            Dialog::Delete { paths, error } => {
                egui::Modal::new("local-delete".into()).show(ctx, |ui| {
                    crate::dlg::fit_modal(ui);
                    ui.set_max_width(520.0);
                    ui.heading(if error.is_some() {
                        "Delete for good?".to_string()
                    } else {
                        format!("Move to the {BIN}?")
                    });
                    for p in paths.iter().take(10) {
                        ui.monospace(p.display().to_string());
                    }
                    if paths.len() > 10 {
                        ui.label(format!("…and {} more", paths.len() - 10));
                    }
                    if let Some(e) = error {
                        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                        ui.label("Deleting them for good cannot be undone; folders go with everything in them.");
                    } else {
                        ui.label(RichText::new(format!("From the {BIN} you can put them back.")).weak());
                    }
                    ui.horizontal(|ui| {
                        if error.is_none() {
                            if ui
                                .add(crate::look::primary(ui, format!("Move to the {BIN}")))
                                .on_hover_text("Enter")
                                .clicked()
                                || crate::dlg::enter(ui, false)
                            {
                                delete = Some((std::mem::take(paths), false));
                            }
                            if ui
                                .button(RichText::new("Delete for good").color(ui.visuals().error_fg_color))
                                .on_hover_text("Cannot be undone")
                                .clicked()
                            {
                                delete = Some((std::mem::take(paths), true));
                            }
                        } else if ui
                            .add(crate::look::danger(ui, "Delete for good"))
                            .on_hover_text("Cannot be undone")
                            .clicked()
                        {
                            delete = Some((std::mem::take(paths), true));
                        }
                        if ui.button("Cancel").on_hover_text("Esc").clicked() || crate::dlg::escape(ui) {
                            close = true;
                        }
                    });
                });
            }
        }
        if let Some((from, name)) = rename
            && let Err(e) = self.rename(&from, &name)
        {
            self.error = Some(e);
        }
        if let Some(name) = new_folder
            && let Err(e) = self.new_folder(&name)
        {
            self.error = Some(e);
        }
        if let Some((paths, permanent)) = delete {
            self.delete(paths, permanent);
            close = true;
        }
        if close {
            self.dialog = Dialog::None;
        }
    }

    fn hidden(name: &str, path: &Path) -> bool {
        if name.starts_with('.') {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const HIDDEN: u32 = 0x2;
            std::fs::metadata(path).is_ok_and(|m| m.file_attributes() & HIDDEN != 0)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            false
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, session: crate::bridge::SessionId) -> Vec<LocalAction> {
        let mut actions = Vec::new();
        ui.horizontal(|ui| {
            let here = if crate::share::available() { "This device" } else { "This computer" };
            ui.label(RichText::new(here).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Phones: one file selected can go to another app.
                if crate::share::available()
                    && let [one] = self.sel.selected.as_slice()
                    && one.is_file()
                    && ui.button("Share…").on_hover_text("Send it to another app").clicked()
                    && let Err(e) = crate::share::share(one)
                {
                    self.error = Some(format!("Could not share {}: {e}", one.display()));
                }
                if !self.sel.selected.is_empty()
                    && ui.button("Upload ⏵").on_hover_text("Upload the selected items to the server folder").clicked()
                {
                    actions.push(LocalAction::Upload(self.sel.selected.clone()));
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.back.is_empty(), egui::Button::new("⬅")).on_hover_text("Back (Alt+Left)").clicked()
            {
                self.history(false);
            }
            if !self.forward.is_empty() && ui.button("➡").on_hover_text("Forward (Alt+Right)").clicked() {
                self.history(true);
            }
            if ui.button("⬆").on_hover_text("Parent folder (Alt+Up or Backspace)").clicked()
                && let Some(p) = self.path.parent().map(Path::to_path_buf)
            {
                self.navigate(p);
            }
            if ui.button("⟳").on_hover_text("Refresh (F5)").clicked() {
                self.refresh();
            }
            if ui.button("~").on_hover_text("Home folder").clicked() {
                self.navigate(home());
            }
            #[cfg(windows)]
            ui.menu_button("Drives", |ui| {
                for letter in 'A'..='Z' {
                    let root = PathBuf::from(format!("{letter}:\\"));
                    if root.exists() && ui.button(format!("{letter}:")).clicked() {
                        self.navigate(root);
                        ui.close();
                    }
                }
            });
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .id(egui::Id::new(("local-path", session)))
                    .desired_width(f32::INFINITY),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let to = malasdoh_core::paths::expand_tilde(self.input.trim());
                self.navigate(to);
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .id(egui::Id::new(("local-filter", session)))
                    .hint_text("Filter (Ctrl+F)")
                    .desired_width(120.0),
            );
            ui.checkbox(&mut self.show_hidden, "Hidden");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("+ Folder").on_hover_text("New folder (Ctrl+Shift+N)").clicked() {
                    self.dialog = Dialog::NewFolder(String::new());
                }
            });
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.separator();

        let filter = self.filter.to_lowercase();
        let visible: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (self.show_hidden || !Self::hidden(&e.name, &e.path))
                    && (filter.is_empty() || e.name.to_lowercase().contains(&filter))
            })
            .collect();
        // Rows a finger can hit on a touch screen.
        let pad = if crate::look::is_touch(ui.ctx()) { 18.0 } else { 4.0 };
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + pad;
        let mut open_dir = None;
        // The list's keys (once it was clicked): Ctrl+A, arrows, Enter on a folder, Backspace.
        let order: Vec<PathBuf> = visible.iter().map(|&i| self.entries[i].path.clone()).collect();
        let (key, scroll_to) = self
            .sel
            .keys(ui.ctx(), &order, |p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        match key {
            Some(KeyAction::Open(p)) if p.is_dir() => open_dir = Some(p),
            Some(KeyAction::Parent) => {
                if let Some(up) = self.path.parent() {
                    open_dir = Some(up.to_path_buf());
                }
            }
            Some(KeyAction::Rename(p)) => self.start_rename(p),
            Some(KeyAction::Delete) => self.start_delete(),
            Some(KeyAction::Back) => self.history(false),
            Some(KeyAction::Forward) => self.history(true),
            Some(KeyAction::Refresh) => self.refresh(),
            Some(KeyAction::FocusPath) => {
                self.sel.focused = false;
                crate::shortcut::focus_select(
                    ui.ctx(),
                    egui::Id::new(("local-path", session)),
                    self.input.chars().count(),
                );
            }
            Some(KeyAction::FocusFilter) => {
                self.sel.focused = false;
                crate::shortcut::focus_select(
                    ui.ctx(),
                    egui::Id::new(("local-filter", session)),
                    self.filter.chars().count(),
                );
            }
            Some(KeyAction::NewFolder) => self.dialog = Dialog::NewFolder(String::new()),
            Some(KeyAction::Copy) => {
                let paths: Vec<String> = self.sel.selected.iter().map(|p| p.display().to_string()).collect();
                ui.ctx().copy_text(paths.join("\n"));
            }
            _ => {}
        }
        let mut area = egui::ScrollArea::vertical().id_salt(("local", session)).auto_shrink([false, false]);
        if let Some(i) = scroll_to
            && let Some(y) = crate::listsel::scroll_for(i, row_h, self.list_view.0, self.list_view.1)
        {
            area = area.vertical_scroll_offset(y);
        }
        let pane = area.show_rows(ui, row_h, visible.len(), |ui, range| {
            for &i in &visible[range] {
                let e = &self.entries[i];
                let selected = self.sel.contains(&e.path);
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click_and_drag());
                if selected {
                    ui.painter().rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let inner = rect.shrink2(egui::vec2(6.0, 0.0));
                let body = egui::TextStyle::Body.resolve(ui.style());
                let weak = ui.visuals().weak_text_color();
                let mut right = inner.right();
                if !e.is_dir {
                    let r = ui.painter().text(
                        egui::pos2(right, inner.center().y),
                        egui::Align2::RIGHT_CENTER,
                        human_size(e.size),
                        body.clone(),
                        weak,
                    );
                    right = r.left() - 10.0;
                }
                let color =
                    if e.is_dir || selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
                let icon = if e.is_dir { "🗀" } else { "🗋" };
                ui.painter_at(egui::Rect::from_min_max(inner.min, egui::pos2(right, inner.max.y))).text(
                    egui::pos2(inner.left(), inner.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("{icon}  {}", e.name),
                    body,
                    color,
                );
                if resp.clicked() {
                    let (ctrl, shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));
                    self.sel.click(&order, &e.path, ctrl, shift);
                }
                // A touch screen has no double click: a tap opens.
                if (resp.double_clicked() || (resp.clicked() && crate::look::is_touch(ui.ctx()))) && e.is_dir {
                    open_dir = Some(e.path.clone());
                }
                if resp.drag_started() && !selected {
                    self.sel.only(&e.path);
                }
                resp.dnd_set_drag_payload(LocalDrag(if selected {
                    self.sel.selected.clone()
                } else {
                    vec![e.path.clone()]
                }));
                if resp.secondary_clicked() && !selected {
                    self.sel.only(&e.path);
                }
                let path = e.path.clone();
                let is_dir = e.is_dir;
                let resp = resp.on_hover_text(format!("{}\n{}", e.path.display(), modified(e.modified)));
                self.sel.menu_popup(&resp, &path).show(|ui| {
                    let n = self.sel.selected.len();
                    if is_dir && n == 1 && ui.button("Open").clicked() {
                        open_dir = Some(path.clone());
                        ui.close();
                    }
                    if ui.button("Upload").clicked() {
                        actions.push(LocalAction::Upload(self.sel.selected.clone()));
                        ui.close();
                    }
                    // Phones: hand the file to another app (mail, a viewer…).
                    if !is_dir && n == 1 && crate::share::available() && ui.button("Share…").clicked() {
                        if let Err(e) = crate::share::share(&path) {
                            self.error = Some(format!("Could not share {}: {e}", path.display()));
                        }
                        ui.close();
                    }
                    if ui.add(egui::Button::new("Copy path").shortcut_text("Ctrl+C")).clicked() {
                        let paths: Vec<String> = self.sel.selected.iter().map(|p| p.display().to_string()).collect();
                        ui.ctx().copy_text(paths.join("\n"));
                        ui.close();
                    }
                    if n == 1 && ui.add(egui::Button::new("Rename…").shortcut_text("F2")).clicked() {
                        self.start_rename(path.clone());
                        ui.close();
                    }
                    ui.separator();
                    let label = if n > 1 { format!("Delete {n} items…") } else { "Delete…".into() };
                    if ui.add(egui::Button::new(label).shortcut_text("Del")).clicked() {
                        self.start_delete();
                        ui.close();
                    }
                });
            }
        });
        self.list_view = (pane.state.offset.y, pane.inner_rect.height());
        self.sel.track_focus(ui, pane.inner_rect);
        if let Some(d) = open_dir {
            self.navigate(d);
        }
        self.dialog_ui(ui.ctx());
        self.poll_delete(ui.ctx());
        // Remote items dropped onto this pane download here.
        let drop = ui.interact(pane.inner_rect, egui::Id::new(("local-drop", session)), Sense::hover());
        if drop.dnd_hover_payload::<RemoteDrag>().is_some() {
            ui.painter().rect_stroke(pane.inner_rect, 4.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
        }
        if let Some(p) = drop.dnd_release_payload::<RemoteDrag>()
            && p.session == session
        {
            actions.push(LocalAction::Download { paths: p.paths.clone(), dir: self.path.clone() });
        }
        actions
    }
}

/// How long ago a file changed, for its tooltip.
fn modified(t: Option<SystemTime>) -> String {
    let Some(d) = t.and_then(|t| SystemTime::now().duration_since(t).ok()) else { return String::new() };
    match d.as_secs() {
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

/// Do both paths name the same file (letter case aside, where it does not count)?
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (a.symlink_metadata(), b.symlink_metadata()) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }
}

/// A new name for a file in the same folder.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("Give it a name".into());
    }
    if name.contains(['/', '\\']) {
        return Err("A name cannot contain / or \\".into());
    }
    #[cfg(windows)]
    if name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.ends_with(['.', ' ']) {
        return Err("Windows does not allow < > : \" | ? * in a name, or a name ending in a dot or space".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_windows_paths_read_normally() {
        let p = |s: &str| without_verbatim(PathBuf::from(s)).to_string_lossy().into_owned();
        assert_eq!(p(r"\\?\Z:\tmp\files"), r"Z:\tmp\files");
        assert_eq!(p(r"\\?\UNC\nas\share\dir"), r"\\nas\share\dir");
        assert_eq!(p(r"\\?\Volume{abc}\x"), r"\\?\Volume{abc}\x");
        assert_eq!(p("/home/me"), "/home/me");
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("notes.txt").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("a\\b").is_err());
    }

    #[test]
    fn rename_keeps_other_files() {
        let dir = std::env::temp_dir().join(format!("malasdoh-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let mut b = LocalBrowser::new(Some(dir.clone()));
        assert!(b.rename(&dir.join("a.txt"), "b.txt").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b");
        b.rename(&dir.join("a.txt"), " c.txt ").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "a");
        assert_eq!(b.selection(), [dir.join("c.txt")]);
        // Changing only the case renames it, and never takes another file's place.
        b.rename(&dir.join("c.txt"), "C.txt").unwrap();
        assert!(dir.join("C.txt").exists());
        b.delete(vec![dir.join("C.txt"), dir.join("b.txt")], true);
        let ctx = egui::Context::default();
        while b.deleting.is_some() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            b.poll_delete(&ctx);
        }
        assert!(b.entries.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
