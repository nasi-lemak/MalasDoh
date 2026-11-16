//! "Replace files?": before a transfer replaces files, the list of them
//! with what differs, the changes of a text file side by side, and a tick
//! for each.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::RichText;
use malasdoh_core::diff::Diff;
use malasdoh_core::replace::{BACKUP_MAX, Replaced, Texts};
use malasdoh_core::sftp::human_size;
use malasdoh_core::transfer::{Job, Way};

/// Both versions of a file and their diff, once loaded.
pub type TextSlot = Arc<Mutex<Option<Result<(Texts, Diff), String>>>>;

/// A planned transfer waiting for the user to choose what it replaces.
pub struct Review {
    /// The transfer's row in the queue.
    pub id: u64,
    pub job: Job,
    /// Where it goes, for the heading.
    pub place: String,
    pub replaced: Vec<Replaced>,
    pub tick: Vec<bool>,
    /// Back up the server's copies first (uploads with the setting on).
    pub backup: bool,
    /// Which row's changes are shown, by its index in `replaced`.
    showing: Option<usize>,
    texts: HashMap<usize, TextSlot>,
}

pub enum ReviewAction {
    /// Load the texts of this row (its index in `replaced`).
    Load(usize, TextSlot),
    Go,
    Cancel,
}

impl Review {
    pub fn new(id: u64, job: Job, place: String, mut replaced: Vec<Replaced>, backup: bool) -> Self {
        // Those that differ first, identical ones last.
        replaced.sort_by_key(|r| (r.same.map_or(1, |s| if s { 2 } else { 0 }), r.rel.clone()));
        // Same contents: nothing to copy. Everything else is replaced unless unticked.
        let tick = replaced.iter().map(|r| r.same != Some(true)).collect();
        Self { id, job, place, replaced, tick, backup, showing: None, texts: HashMap::new() }
    }

    /// The ticked files, and the rest with why they are left out.
    pub fn choice(&self) -> (Vec<Replaced>, Vec<(usize, String)>) {
        let mut keep = Vec::new();
        let mut drop = Vec::new();
        for (r, &t) in self.replaced.iter().zip(&self.tick) {
            if r.same == Some(true) {
                drop.push((r.index, crate::queue::SAME.trim_matches(['(', ')']).to_string()));
            } else if t {
                keep.push(r.clone());
            } else {
                drop.push((r.index, "already there, kept as it was".to_string()));
            }
        }
        (keep, drop)
    }

    pub fn ui(&mut self, ctx: &egui::Context, id: egui::Id) -> Option<ReviewAction> {
        let mut action = None;
        let mut open = true;
        let same = self.replaced.iter().filter(|r| r.same == Some(true)).count();
        let differ = self.replaced.iter().filter(|r| r.same == Some(false)).count();
        let unknown = self.replaced.len() - same - differ;
        let new_files = self.job.files.len() - self.replaced.len();
        egui::Window::new("Replace files?")
            .id(id)
            .open(&mut open)
            .default_width(760.0)
            .default_height(560.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("{} file(s) are already in {}", self.replaced.len(), self.place)).strong(),
                );
                let mut parts = Vec::new();
                if differ > 0 {
                    parts.push(format!("different: {differ}"));
                }
                if same > 0 {
                    parts.push(format!("identical, not copied again: {same}"));
                }
                if unknown > 0 {
                    parts.push(format!("not compared (large, or no checksum tool on the server): {unknown}"));
                }
                if new_files > 0 {
                    parts.push(format!("new, copied as well: {new_files}"));
                }
                ui.label(RichText::new(parts.join(" · ")).weak());
                ui.horizontal(|ui| {
                    if ui.small_button("Tick all").clicked() {
                        for (t, r) in self.tick.iter_mut().zip(&self.replaced) {
                            *t = r.same != Some(true);
                        }
                    }
                    if ui.small_button("Untick all").clicked() {
                        self.tick.iter_mut().for_each(|t| *t = false);
                    }
                });
                // Only the rows in view are drawn: a folder can replace thousands.
                let row_h = ui.spacing().interact_size.y + 2.0;
                let n_rows = self.replaced.len();
                egui::ScrollArea::vertical().id_salt(("replace-list", self.id)).max_height(200.0).show_rows(
                    ui,
                    row_h,
                    n_rows,
                    |ui, rows| {
                        for i in rows {
                            let r = &self.replaced[i];
                            ui.horizontal(|ui| {
                                ui.set_min_height(row_h - 2.0);
                                let same = r.same == Some(true);
                                if same {
                                    ui.add_enabled(false, egui::Checkbox::without_text(&mut false));
                                } else {
                                    ui.checkbox(&mut self.tick[i], "").on_hover_text("Replace this file");
                                }
                                let name = RichText::new(&r.rel).monospace();
                                cell(
                                    ui,
                                    [260.0, row_h - 2.0],
                                    egui::Label::new(if same { name.weak() } else { name }).truncate(),
                                )
                                .on_hover_text(&r.rel);
                                if same {
                                    cell(
                                        ui,
                                        [150.0, row_h - 2.0],
                                        egui::Label::new(RichText::new("same content").weak()),
                                    );
                                } else {
                                    cell(
                                        ui,
                                        [150.0, row_h - 2.0],
                                        egui::Label::new(format!(
                                            "{}, new {}",
                                            human_size(r.old_size),
                                            human_size(r.new_size)
                                        )),
                                    );
                                }
                                if !same && r.can_show() {
                                    let shown = self.showing == Some(i);
                                    if ui.selectable_label(shown, "Show changes").clicked() {
                                        if shown {
                                            self.showing = None;
                                        } else {
                                            self.showing = Some(i);
                                            if let std::collections::hash_map::Entry::Vacant(e) = self.texts.entry(i) {
                                                let slot = TextSlot::default();
                                                e.insert(slot.clone());
                                                action = Some(ReviewAction::Load(i, slot));
                                            }
                                        }
                                    }
                                } else if !same {
                                    ui.label(RichText::new("too big to show").weak().small());
                                }
                                match (r.old_mtime, r.new_mtime) {
                                    (Some(o), Some(n)) if o > n + 1 && !same => {
                                        ui.colored_label(ui.visuals().warn_fg_color, "the current copy is newer")
                                            .on_hover_text(when(o, n));
                                    }
                                    (Some(o), Some(n)) if !same => {
                                        ui.label(RichText::new(when(o, n)).weak().small());
                                    }
                                    _ => {}
                                }
                            });
                        }
                    },
                );
                if let Some(i) = self.showing {
                    ui.separator();
                    ui.label(RichText::new(format!("{}: what replacing it changes", self.replaced[i].rel)).strong());
                    match self.texts.get(&i).and_then(|s| s.lock().unwrap().clone()) {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Reading both copies…");
                            });
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().warn_fg_color, format!("Cannot show it: {e}"));
                        }
                        Some(Ok((texts, diff))) => {
                            if let Some(n) = &texts.note {
                                ui.colored_label(ui.visuals().warn_fg_color, n);
                            }
                            if diff.is_empty() {
                                ui.label("The text is the same.");
                            } else {
                                ui.label(
                                    RichText::new(format!(
                                        "+{} -{} lines (- is in the current copy, + in the one replacing it)",
                                        diff.added, diff.removed
                                    ))
                                    .weak(),
                                );
                                crate::editor::diff_ui_max(
                                    ui,
                                    &diff,
                                    egui::Id::new(("replace-diff", self.id, i)),
                                    300.0,
                                );
                            }
                        }
                    }
                }
                ui.separator();
                let note = match (self.job.way, self.backup) {
                    (Way::Up, true) => format!(
                        "The server's copies are backed up first (up to {} each): Backups… on a file shows them, \
                         and text files go into the change journal.",
                        human_size(BACKUP_MAX)
                    ),
                    (Way::Up, false) => "The server's copies are not backed up (Settings > Transfers).".into(),
                    (Way::Down, _) => "The copies on this computer are replaced.".into(),
                };
                ui.label(RichText::new(note).weak().small());
                let n = self.tick.iter().zip(&self.replaced).filter(|(t, r)| **t && r.same != Some(true)).count();
                ui.horizontal(|ui| {
                    let label = match (n, new_files) {
                        (0, 0) => "Copy nothing".to_string(),
                        (0, m) => format!("Copy only the {m} new file(s)"),
                        (n, 0) => format!("Replace {n} file(s)"),
                        (n, m) => format!("Replace {n} and copy {m} new"),
                    };
                    if ui.button(RichText::new(label).strong()).on_hover_text("Enter").clicked()
                        || crate::dlg::window_enter(ui, false)
                    {
                        action = Some(ReviewAction::Go);
                    }
                    if ui.button("Cancel").on_hover_text("Esc").clicked() || crate::dlg::window_escape(ui) {
                        action = Some(ReviewAction::Cancel);
                    }
                });
            });
        if !open {
            action = Some(ReviewAction::Cancel);
        }
        action
    }
}

/// When the two copies were last changed, relative to each other.
fn when(old: u64, new: u64) -> String {
    let fmt = |t: u64| malasdoh_core::journal::format_time(t as i64);
    if old.abs_diff(new) <= 1 { "same time".into() } else { format!("current: {} · new: {}", fmt(old), fmt(new)) }
}

/// A column of the list: `size` (width, height), left-aligned.
fn cell(ui: &mut egui::Ui, size: [f32; 2], label: egui::Label) -> egui::Response {
    ui.allocate_ui_with_layout(size.into(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(size[0]);
        ui.add(label)
    })
    .inner
}
