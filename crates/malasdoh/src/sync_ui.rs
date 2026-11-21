//! The folder compare / synchronise window.

use crate::dlg::FitScreen as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::RichText;
use malasdoh_core::replace::DIFF_MAX;
use malasdoh_core::sftp::{Progress, RemoteFs, human_size};
use malasdoh_core::sync::{self, Action, Direction, Plan, SyncOptions, SyncReport};
use malasdoh_core::transfer::{self, Checker, Way};

use crate::replace_ui::TextSlot;

use crate::bridge::SessionId;
use crate::session::{SessionMsg, Shared};

pub enum SyncMsg {
    Planned(Result<(Plan, PathBuf, String), String>),
    /// With the roots it ran for.
    Done(Result<SyncReport, String>, PathBuf, String),
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Planning,
    /// The plan, with the roots it was made for (the fields may change after).
    Planned(Plan, PathBuf, String),
    Running(Arc<Progress>),
    Done(SyncReport),
    Failed(String),
}

#[derive(Default)]
pub struct SyncWindow {
    pub open: bool,
    pub remote: String,
    local: String,
    opts: SyncOptions,
    exclude_text: String,
    phase: Phase,
    /// The plan row whose changes are shown, and both its texts once read.
    showing: Option<(String, TextSlot)>,
}

impl SyncWindow {
    pub fn show_for(&mut self, remote_dir: &str) {
        self.open = true;
        if self.remote.is_empty() || matches!(self.phase, Phase::Idle | Phase::Done(_) | Phase::Failed(_)) {
            self.remote = remote_dir.to_string();
        }
        if self.exclude_text.is_empty() {
            self.exclude_text = self.opts.exclude.join(", ");
        }
    }

    pub fn handle(&mut self, msg: SyncMsg) {
        self.showing = None;
        self.phase = match msg {
            SyncMsg::Planned(Ok((p, l, r))) => Phase::Planned(p, l, r),
            SyncMsg::Done(Ok(r), ..) => Phase::Done(r),
            SyncMsg::Planned(Err(e)) | SyncMsg::Done(Err(e), ..) => Phase::Failed(e),
        };
    }

    /// `checker`: the connection (and sudo password, as root) for checksums;
    /// `backups`: where to back up server files before replacing or
    /// deleting them (none: no backups).
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        fs: Option<&RemoteFs>,
        checker: Option<(Arc<malasdoh_core::ssh::Connection>, Option<Option<String>>)>,
        backups: Option<PathBuf>,
        shared: &Shared,
        session: SessionId,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut compare = false;
        let mut run = false;
        let mut show = None;
        let shown = egui::Window::new("Compare and synchronise folders")
            .id(egui::Id::new(("sync", session)))
            .open(&mut open)
            .default_width(720.0)
            .fit_to_screen(ctx)
            .show(ctx, |ui| {
                let busy = matches!(self.phase, Phase::Planning | Phase::Running(_));
                ui.add_enabled_ui(!busy, |ui| {
                    egui::Grid::new(("sync-form", session)).num_columns(2).show(ui, |ui| {
                        ui.label("This computer");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.local)
                                    .desired_width(crate::dlg::field_width(ui, 420.0))
                                    .hint_text("C:\\site or ~/site"),
                            );
                            if ui.button("Browse…").clicked()
                                && let Some(d) = crate::dialogs::FileDialog::new()
                                    .id("sync-folder")
                                    .set_title("Local folder")
                                    .pick_folder()
                            {
                                self.local = d.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Server");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.remote)
                                .desired_width(crate::dlg::field_width(ui, 420.0)),
                        );
                        ui.end_row();
                        ui.label("Direction");
                        ui.horizontal(|ui| {
                            for d in [Direction::Upload, Direction::Download, Direction::Both] {
                                ui.selectable_value(&mut self.opts.direction, d, d.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Options");
                        ui.vertical(|ui| {
                            ui.add_enabled(
                                self.opts.direction != Direction::Both,
                                egui::Checkbox::new(
                                    &mut self.opts.delete_extra,
                                    "Delete files the other side does not have",
                                ),
                            );
                            ui.checkbox(&mut self.opts.compare_contents, "Compare contents when size and time match");
                        });
                        ui.end_row();
                        ui.label("Skip names");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.exclude_text)
                                .desired_width(crate::dlg::field_width(ui, 420.0))
                                .hint_text(".git, node_modules, *.log"),
                        );
                        ui.end_row();
                    });
                    if ui.button("🔍 Compare").clicked() {
                        compare = true;
                    }
                });
                ui.separator();
                match &mut self.phase {
                    Phase::Idle => {
                        ui.label("Compare first: you will see every copy and deletion before anything happens.");
                    }
                    Phase::Planning => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Comparing…");
                        });
                    }
                    Phase::Planned(plan, ..) => {
                        if plan.items.is_empty() {
                            ui.label(format!("Already in sync ({} files unchanged).", plan.unchanged));
                        } else {
                            show = plan_ui(ui, plan, self.showing.as_ref().map(|(r, _)| r.as_str()), session);
                            if let Some((rel, slot)) = &self.showing {
                                changes_ui(ui, rel, slot, session);
                            }
                            if backups.is_some() {
                                ui.label(
                                    RichText::new(
                                        "Files on the server are backed up before they are replaced or deleted \
                                         (Backups… on a file shows them).",
                                    )
                                    .weak()
                                    .small(),
                                );
                            }
                            ui.horizontal(|ui| {
                                let n = plan.items.iter().filter(|i| i.enabled).count();
                                if ui
                                    .add_enabled(
                                        n > 0,
                                        egui::Button::new(format!(
                                            "Synchronise {n} item(s), {}",
                                            human_size(plan.bytes())
                                        )),
                                    )
                                    .clicked()
                                {
                                    run = true;
                                }
                                if ui.small_button("Select all").clicked() {
                                    plan.items.iter_mut().for_each(|i| i.enabled = true);
                                }
                                if ui.small_button("Select none").clicked() {
                                    plan.items.iter_mut().for_each(|i| i.enabled = false);
                                }
                            });
                        }
                        if !plan.skipped.is_empty() {
                            egui::CollapsingHeader::new(format!("Skipped ({})", plan.skipped.len())).show(ui, |ui| {
                                for s in &plan.skipped {
                                    ui.monospace(s);
                                }
                            });
                        }
                    }
                    Phase::Running(p) => {
                        let total = p.total_bytes.load(Ordering::Relaxed).max(1);
                        let done = p.done_bytes.load(Ordering::Relaxed);
                        ui.add(egui::ProgressBar::new(done as f32 / total as f32).show_percentage());
                        ui.label(p.current.lock().unwrap().clone());
                        if ui.button("Cancel").clicked() {
                            p.cancel.store(true, Ordering::Relaxed);
                        }
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    }
                    Phase::Done(r) => {
                        ui.label(format!("✔ Done: {} item(s).", r.done));
                        if !r.backups.is_empty() {
                            ui.label(format!(
                                "{} file(s) on the server were backed up first (Backups… on each file).",
                                r.backups.len()
                            ));
                            let deleted: Vec<_> = r.backups.iter().filter(|(_, _, up)| !*up).collect();
                            if !deleted.is_empty() {
                                ui.label(RichText::new("Backups of the deleted files:").weak());
                                for (rel, path, _) in deleted {
                                    ui.horizontal(|ui| {
                                        ui.monospace(rel);
                                        ui.add(
                                            egui::Label::new(RichText::new(path.display().to_string()).weak().small())
                                                .selectable(true),
                                        );
                                    });
                                }
                            }
                        }
                        if !r.too_big.is_empty() {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!(
                                    "Not backed up, being over {}: {}",
                                    human_size(malasdoh_core::replace::BACKUP_MAX),
                                    r.too_big.join(", ")
                                ),
                            );
                        }
                        if r.verified > 0 {
                            ui.label(format!("{} copied file(s) checked by SHA-256: they match.", r.verified));
                        }
                        if let Some(why) = &r.unverified {
                            ui.label(RichText::new(format!("Not compared by checksum: {why}.")).weak());
                        }
                        for (path, e) in &r.failed {
                            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {path}: {e}"));
                        }
                    }
                    Phase::Failed(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                    }
                }
            });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        let Some(fs) = fs else { return };
        if let Some(rel) = show {
            self.load_changes(rel, fs, shared, ctx);
        }
        self.opts.exclude =
            self.exclude_text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if compare {
            let local = malasdoh_core::paths::expand_tilde(self.local.trim());
            let remote = self.remote.trim().to_string();
            if self.local.trim().is_empty() || remote.is_empty() {
                self.phase = Phase::Failed("Choose both folders first.".into());
                return;
            }
            self.phase = Phase::Planning;
            let (fs, opts, events, checker) = (fs.clone(), self.opts.clone(), shared.events.clone(), checker.clone());
            shared.rt.spawn(async move {
                let checker = checker.map(|(c, sudo)| Checker::new(c, sudo));
                let r = sync::plan(&fs, &local, &remote, &opts, checker.as_ref())
                    .await
                    .map(|p| (p, local, remote))
                    .map_err(|e| format!("{e:#}"));
                events.session(session, SessionMsg::Sync(SyncMsg::Planned(r)));
            });
        }
        if run && let Phase::Planned(plan, local, remote) = std::mem::take(&mut self.phase) {
            let progress = Arc::new(Progress::default());
            self.phase = Phase::Running(progress.clone());
            let (fs, events) = (fs.clone(), shared.events.clone());
            let topts = shared.transfer.read().unwrap().clone();
            shared.rt.spawn(async move {
                let checker = checker.map(|(c, sudo)| Checker::new(c, sudo));
                let r =
                    sync::execute(&fs, &local, &remote, &plan, &progress, &topts, checker.as_ref(), backups.as_deref())
                        .await
                        .map_err(|e| format!("{e:#}"));
                events.session(session, SessionMsg::Sync(SyncMsg::Done(r, local, remote)));
            });
        }
    }
}

/// The plan's rows; returns a row whose changes to show (or hide).
fn plan_ui(ui: &mut egui::Ui, plan: &mut Plan, showing: Option<&str>, session: SessionId) -> Option<Option<String>> {
    let mut show = None;
    ui.label(format!("{} unchanged. Untick anything you do not want:", plan.unchanged));
    egui::ScrollArea::vertical().id_salt(("plan", session)).max_height(260.0).show(ui, |ui| {
        egui::Grid::new(("plan-grid", session)).striped(true).num_columns(5).show(ui, |ui| {
            for it in &mut plan.items {
                ui.checkbox(&mut it.enabled, "");
                let (what, danger) = match &it.action {
                    Action::MkdirRemote => ("create folder on server".to_string(), false),
                    Action::MkdirLocal => ("create folder here".to_string(), false),
                    Action::Upload { size } => (format!("upload {}", human_size(*size)), false),
                    Action::Download { size } => (format!("download {}", human_size(*size)), false),
                    Action::DeleteRemote { dir } => {
                        (format!("DELETE on server{}", if *dir { " (folder)" } else { "" }), true)
                    }
                    Action::DeleteLocal { dir } => {
                        (format!("DELETE here{}", if *dir { " (folder)" } else { "" }), true)
                    }
                };
                let t = RichText::new(what);
                ui.label(if danger { t.color(ui.visuals().error_fg_color) } else { t });
                ui.monospace(&it.rel);
                ui.label(RichText::new(it.reason).weak());
                let copy = matches!(it.action, Action::Upload { .. } | Action::Download { .. });
                match it.replaces {
                    Some(old) if copy && old <= DIFF_MAX && it.action_size() <= DIFF_MAX => {
                        let shown = showing == Some(it.rel.as_str());
                        if ui.selectable_label(shown, "Show changes").clicked() {
                            show = Some((!shown).then(|| it.rel.clone()));
                        }
                    }
                    _ => {
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });
    });
    show
}

/// Both versions of a plan row, as a diff.
fn changes_ui(ui: &mut egui::Ui, rel: &str, slot: &TextSlot, session: SessionId) {
    ui.separator();
    ui.label(RichText::new(format!("{rel}: what the copy changes")).strong());
    match slot.lock().unwrap().clone() {
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
                        "+{} -{} lines (- is in the copy being replaced, + in the one replacing it)",
                        diff.added, diff.removed
                    ))
                    .weak(),
                );
                crate::editor::diff_ui_max(ui, &diff, egui::Id::new(("sync-diff", session, rel)), 260.0);
            }
        }
    }
    ui.separator();
}

impl SyncWindow {
    /// Reads both versions of a plan row for [`changes_ui`]; `None` hides it.
    fn load_changes(&mut self, rel: Option<String>, fs: &RemoteFs, shared: &Shared, ctx: &egui::Context) {
        let Some(rel) = rel else {
            self.showing = None;
            return;
        };
        let Phase::Planned(plan, local, remote) = &self.phase else { return };
        let Some(it) = plan.items.iter().find(|i| i.rel == rel) else { return };
        let way = if matches!(it.action, Action::Upload { .. }) { Way::Up } else { Way::Down };
        let local = rel.split('/').fold(local.clone(), |p, c| p.join(c));
        let file = transfer::job_of(way, vec![(local, malasdoh_core::sftp::join(remote, &rel), it.action_size())])
            .files
            .remove(0);
        let slot = TextSlot::default();
        self.showing = Some((rel, slot.clone()));
        let (fs, ctx) = (fs.clone(), ctx.clone());
        shared.rt.spawn(async move {
            let got = malasdoh_core::replace::texts(&fs, way, &file)
                .await
                .map(|t| {
                    let d = malasdoh_core::diff::diff(&t.old, &t.new, 3);
                    (t, d)
                })
                .map_err(|e| format!("{e:#}"));
            *slot.lock().unwrap() = Some(got);
            ctx.request_repaint();
        });
    }
}
