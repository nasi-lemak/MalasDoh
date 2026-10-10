//! The built-in editor for remote files, with save review and conflict screens.

use egui::{Color32, RichText};
use malasdoh_core::diff::{self, Diff, LineKind};
use malasdoh_core::edit::{Access, EditSession};

pub enum State {
    Ready,
    Busy(String),
    /// Showing what will change before writing.
    Review(Diff),
    /// The server copy changed under us; the diff is theirs against ours.
    /// `merged` is both sets of changes combined, when they do not overlap.
    Conflict {
        diff: Diff,
        theirs: Vec<u8>,
        merged: Option<String>,
    },
    /// Saving failed with "permission denied"; offer sudo.
    NeedsSudo(String),
}

pub enum EditorAction {
    /// Write now. `force` overwrites a newer server copy.
    Save {
        force: bool,
    },
    Reload,
    RetryWithSudo,
}

pub struct Editor {
    pub id: u64,
    pub session: EditSession,
    pub text: String,
    pub access: Access,
    pub state: State,
    pub message: Option<(String, bool)>,
    /// Edits made in an external editor arrive here; true once uploaded.
    pub external: Option<std::path::PathBuf>,
    find: String,
    show_find: bool,
}

impl Editor {
    pub fn new(id: u64, session: EditSession, access: Access) -> Self {
        Self {
            id,
            text: session.base_text.clone(),
            session,
            access,
            state: State::Ready,
            message: None,
            external: None,
            find: String::new(),
            show_find: false,
        }
    }

    pub fn title(&self) -> String {
        let name = malasdoh_core::sftp::file_name(&self.session.path).to_string();
        let root = if self.access.is_sudo() { " (root)" } else { "" };
        let dirty = if self.is_modified() { " ●" } else { "" };
        format!("{name}{root}{dirty}")
    }

    pub fn is_modified(&self) -> bool {
        self.session.is_modified(&self.text)
    }

    /// Start a save, through review if the user wants one.
    pub fn request_save(&mut self, review: bool) -> Option<EditorAction> {
        if !self.is_modified() {
            self.message = Some(("No changes to save.".into(), false));
            return None;
        }
        if review {
            self.state = State::Review(diff::diff(&self.session.base_text, &self.text, 3));
            None
        } else {
            Some(EditorAction::Save { force: false })
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, font_size: f32, review: bool) -> Option<EditorAction> {
        let mut action = None;

        ui.horizontal(|ui| {
            ui.monospace(&self.session.path);
            ui.label(
                RichText::new(format!(
                    "{} · {}{}",
                    self.session.format.line_ending.label(),
                    if self.session.format.bom { "UTF-8 BOM" } else { "UTF-8" },
                    if self.access.is_sudo() { " · via sudo" } else { "" }
                ))
                .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = matches!(self.state, State::Busy(_));
                if ui.add_enabled(!busy, egui::Button::new("💾 Save")).on_hover_text("Ctrl+S").clicked() {
                    action = self.request_save(review);
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("⟳ Reload"))
                    .on_hover_text("Discard changes and load the server copy")
                    .clicked()
                {
                    action = Some(EditorAction::Reload);
                }
                if ui.button("🔍").on_hover_text("Find (Ctrl+F)").clicked() {
                    self.show_find = !self.show_find;
                }
            });
        });

        if let Some((msg, is_err)) = &self.message {
            let color = if *is_err { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
            ui.colored_label(color, msg);
        }
        if let State::Busy(what) = &self.state {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(what);
            });
        }

        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
            && !matches!(self.state, State::Busy(_))
        {
            action = self.request_save(review);
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            self.show_find = true;
        }

        if self.show_find {
            ui.horizontal(|ui| {
                ui.label("Find:");
                ui.text_edit_singleline(&mut self.find);
                let n = if self.find.is_empty() { 0 } else { self.text.matches(&self.find).count() };
                ui.label(format!("{n} match{}", if n == 1 { "" } else { "es" }));
                if ui.small_button("✖").clicked() {
                    self.show_find = false;
                }
            });
        }

        ui.separator();
        let find = if self.show_find { self.find.clone() } else { String::new() };
        let font = egui::FontId::monospace(font_size);
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = highlight(buf.as_str(), &find, &font, ui.visuals().text_color());
            job.wrap.max_width = wrap_width;
            ui.ctx().fonts_mut(|f| f.layout_job(job))
        };
        egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("editor", self.id)).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let lines = self.text.lines().count().max(1) + usize::from(self.text.ends_with('\n'));
                let numbers: String = (1..=lines).map(|n| format!("{n:>4}\n")).collect();
                ui.label(RichText::new(numbers).font(egui::FontId::monospace(font_size)).weak());
                ui.add(
                    egui::TextEdit::multiline(&mut self.text)
                        .code_editor()
                        .lock_focus(true)
                        .desired_width(f32::INFINITY)
                        .desired_rows(30)
                        .layouter(&mut layouter),
                );
            });
        });

        if let Some(a) = self.modal_ui(ui.ctx()) {
            action = Some(a);
        }
        action
    }

    fn modal_ui(&mut self, ctx: &egui::Context) -> Option<EditorAction> {
        let mut action = None;
        let mut close = false;
        match &self.state {
            State::Review(d) => {
                egui::Modal::new(egui::Id::new(("review", self.id))).show(ctx, |ui| {
                    ui.set_max_width(900.0);
                    ui.heading(format!("Save {}?", self.session.path));
                    ui.label(format!("+{} −{} lines", d.added, d.removed));
                    diff_ui(ui, d, egui::Id::new(("review-diff", self.id)));
                    ui.horizontal(|ui| {
                        if ui.button("💾 Save").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command)
                        {
                            action = Some(EditorAction::Save { force: false });
                        }
                        if ui.button("Copy as patch").clicked() {
                            ui.ctx().copy_text(diff::unified(&self.session.base_text, &self.text, &self.session.path));
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                    ui.label(RichText::new("The original is backed up on this computer before writing.").weak());
                });
            }
            State::Conflict { diff: d, theirs, merged } => {
                let mut take_merge = false;
                egui::Modal::new(egui::Id::new(("conflict", self.id))).show(ctx, |ui| {
                    ui.set_max_width(900.0);
                    ui.heading("⚠ The file changed on the server");
                    ui.label("Someone else saved this file after you opened it. Below: their version (−) against yours (+).");
                    diff_ui(ui, d, egui::Id::new(("conflict-diff", self.id)));
                    if merged.is_some() {
                        ui.colored_label(ui.visuals().text_color(), "Your edits and theirs touch different lines, so both can be kept.");
                    } else {
                        ui.colored_label(ui.visuals().warn_fg_color, "Your edits and theirs overlap; choose which version wins, or copy yours and redo the edit.");
                    }
                    ui.horizontal(|ui| {
                        if merged.is_some() && ui.button("Keep both changes").on_hover_text("Apply your edits on top of their version, then review").clicked() {
                            take_merge = true;
                        }
                        if ui.button("Overwrite with mine").on_hover_text("Their version is backed up first").clicked() {
                            action = Some(EditorAction::Save { force: true });
                        }
                        if ui.button("Copy my version").clicked() {
                            ui.ctx().copy_text(self.text.clone());
                        }
                        if ui.button("Discard mine, load theirs").clicked() {
                            action = Some(EditorAction::Reload);
                        }
                        if ui.button("Keep editing").clicked() {
                            close = true;
                        }
                    });
                });
                if take_merge {
                    let merged = merged.clone().unwrap();
                    match self.session.rebase(theirs) {
                        Ok(()) => {
                            self.text = merged;
                            self.state = State::Review(diff::diff(&self.session.base_text, &self.text, 3));
                            self.message = Some(("Merged onto the server's version; review and save.".into(), false));
                        }
                        Err(e) => {
                            self.state = State::Ready;
                            self.message = Some((format!("Cannot merge: {e:#}"), true));
                        }
                    }
                    return action;
                }
            }
            State::NeedsSudo(err) => {
                let err = err.clone();
                egui::Modal::new(egui::Id::new(("sudo", self.id))).show(ctx, |ui| {
                    ui.heading("Permission denied");
                    ui.label(err);
                    ui.label("Save through sudo instead? Your edits are kept either way.");
                    ui.horizontal(|ui| {
                        if ui.button("Save as root (sudo)").clicked() {
                            action = Some(EditorAction::RetryWithSudo);
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            State::Ready | State::Busy(_) => {}
        }
        if close {
            self.state = State::Ready;
        }
        action
    }
}

/// Plain text with search matches highlighted.
fn highlight(text: &str, find: &str, font: &egui::FontId, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let normal = egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    if find.is_empty() {
        job.append(text, 0.0, normal);
        return job;
    }
    let hit = egui::TextFormat { background: Color32::from_rgb(0x8a, 0x6d, 0x1f), ..normal.clone() };
    let mut rest = text;
    while let Some(i) = rest.find(find) {
        job.append(&rest[..i], 0.0, normal.clone());
        job.append(&rest[i..i + find.len()], 0.0, hit.clone());
        rest = &rest[i + find.len()..];
    }
    job.append(rest, 0.0, normal);
    job
}

pub fn diff_ui(ui: &mut egui::Ui, d: &Diff, id: egui::Id) {
    if d.is_empty() {
        ui.label("No differences.");
        return;
    }
    let add_bg = Color32::from_rgba_unmultiplied(60, 160, 60, 60);
    let del_bg = Color32::from_rgba_unmultiplied(200, 60, 60, 60);
    egui::ScrollArea::vertical().id_salt(id).max_height(500.0).auto_shrink([false, true]).show(ui, |ui| {
        for (i, h) in d.hunks.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.label(RichText::new(format!("@@ line {} @@", h.new_start)).weak().monospace());
            for l in &h.lines {
                let (sign, bg) = match l.kind {
                    LineKind::Context => (' ', Color32::TRANSPARENT),
                    LineKind::Added => ('+', add_bg),
                    LineKind::Removed => ('-', del_bg),
                };
                let num = l.new_no.or(l.old_no).map(|n| format!("{n:>5}")).unwrap_or_default();
                let text = RichText::new(format!("{num} {sign} {}", l.text)).monospace().background_color(bg);
                ui.label(text);
            }
        }
    });
}
