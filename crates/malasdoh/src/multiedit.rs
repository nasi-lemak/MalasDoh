//! Change one file on many servers: edit it once (from one server's copy),
//! see what that does to each server's own copy, then save to all of them
//! the safe way (conflict check, backups, config check, journal entry).
//!
//! "Apply my change" carries just the lines you changed, so the parts that
//! differ per server are kept, and a server whose copy does not match around
//! the change is left alone. "Same content" replaces the whole file.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use egui::RichText;
use malasdoh_core::diff;
use malasdoh_core::edit::{self, Access, EditSession, RemoteIo, SaveOptions};
use malasdoh_core::journal::{self, Journal};
use malasdoh_core::patch::{self, Applied};
use malasdoh_core::profile::{Profile, ProfileId, ProfileStore};
use malasdoh_core::ssh::{self, ConnectContext};
use malasdoh_core::validate::{ValidationRule, find_rule};

use crate::bridge::{EventKind, MULTI_RUN, UiPrompter};
use crate::multirun::host_picker;
use crate::session::{Session, Shared};

const TIMEOUT: Duration = Duration::from_secs(90);

pub enum EditMsg {
    Loaded { run: u64, result: Result<String, String> },
    Checked { run: u64, index: usize, result: Result<Prepared, String> },
    Saved { run: u64, index: usize, result: Result<String, String> },
    SaveDone { run: u64 },
}

/// One server's copy, read and ready to save.
pub struct Prepared {
    io: RemoteIo,
    /// `None`: the file is not there and will be created.
    session: Option<EditSession>,
    old_text: String,
    new_text: String,
    /// Placed by the changed lines alone (the copy differs close by).
    loose: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Change,
    Replace,
}

enum Check {
    Waiting,
    Ready { prep: Box<Prepared>, added: usize, removed: usize },
    Already(Box<Prepared>),
    Failed(String),
}

struct Host {
    id: ProfileId,
    name: String,
    check: Check,
    saved: Option<Result<String, String>>,
}

pub struct MultiEdit {
    pub open: bool,
    path: String,
    selected: BTreeSet<ProfileId>,
    filter: String,
    reference: Option<ProfileId>,
    as_root: bool,
    mode: Mode,
    /// The reference server's copy as loaded, and as edited.
    original: Option<String>,
    text: String,
    loading: bool,
    load_error: Option<String>,
    hosts: Vec<Host>,
    run: u64,
    saving: bool,
    keep_going: bool,
    expanded: Option<usize>,
    rt: Option<tokio::runtime::Handle>,
}

impl Default for MultiEdit {
    fn default() -> Self {
        Self {
            open: false,
            path: String::new(),
            selected: BTreeSet::new(),
            filter: String::new(),
            reference: None,
            as_root: false,
            mode: Mode::Change,
            original: None,
            text: String::new(),
            loading: false,
            load_error: None,
            hosts: Vec::new(),
            run: 0,
            saving: false,
            keep_going: false,
            expanded: None,
            rt: None,
        }
    }
}

/// What `edited` (made from `original`) does to `have`: the new text (`None`
/// when the change is already there), and whether it was placed by the
/// changed lines alone. First with three lines of context that must match
/// as in a normal patch; if this copy differs that close to the change, then
/// with less context, but only where the lines occur exactly once.
pub fn carry_change(original: &str, edited: &str, have: &str, path: &str) -> Result<(Option<String>, bool), String> {
    let mut first_error = None;
    for context in [3, 1, 0] {
        let patch_text = diff::unified_context(original, edited, path, context);
        let Some(fp) = patch::parse(&patch_text).map_err(|e| format!("{e:#}"))?.into_iter().next() else {
            return Ok((None, false));
        };
        let applied = if context == 3 { patch::apply(have, &fp) } else { patch::apply_unique(have, &fp) };
        match applied {
            Ok(Applied::Changed(t)) => return Ok((Some(t), context < 3)),
            Ok(Applied::AlreadyApplied) => return Ok((None, context < 3)),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    let e = first_error.map(|e| e.to_string()).unwrap_or_default();
    Err(format!("{e}; this server's copy differs where the change goes"))
}

impl MultiEdit {
    /// Open on a file, with the server it came from as the reference.
    pub fn show_with(&mut self, path: String, from: ProfileId, sudo: bool) {
        if self.path != path || self.reference != Some(from) {
            self.reset();
            self.path = path;
            self.reference = Some(from);
            self.as_root = sudo;
        }
        self.selected.insert(from);
        self.open = true;
    }

    fn reset(&mut self) {
        self.original = None;
        self.text.clear();
        self.load_error = None;
        self.clear_hosts();
    }

    fn clear_hosts(&mut self) {
        self.run += 1;
        self.saving = false;
        self.expanded = None;
        for h in self.hosts.drain(..) {
            if let Check::Ready { prep, .. } | Check::Already(prep) = h.check {
                let conn = prep.io.conn.clone();
                if let Some(rt) = &self.rt {
                    rt.spawn(async move { conn.disconnect().await });
                }
            }
        }
    }

    pub fn handle(&mut self, msg: EditMsg) {
        match msg {
            EditMsg::Loaded { run, result } if run == self.run => {
                self.loading = false;
                match result {
                    Ok(t) => {
                        self.text = t.clone();
                        self.original = Some(t);
                        self.load_error = None;
                    }
                    Err(e) => self.load_error = Some(e),
                }
            }
            EditMsg::Checked { run, index, result } if run == self.run => {
                let Some(h) = self.hosts.get_mut(index) else { return };
                h.check = match result {
                    Err(e) => Check::Failed(e),
                    Ok(p) if p.session.is_some() && p.old_text == p.new_text => Check::Already(Box::new(p)),
                    Ok(p) => {
                        let d = diff::diff(&p.old_text, &p.new_text, 0);
                        Check::Ready { added: d.added, removed: d.removed, prep: Box::new(p) }
                    }
                };
            }
            EditMsg::Saved { run, index, result } if run == self.run => {
                if let Some(h) = self.hosts.get_mut(index) {
                    h.saved = Some(result);
                }
            }
            EditMsg::SaveDone { run } if run == self.run => self.saving = false,
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        self.loading || self.saving || self.hosts.iter().any(|h| matches!(h.check, Check::Waiting))
    }

    fn connect_ctx(shared: &Shared) -> ConnectContext<UiPrompter> {
        let prompter = Arc::new(UiPrompter { events: shared.events.clone(), session: MULTI_RUN });
        ConnectContext::new(shared.known_hosts.clone(), shared.secrets.clone(), prompter)
    }

    /// Connect and get ready to read and write files as the chosen user.
    async fn io_for(chain: Vec<Profile>, ctx: &ConnectContext<UiPrompter>, as_root: bool) -> anyhow::Result<RemoteIo> {
        let conn = Arc::new(ssh::connect(chain, ctx).await?);
        let access = if as_root {
            // Passwordless sudo only: asking for a password per server is
            // not workable for many servers.
            RemoteIo::check_sudo(&conn, None)
                .await
                .map_err(|e| anyhow::anyhow!("{e:#}; only passwordless sudo is used here"))?;
            Access::Sudo { password: None }
        } else {
            Access::Sftp
        };
        let fs = conn.open_fs().await?;
        Ok(RemoteIo { fs, conn, access })
    }

    fn load(&mut self, profiles: &ProfileStore, shared: &Shared) {
        let Some(id) = self.reference else { return };
        let chain = match profiles.chain(id) {
            Ok(c) => c,
            Err(e) => return self.load_error = Some(format!("{e:#}")),
        };
        self.clear_hosts();
        self.loading = true;
        let (run, path, as_root, ctx, events) =
            (self.run, self.path.trim().to_string(), self.as_root, Self::connect_ctx(shared), shared.events.clone());
        shared.rt.spawn(async move {
            let work = async {
                let io = Self::io_for(chain, &ctx, as_root).await?;
                let s = io.open(&path).await;
                io.conn.disconnect().await;
                anyhow::Ok(s?.base_text)
            };
            let result = match tokio::time::timeout(TIMEOUT, work).await {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(_) => Err("timed out".into()),
            };
            events.send(MULTI_RUN, EventKind::MultiEdit(EditMsg::Loaded { run, result }));
        });
    }

    fn preview(&mut self, profiles: &ProfileStore, shared: &Shared) {
        self.clear_hosts();
        let mut hosts: Vec<(Host, Vec<Profile>)> = Vec::new();
        for id in &self.selected {
            let (Some(p), Ok(chain)) = (profiles.get(*id), profiles.chain(*id)) else { continue };
            let host = Host { id: *id, name: p.display_name(), check: Check::Waiting, saved: None };
            hosts.push((host, chain));
        }
        hosts.sort_by_key(|(h, _)| h.name.to_lowercase());
        let sem = Arc::new(tokio::sync::Semaphore::new(8));
        let original = self.original.clone().unwrap_or_default();
        for (index, (_, chain)) in hosts.iter().enumerate() {
            let (run, path, as_root, mode) = (self.run, self.path.trim().to_string(), self.as_root, self.mode);
            let (chain, ctx, events, sem) =
                (chain.clone(), Self::connect_ctx(shared), shared.events.clone(), sem.clone());
            let (original, edited) = (original.clone(), self.text.clone());
            shared.rt.spawn(async move {
                let _permit = sem.acquire().await;
                let work = async {
                    let io = Self::io_for(chain, &ctx, as_root).await?;
                    let exists = io.read_bytes(&path).await?.is_some();
                    let prep = if !exists {
                        if mode == Mode::Change {
                            anyhow::bail!("{path} does not exist here");
                        }
                        Prepared { io, session: None, old_text: String::new(), new_text: edited, loose: false }
                    } else {
                        let session = io.open(&path).await?;
                        let old_text = session.base_text.clone();
                        let (new_text, loose) = match mode {
                            Mode::Replace => (edited, false),
                            Mode::Change => {
                                let (t, loose) =
                                    carry_change(&original, &edited, &old_text, &path).map_err(anyhow::Error::msg)?;
                                (t.unwrap_or_else(|| old_text.clone()), loose)
                            }
                        };
                        Prepared { io, session: Some(session), old_text, new_text, loose }
                    };
                    anyhow::Ok(prep)
                };
                let result = match tokio::time::timeout(TIMEOUT, work).await {
                    Ok(r) => r.map_err(|e| format!("{e:#}")),
                    Err(_) => Err("timed out".into()),
                };
                events.send(MULTI_RUN, EventKind::MultiEdit(EditMsg::Checked { run, index, result }));
            });
        }
        self.hosts = hosts.into_iter().map(|(h, _)| h).collect();
    }

    /// Save to the ready servers in order, stopping at the first failure
    /// unless told to keep going.
    fn save(&mut self, profiles: &ProfileStore, shared: &Shared, rules: &[ValidationRule]) {
        let path = self.path.trim().to_string();
        let mut jobs = Vec::new();
        for (index, h) in self.hosts.iter().enumerate() {
            let (Check::Ready { prep, .. }, Some(p)) = (&h.check, profiles.get(h.id)) else { continue };
            let opts = SaveOptions {
                local_backup_dir: Some(edit::host_backup_dir(&shared.paths.backups_dir(), &p.host, p.port)),
                remote_backup: p.remote_backup,
                force: false,
                validation: find_rule(rules, &path).cloned(),
            };
            let entry = Session::journal_entry(p, &path, self.as_root);
            jobs.push((index, prep.io.clone(), prep.session.clone(), prep.new_text.clone(), opts, entry));
        }
        if jobs.is_empty() {
            return;
        }
        self.saving = true;
        let (run, keep_going, events) = (self.run, self.keep_going, shared.events.clone());
        let journal = Journal::new(shared.paths.journal_file());
        shared.rt.spawn(async move {
            for (index, io, session, text, opts, entry) in jobs {
                let result = match session {
                    Some(mut session) => {
                        let saved = io.save(&mut session, &text, &opts).await;
                        if let Some(e) = journal::entry_for(entry, &saved, &session.base_text, &text) {
                            let _ = journal.append(&e);
                        }
                        saved.map(|rep| match rep.check {
                            Some(c) => format!("saved, {} check passed", c.rule),
                            None => "saved".to_string(),
                        })
                    }
                    None => {
                        let made = io.create_new(&path, text.as_bytes(), 0o644).await;
                        if made.is_ok() {
                            let e = journal::Entry { time: journal::now(), ..entry }.with_change("", &text);
                            let _ = journal.append(&e);
                        }
                        made.map(|_| "created".to_string())
                    }
                };
                let failed = result.is_err();
                let result = result.map_err(|e| format!("{e:#}"));
                events.send(MULTI_RUN, EventKind::MultiEdit(EditMsg::Saved { run, index, result }));
                if failed && !keep_going {
                    break;
                }
            }
            events.send(MULTI_RUN, EventKind::MultiEdit(EditMsg::SaveDone { run }));
        });
    }

    pub fn ui(&mut self, ctx: &egui::Context, profiles: &ProfileStore, shared: &Shared, rules: &[ValidationRule]) {
        if !self.open {
            return;
        }
        self.rt = Some(shared.rt.clone());
        let mut open = true;
        let (mut load, mut preview, mut save) = (false, false, false);
        egui::Window::new("Edit a file on many servers")
            .open(&mut open)
            .default_width(980.0)
            .default_height(640.0)
            .show(ctx, |ui| {
                let busy = self.busy();
                egui::Panel::left("multi-edit-hosts").resizable(true).default_size(230.0).show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| host_picker(ui, profiles, &mut self.selected, &mut self.filter));
                });
                if self.reference.is_none_or(|r| !self.selected.contains(&r)) {
                    self.reference = self
                        .selected
                        .iter()
                        .min_by_key(|id| profiles.get(**id).map(|p| p.display_name().to_lowercase()))
                        .copied();
                }
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("File").strong());
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.path)
                                .hint_text("/etc/nginx/nginx.conf")
                                .desired_width(320.0),
                        );
                        if r.changed() {
                            self.reset();
                        }
                        if ui
                            .checkbox(&mut self.as_root, "As root (sudo -n)")
                            .on_hover_text("Needs passwordless sudo; servers that need a password fail")
                            .changed()
                        {
                            self.clear_hosts();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Start from the copy on");
                        let name = |id: ProfileId| profiles.get(id).map(|p| p.display_name()).unwrap_or_default();
                        let current = self.reference.map(name).unwrap_or_else(|| "(pick servers)".into());
                        egui::ComboBox::from_id_salt("multi-edit-ref").selected_text(current).show_ui(ui, |ui| {
                            for id in self.selected.clone() {
                                ui.selectable_value(&mut self.reference, Some(id), name(id));
                            }
                        });
                        let can = self.reference.is_some() && !self.path.trim().is_empty();
                        let label = if self.original.is_some() { "Load again" } else { "Load" };
                        if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                            load = true;
                        }
                        if self.loading {
                            ui.spinner();
                        }
                    });
                    ui.horizontal(|ui| {
                        let before = self.mode;
                        ui.radio_value(&mut self.mode, Mode::Change, "Apply my change").on_hover_text(
                            "Only the lines you change are carried over; each server keeps its own differences",
                        );
                        ui.radio_value(&mut self.mode, Mode::Replace, "Same content everywhere")
                            .on_hover_text("Every server gets exactly this text (created where missing)");
                        if self.mode != before {
                            self.clear_hosts();
                        }
                    });
                });
                if let Some(e) = &self.load_error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                if self.original.is_some() {
                    egui::ScrollArea::vertical().id_salt("multi-edit-text").max_height(240.0).show(ui, |ui| {
                        let r = ui.add_enabled(
                            !busy,
                            egui::TextEdit::multiline(&mut self.text)
                                .code_editor()
                                .desired_rows(10)
                                .desired_width(f32::INFINITY),
                        );
                        if r.changed() {
                            self.clear_hosts();
                        }
                    });
                }
                let changed = self.original.as_deref().is_some_and(|o| o != self.text);
                ui.horizontal(|ui| {
                    let can_preview = !busy
                        && !self.selected.is_empty()
                        && self.original.is_some()
                        && (changed || self.mode == Mode::Replace);
                    let b = ui
                        .add_enabled(
                            can_preview,
                            egui::Button::new(format!("Preview on {} server(s)", self.selected.len())),
                        )
                        .on_hover_text("Read each server's copy and work out the change, without writing");
                    let b = if self.original.is_some() && !changed && self.mode == Mode::Change {
                        b.on_disabled_hover_text("Change the text first")
                    } else {
                        b
                    };
                    if b.clicked() {
                        preview = true;
                    }
                    let ready = self.hosts.iter().filter(|h| matches!(h.check, Check::Ready { .. })).count();
                    let any_saved = self.hosts.iter().any(|h| h.saved.is_some());
                    if ui
                        .add_enabled(
                            !busy && ready > 0 && !any_saved,
                            egui::Button::new(format!("Save to {ready} server(s)")),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    ui.checkbox(&mut self.keep_going, "Keep going past failures")
                        .on_hover_text("Otherwise saving stops at the first server that fails");
                    if busy {
                        ui.spinner();
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                    }
                });
                ui.separator();
                self.results_ui(ui);
            });
        if !open {
            self.clear_hosts();
        }
        self.open = open;
        if load {
            self.load(profiles, shared);
        } else if preview {
            self.preview(profiles, shared);
        } else if save {
            self.save(profiles, shared, rules);
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        if self.hosts.is_empty() {
            let hint = if self.original.is_none() {
                "Pick the servers and the file, then load it from one of them to edit."
            } else {
                "Edit the text, then preview to see what it does on each server. Nothing is written until you save."
            };
            ui.label(RichText::new(hint).weak());
            return;
        }
        let ok = egui::Color32::from_rgb(90, 190, 90);
        let err = ui.visuals().error_fg_color;
        let weak = ui.visuals().weak_text_color();
        let warn = ui.visuals().warn_fg_color;
        let count = |f: fn(&Check) -> bool| self.hosts.iter().filter(|h| f(&h.check)).count();
        ui.label(format!(
            "{} to change · {} already done · {} failed · {} reading",
            count(|c| matches!(c, Check::Ready { .. })),
            count(|c| matches!(c, Check::Already(_))),
            count(|c| matches!(c, Check::Failed(_))),
            count(|c| matches!(c, Check::Waiting)),
        ));
        let mut toggle = None;
        egui::ScrollArea::vertical().id_salt("multi-edit-results").auto_shrink([false, false]).show(ui, |ui| {
            for (i, h) in self.hosts.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (text, color) = match &h.check {
                        Check::Waiting => ("reading…".to_string(), weak),
                        Check::Ready { prep, .. } if prep.session.is_none() => ("new file".to_string(), ok),
                        Check::Ready { added, removed, prep } if prep.loose => {
                            (format!("ready  +{added} -{removed}  (placed by the changed lines; check the diff)"), warn)
                        }
                        Check::Ready { added, removed, .. } => (format!("ready  +{added} -{removed}"), ok),
                        Check::Already(_) => ("already like this".to_string(), weak),
                        Check::Failed(m) => (m.clone(), err),
                    };
                    if matches!(h.check, Check::Ready { .. })
                        && ui.small_button(if self.expanded == Some(i) { "Hide" } else { "Show" }).clicked()
                    {
                        toggle = Some(i);
                    }
                    ui.strong(&h.name);
                    ui.colored_label(color, text);
                    match &h.saved {
                        Some(Ok(m)) => {
                            ui.colored_label(ok, format!("✔ {m}"));
                        }
                        Some(Err(m)) => {
                            ui.colored_label(err, format!("✖ {m}"));
                        }
                        None => {}
                    }
                });
                if self.expanded == Some(i)
                    && let Check::Ready { prep, .. } = &h.check
                {
                    let d = diff::diff(&prep.old_text, &prep.new_text, 3);
                    crate::editor::diff_ui(ui, &d, egui::Id::new(("multi-edit-diff", i)));
                }
            }
        });
        if let Some(i) = toggle {
            self.expanded = if self.expanded == Some(i) { None } else { Some(i) };
        }
        let saved = self.hosts.iter().filter(|h| h.saved.is_some()).count();
        let failed = self.hosts.iter().any(|h| matches!(h.saved, Some(Err(_))));
        let ready = self.hosts.iter().filter(|h| matches!(h.check, Check::Ready { .. })).count();
        if failed && !self.saving && saved < ready {
            ui.colored_label(
                err,
                "Stopped at the first server that could not be saved; the servers before it are changed (each has a backup and a journal entry).",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_only_the_changed_lines() {
        let original = "user www;\nworker_processes 4;\npid /run/nginx.pid;\nevents {}\nhttp {\n  sendfile on;\n  server_name a.example;\n}\n";
        let edited = original.replace("worker_processes 4;", "worker_processes 8;");
        // Another server with its own name further down: kept.
        let other = original.replace("a.example", "b.example");
        let (got, loose) = carry_change(original, &edited, &other, "/etc/nginx/nginx.conf").unwrap();
        assert_eq!(got.unwrap(), other.replace("worker_processes 4;", "worker_processes 8;"));
        assert!(!loose);
        // Already changed: nothing to do.
        assert_eq!(carry_change(original, &edited, &edited, "/x").unwrap().0, None);
        // Differs right at the change: refused.
        let odd = original.replace("worker_processes 4;", "worker_processes auto;");
        assert!(carry_change(original, &edited, &odd, "/x").is_err());
    }

    #[test]
    fn differences_next_to_the_change_fall_back_to_unique_lines() {
        let original = "a\nworkers 4\ntimeout 30\nlog info\ncache on\n";
        let edited = original.replace("timeout 30", "timeout 60");
        // "cache off" is within the three lines of context.
        let other = original.replace("cache on", "cache off");
        let (got, loose) = carry_change(original, &edited, &other, "/x").unwrap();
        assert_eq!(got.unwrap(), other.replace("timeout 30", "timeout 60"));
        assert!(loose);
        // Both neighbours differ too: placed by the changed line alone...
        let both = original.replace("workers 4", "workers 8").replace("log info", "log debug");
        assert_eq!(carry_change(original, &edited, &both, "/x").unwrap().0.unwrap(), both.replace("30", "60"));
        // ...but only when it occurs once: not guessed.
        let twice = format!("{both}timeout 30\n");
        assert!(carry_change(original, &edited, &twice, "/x").is_err());
    }
}
