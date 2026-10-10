//! The local file pane: this computer's files next to the server's, for
//! WinSCP-style transfers by button or drag and drop.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use egui::{Align, Layout, RichText, Sense};
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
    selected: Vec<PathBuf>,
    filter: String,
    show_hidden: bool,
}

fn home() -> PathBuf {
    malasdoh_core::paths::expand_tilde("~")
}

impl LocalBrowser {
    pub fn new(start: Option<PathBuf>) -> Self {
        let path = start.filter(|p| p.is_dir()).unwrap_or_else(home);
        let mut b = Self {
            input: path.display().to_string(),
            path,
            entries: Vec::new(),
            error: None,
            selected: Vec::new(),
            filter: String::new(),
            show_hidden: false,
        };
        b.refresh();
        b
    }

    pub fn navigate(&mut self, to: PathBuf) {
        match std::fs::read_dir(&to) {
            Ok(_) => {
                self.path = to.canonicalize().unwrap_or(to);
                self.input = self.path.display().to_string();
                self.selected.clear();
                self.refresh();
            }
            Err(e) => self.error = Some(format!("{}: {e}", to.display())),
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
        self.selected.retain(|p| p.exists());
    }

    fn hidden(name: &str, path: &Path) -> bool {
        if name.starts_with('.') {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const HIDDEN: u32 = 0x2;
            return std::fs::metadata(path).map(|m| m.file_attributes() & HIDDEN != 0).unwrap_or(false);
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
            ui.label(RichText::new("This computer").strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.selected.is_empty()
                    && ui.button("Upload ⏵").on_hover_text("Upload the selected items to the server folder").clicked()
                {
                    actions.push(LocalAction::Upload(self.selected.clone()));
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.button("⬆").on_hover_text("Parent folder").clicked()
                && let Some(p) = self.path.parent().map(Path::to_path_buf)
            {
                self.navigate(p);
            }
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
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
            let r = ui.add(egui::TextEdit::singleline(&mut self.input).desired_width(f32::INFINITY));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let to = malasdoh_core::paths::expand_tilde(self.input.trim());
                self.navigate(to);
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter").desired_width(120.0));
            ui.checkbox(&mut self.show_hidden, "Hidden");
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
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let mut open_dir = None;
        let pane = egui::ScrollArea::vertical().id_salt(("local", session)).auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            visible.len(),
            |ui, range| {
                for &i in &visible[range] {
                    let e = &self.entries[i];
                    let selected = self.selected.contains(&e.path);
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
                        if ui.input(|i| i.modifiers.command || i.modifiers.ctrl) {
                            if selected {
                                self.selected.retain(|p| p != &e.path);
                            } else {
                                self.selected.push(e.path.clone());
                            }
                        } else {
                            self.selected = vec![e.path.clone()];
                        }
                    }
                    if resp.double_clicked() && e.is_dir {
                        open_dir = Some(e.path.clone());
                    }
                    if resp.drag_started() && !selected {
                        self.selected = vec![e.path.clone()];
                    }
                    resp.dnd_set_drag_payload(LocalDrag(if selected {
                        self.selected.clone()
                    } else {
                        vec![e.path.clone()]
                    }));
                    let modified = e.modified.and_then(|t| {
                        let d = SystemTime::now().duration_since(t).ok()?;
                        Some(match d.as_secs() {
                            s if s < 3600 => format!("{} min ago", s / 60),
                            s if s < 86400 => format!("{} h ago", s / 3600),
                            s => format!("{} days ago", s / 86400),
                        })
                    });
                    resp.on_hover_text(format!("{}\n{}", e.path.display(), modified.unwrap_or_default()));
                }
            },
        );
        if let Some(d) = open_dir {
            self.navigate(d);
        }
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
