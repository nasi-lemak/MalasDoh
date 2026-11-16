//! Apply a patch (`diff -u`, `git diff`) to files on the server: check it
//! first (every hunk must match), see each file's change, then save them
//! one by one the safe way (conflict check, backup, config check, journal).

use egui::RichText;
use malasdoh_core::diff;
use malasdoh_core::edit::EditSession;

pub struct FileCheck {
    /// The path as written in the patch.
    pub patch_path: String,
    /// Where it lands on the server.
    pub remote: String,
    pub status: Status,
}

pub enum Status {
    Ready {
        session: Box<EditSession>,
        new_text: String,
        added: usize,
        removed: usize,
    },
    /// A new file, with its contents.
    New {
        text: String,
    },
    Already,
    Failed(String),
    /// Not done by design (deleting files).
    Skipped(String),
}

impl FileCheck {
    pub fn will_change(&self) -> bool {
        matches!(self.status, Status::Ready { .. } | Status::New { .. })
    }
}

pub enum PatchAction {
    Check { text: String, base: String, strip: usize, sudo: bool },
    Apply,
}

pub struct PatchWindow {
    pub open: bool,
    text: String,
    pub base: String,
    strip: usize,
    pub sudo: bool,
    pub busy: bool,
    pub checked: Option<Result<Vec<FileCheck>, String>>,
    /// Per file, after applying: what happened.
    pub applied: Vec<(String, Result<String, String>)>,
    expanded: Option<usize>,
    error: Option<String>,
}

impl Default for PatchWindow {
    fn default() -> Self {
        Self {
            open: false,
            text: String::new(),
            base: "/".into(),
            strip: 0,
            sudo: false,
            busy: false,
            checked: None,
            applied: Vec::new(),
            expanded: None,
            error: None,
        }
    }
}

/// Where a patch path goes: absolute paths as they are, others under `base`
/// after dropping `strip` leading parts.
pub fn remote_path(base: &str, patch_path: &str, strip: usize) -> String {
    if patch_path.starts_with('/') && strip == 0 {
        return patch_path.to_string();
    }
    let rest: Vec<&str> = patch_path.split('/').filter(|p| !p.is_empty()).skip(strip).collect();
    malasdoh_core::sftp::join(base, &rest.join("/"))
}

impl PatchWindow {
    pub fn show(&mut self, dir: &str, root: bool) {
        if !self.open {
            self.base = dir.to_string();
            self.sudo = root;
        }
        self.open = true;
    }

    pub fn ui(&mut self, ctx: &egui::Context, session: u64) -> Option<PatchAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = self.open;
        let shown = egui::Window::new("Apply a patch")
            .id(egui::Id::new(("patch", session)))
            .open(&mut open)
            .default_size([760.0, 560.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Patch (diff -u or git diff):");
                    if ui.button("Open file…").clicked()
                        && let Some(f) = rfd::FileDialog::new().add_filter("Patch", &["patch", "diff", "txt"]).pick_file()
                    {
                        match std::fs::read_to_string(&f) {
                            Ok(t) => {
                                self.text = t;
                                self.checked = None;
                                self.applied.clear();
                                self.error = None;
                            }
                            Err(e) => self.error = Some(format!("{}: {e}", f.display())),
                        }
                    }
                });
                egui::ScrollArea::vertical().id_salt("patch-text").max_height(160.0).show(ui, |ui| {
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut self.text)
                            .code_editor()
                            .desired_rows(6)
                            .desired_width(f32::INFINITY)
                            .hint_text("Paste a patch here"),
                    );
                    if r.changed() {
                        self.checked = None;
                        self.applied.clear();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Paths are relative to");
                    ui.add(egui::TextEdit::singleline(&mut self.base).desired_width(220.0));
                    ui.label("strip");
                    ui.add(egui::DragValue::new(&mut self.strip).range(0..=6))
                        .on_hover_text("Leading folders to drop from the paths in the patch (like patch -p). a/ and b/ are dropped already.");
                    ui.checkbox(&mut self.sudo, "As root (sudo)");
                });
                if let Some(e) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.horizontal(|ui| {
                    let can = !self.busy && !self.text.trim().is_empty();
                    if ui.add_enabled(can, egui::Button::new("Check")).on_hover_text("Read each file and try the patch, without writing").clicked() {
                        self.busy = true;
                        self.applied.clear();
                        self.expanded = None;
                        action = Some(PatchAction::Check {
                            text: self.text.clone(),
                            base: self.base.trim().to_string(),
                            strip: self.strip,
                            sudo: self.sudo,
                        });
                    }
                    let ready = match &self.checked {
                        Some(Ok(v)) => v.iter().filter(|f| f.will_change()).count(),
                        _ => 0,
                    };
                    let all_ok = matches!(&self.checked, Some(Ok(v)) if v.iter().all(|f| !matches!(f.status, Status::Failed(_))));
                    let label = format!("Apply to {ready} file(s)");
                    let b = ui.add_enabled(!self.busy && ready > 0 && all_ok && self.applied.is_empty(), egui::Button::new(label));
                    let b = if !all_ok && ready > 0 {
                        b.on_disabled_hover_text("Some files do not match; nothing is applied until all do")
                    } else {
                        b
                    };
                    if b.clicked() {
                        self.busy = true;
                        action = Some(PatchAction::Apply);
                    }
                    if self.busy {
                        ui.spinner();
                    }
                });
                ui.separator();
                self.results_ui(ui);
            });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        action
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        match &self.checked {
            None => {
                ui.label(
                    RichText::new("Check the patch to see what it changes. Nothing is written until you apply.").weak(),
                );
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(files)) => {
                let ok = egui::Color32::from_rgb(90, 190, 90);
                let warn = ui.visuals().warn_fg_color;
                let err = ui.visuals().error_fg_color;
                let mut toggle = None;
                egui::ScrollArea::vertical().id_salt("patch-files").auto_shrink([false, false]).show(ui, |ui| {
                    for (i, f) in files.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let (text, color) = match &f.status {
                                Status::Ready { added, removed, .. } => (format!("ready  +{added} -{removed}"), ok),
                                Status::New { text } => {
                                    let n = text.lines().count();
                                    (format!("new file, {n} line{}", if n == 1 { "" } else { "s" }), ok)
                                }
                                Status::Already => ("already applied".to_string(), ui.visuals().weak_text_color()),
                                Status::Failed(m) => (m.clone(), err),
                                Status::Skipped(m) => (m.clone(), warn),
                            };
                            let can_show = matches!(f.status, Status::Ready { .. } | Status::New { .. });
                            if can_show
                                && ui.small_button(if self.expanded == Some(i) { "Hide" } else { "Show" }).clicked()
                            {
                                toggle = Some(i);
                            }
                            ui.monospace(&f.remote).on_hover_text(format!("In the patch: {}", f.patch_path));
                            ui.colored_label(color, text);
                            if let Some((_, r)) = self.applied.iter().find(|(p, _)| *p == f.remote) {
                                match r {
                                    Ok(m) => ui.colored_label(ok, format!("✔ {m}")),
                                    Err(m) => ui.colored_label(err, format!("✖ {m}")),
                                };
                            }
                        });
                        if self.expanded == Some(i) {
                            let d = match &f.status {
                                Status::Ready { session, new_text, .. } => diff::diff(&session.base_text, new_text, 3),
                                Status::New { text } => diff::diff("", text, 3),
                                _ => Default::default(),
                            };
                            crate::editor::diff_ui(ui, &d, egui::Id::new(("patch-diff", i)));
                        }
                    }
                });
                if let Some(i) = toggle {
                    self.expanded = if self.expanded == Some(i) { None } else { Some(i) };
                }
                if !self.applied.is_empty() && self.applied.iter().any(|(_, r)| r.is_err()) {
                    ui.colored_label(
                        err,
                        "Stopped at the first file that could not be saved; the files before it are changed (each has a backup and a journal entry).",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_patch_paths_land() {
        assert_eq!(remote_path("/", "etc/nginx/nginx.conf", 0), "/etc/nginx/nginx.conf");
        assert_eq!(remote_path("/srv/app", "config/app.toml", 0), "/srv/app/config/app.toml");
        assert_eq!(remote_path("/srv/app", "repo/config/app.toml", 1), "/srv/app/config/app.toml");
        assert_eq!(remote_path("/x", "/etc/hosts", 0), "/etc/hosts");
    }
}
