//! The change journal window: every file saved from MalasDoh, searchable,
//! with its patch.

use egui::RichText;
use malasdoh_core::journal::{self, Entry, Journal};

use crate::compare::Side;
use crate::session::{ConnState, Session};

#[derive(Default)]
pub struct JournalWindow {
    pub open: bool,
    entries: Vec<Entry>,
    error: Option<String>,
    pub filter: String,
    problems_only: bool,
    selected: Option<usize>,
}

impl JournalWindow {
    /// Open (or bring up) the window, re-reading the journal.
    pub fn show(&mut self, journal: &Journal, filter: Option<String>) {
        self.open = true;
        if let Some(f) = filter {
            self.filter = f;
        }
        self.reload(journal);
    }

    fn reload(&mut self, journal: &Journal) {
        match journal.read() {
            Ok(e) => {
                self.entries = e;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.selected = None;
    }

    /// Returns a comparison to open (a backup against the server's file now).
    pub fn ui(&mut self, ctx: &egui::Context, journal: &Journal, sessions: &[Session]) -> Option<(Side, Side)> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut compare = None;
        let shown = egui::Window::new("Change journal")
            .open(&mut open)
            .default_size([980.0, 600.0])
            .resizable(true)
            .show(ctx, |ui| compare = self.body(ui, journal, sessions));
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        compare
    }

    fn body(&mut self, ui: &mut egui::Ui, journal: &Journal, sessions: &[Session]) -> Option<(Side, Side)> {
        let mut compare = None;
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Search server, path or changed text")
                    .desired_width(320.0),
            );
            if r.changed() {
                self.selected = None;
            }
            if ui.checkbox(&mut self.problems_only, "Problems only").changed() {
                self.selected = None;
            }
            if ui.button("⟳ Reload").clicked() {
                self.reload(journal);
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        let shown: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (!self.problems_only || e.outcome.is_problem()) && e.matches(&self.filter)
            })
            .collect();
        ui.label(
            RichText::new(format!(
                "{} of {} saves. Kept in {}",
                shown.len(),
                self.entries.len(),
                journal.path().display()
            ))
            .weak()
            .small(),
        );
        ui.separator();

        let sem = crate::look::sem(ui.ctx());
        let (add, remove) = (sem.good, sem.bad);
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        egui::ScrollArea::vertical().id_salt("journal-list").max_height(220.0).auto_shrink([false, true]).show_rows(
            ui,
            row_h,
            shown.len(),
            |ui, range| {
                for &i in &shown[range] {
                    let e = &self.entries[i];
                    let selected = self.selected == Some(i);
                    let row = ui.horizontal(|ui| {
                        ui.set_min_height(row_h - 4.0);
                        // Let clicks on the text reach the row.
                        ui.style_mut().interaction.selectable_labels = false;
                        let r = ui.selectable_label(selected, RichText::new(journal::format_time(e.time)).monospace());
                        ui.label(RichText::new(&e.connection).strong());
                        ui.label(RichText::new(format!("+{}", e.added)).color(add));
                        ui.label(RichText::new(format!("-{}", e.removed)).color(remove));
                        if e.outcome != journal::Outcome::Saved {
                            let c = if e.outcome.is_alarming() {
                                ui.visuals().error_fg_color
                            } else {
                                ui.visuals().warn_fg_color
                            };
                            ui.colored_label(c, e.outcome.label());
                        }
                        if let Some(c) = &e.check {
                            ui.label(RichText::new(c).weak());
                        }
                        if e.sudo {
                            ui.label(RichText::new("sudo").weak());
                        }
                        // Last, so a long path is cut instead of widening the window.
                        ui.add(egui::Label::new(RichText::new(&e.path).monospace()).truncate()).on_hover_text(&e.path);
                        r
                    });
                    // The whole row selects, not only the time.
                    if row.inner.clicked() || row.response.interact(egui::Sense::click()).clicked() {
                        self.selected = Some(i);
                    }
                }
            },
        );
        ui.separator();

        let Some(e) = self.selected.and_then(|i| self.entries.get(i)) else {
            ui.label(RichText::new("Pick a save to see what changed.").weak());
            return None;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{}@{}:{}", e.user, e.host, e.port));
            ui.monospace(&e.path);
            ui.label(journal::format_time(e.time));
        });
        if let Some(b) = &e.local_backup {
            ui.horizontal(|ui| {
                // The same server, connected now?
                let live = sessions.iter().find(|s| {
                    s.state == ConnState::Connected
                        && s.profile.host == e.host
                        && s.profile.port == e.port
                        && s.profile.user == e.user
                });
                let r = ui.add_enabled(live.is_some(), egui::Button::new("Compare with the server now"));
                let r = if live.is_none() {
                    r.on_disabled_hover_text(format!("Connect to {} first", e.connection))
                } else {
                    r
                };
                if r.clicked()
                    && let Some(s) = live
                {
                    compare = Some((Side::Local(b.clone()), Side::remote(s, &e.path)));
                }
                if ui.button("Copy backup path").clicked() {
                    ui.ctx().copy_text(b.display().to_string());
                }
                ui.add(egui::Label::new(RichText::new(b.display().to_string()).monospace().weak()).truncate())
                    .on_hover_text("The file as it was before this save");
            });
        }
        if let Some(b) = &e.remote_backup {
            ui.horizontal(|ui| {
                ui.label("Server backup:");
                ui.add(egui::Label::new(RichText::new(b).monospace()).truncate());
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Copy patch").clicked() {
                ui.ctx().copy_text(e.patch.clone());
            }
            if e.truncated {
                ui.label(RichText::new("The patch was cut short; the backup has the whole file.").weak());
            }
        });
        patch_ui(ui, &e.patch);
        compare
    }
}

fn patch_ui(ui: &mut egui::Ui, patch: &str) {
    let lines: Vec<&str> = patch.lines().collect();
    let sem = crate::look::sem(ui.ctx());
    let (add_bg, del_bg) = (sem.add_bg, sem.del_bg);
    let weak = ui.visuals().weak_text_color();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    egui::ScrollArea::both().id_salt("journal-patch").auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        lines.len(),
        |ui, range| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.spacing_mut().item_spacing.y = 2.0;
            for l in &lines[range] {
                let t = RichText::new(*l).monospace();
                let t = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("@@") {
                    t.color(weak)
                } else if l.starts_with('+') {
                    t.background_color(add_bg)
                } else if l.starts_with('-') {
                    t.background_color(del_bg)
                } else {
                    t
                };
                ui.label(t);
            }
        },
    );
}
