//! The folder compare / synchronise window.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::RichText;
use malasdoh_core::sftp::{Progress, RemoteFs, human_size};
use malasdoh_core::sync::{self, Action, Direction, Plan, SyncOptions, SyncReport};

use crate::bridge::SessionId;
use crate::session::{SessionMsg, Shared};

pub enum SyncMsg {
    Planned(Result<(Plan, PathBuf, String), String>),
    Done(Result<SyncReport, String>),
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
        self.phase = match msg {
            SyncMsg::Planned(Ok((p, l, r))) => Phase::Planned(p, l, r),
            SyncMsg::Done(Ok(r)) => Phase::Done(r),
            SyncMsg::Planned(Err(e)) | SyncMsg::Done(Err(e)) => Phase::Failed(e),
        };
    }

    pub fn ui(&mut self, ctx: &egui::Context, fs: Option<&RemoteFs>, shared: &Shared, session: SessionId) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut compare = false;
        let mut run = false;
        egui::Window::new("Compare and synchronise folders")
            .id(egui::Id::new(("sync", session)))
            .open(&mut open)
            .default_width(720.0)
            .show(ctx, |ui| {
                let busy = matches!(self.phase, Phase::Planning | Phase::Running(_));
                ui.add_enabled_ui(!busy, |ui| {
                    egui::Grid::new(("sync-form", session)).num_columns(2).show(ui, |ui| {
                        ui.label("This computer");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.local)
                                    .desired_width(420.0)
                                    .hint_text("C:\\site or ~/site"),
                            );
                            if ui.button("Browse…").clicked()
                                && let Some(d) = rfd::FileDialog::new().set_title("Local folder").pick_folder()
                            {
                                self.local = d.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Server");
                        ui.add(egui::TextEdit::singleline(&mut self.remote).desired_width(420.0));
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
                                .desired_width(420.0)
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
                            plan_ui(ui, plan, session);
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
                        for (path, e) in &r.failed {
                            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {path}: {e}"));
                        }
                    }
                    Phase::Failed(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                    }
                }
            });
        self.open = open;
        let Some(fs) = fs else { return };
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
            let (fs, opts, events) = (fs.clone(), self.opts.clone(), shared.events.clone());
            shared.rt.spawn(async move {
                let r = sync::plan(&fs, &local, &remote, &opts)
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
            shared.rt.spawn(async move {
                let r = sync::execute(&fs, &local, &remote, &plan, &progress).await.map_err(|e| format!("{e:#}"));
                events.session(session, SessionMsg::Sync(SyncMsg::Done(r)));
            });
        }
    }
}

fn plan_ui(ui: &mut egui::Ui, plan: &mut Plan, session: SessionId) {
    ui.label(format!("{} unchanged. Untick anything you do not want:", plan.unchanged));
    egui::ScrollArea::vertical().id_salt(("plan", session)).max_height(320.0).show(ui, |ui| {
        egui::Grid::new(("plan-grid", session)).striped(true).num_columns(4).show(ui, |ui| {
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
                ui.end_row();
            }
        });
    });
}
