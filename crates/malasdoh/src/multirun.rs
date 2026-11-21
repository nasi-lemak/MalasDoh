//! Run one command on many hosts and compare the results side by side.

use crate::dlg::FitScreen as _;
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::RichText;

use crate::listsel::{KeyAction, Selection};
use malasdoh_core::profile::{Profile, ProfileId, ProfileStore};
use malasdoh_core::ssh::{self, ConnectContext, ExecOutput, shell_quote};

use crate::bridge::{EventKind, MULTI_RUN, Question, UiPrompter};
use crate::session::Shared;
use crate::snippets::{self, SnippetStore};

pub enum MultiMsg {
    Started { run: u64, index: usize },
    Finished { run: u64, index: usize, result: Result<ExecOutput, String>, elapsed: Duration },
}

enum HostState {
    Waiting,
    Running(Instant),
    Done(ExecOutput, Duration),
    Failed(String, Duration),
    Skipped,
}

struct HostRun {
    name: String,
    state: HostState,
}

pub struct MultiRun {
    pub open: bool,
    pub command: String,
    selected: BTreeSet<ProfileId>,
    parallel: usize,
    timeout_secs: u64,
    as_root: bool,
    runs: Vec<HostRun>,
    run_id: u64,
    stop: Arc<AtomicBool>,
    pub questions: VecDeque<Question>,
    only_problems: bool,
    filter: String,
}

impl Default for MultiRun {
    fn default() -> Self {
        Self {
            open: false,
            command: String::new(),
            selected: BTreeSet::new(),
            parallel: 8,
            timeout_secs: 120,
            as_root: false,
            runs: Vec::new(),
            run_id: 0,
            stop: Arc::new(AtomicBool::new(false)),
            questions: VecDeque::new(),
            only_problems: false,
            filter: String::new(),
        }
    }
}

impl MultiRun {
    pub fn running(&self) -> bool {
        self.runs.iter().any(|r| matches!(r.state, HostState::Waiting | HostState::Running(_)))
    }

    pub fn handle(&mut self, msg: MultiMsg) {
        match msg {
            MultiMsg::Started { run, index } if run == self.run_id => {
                if let Some(r) = self.runs.get_mut(index) {
                    r.state = HostState::Running(Instant::now());
                }
            }
            MultiMsg::Finished { run, index, result, elapsed } if run == self.run_id => {
                if let Some(r) = self.runs.get_mut(index) {
                    r.state = match result {
                        Ok(out) => HostState::Done(out, elapsed),
                        Err(e) if e == "skipped" => HostState::Skipped,
                        Err(e) => HostState::Failed(e, elapsed),
                    };
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, profiles: &ProfileStore, shared: &Shared, only: Option<Vec<usize>>) {
        let mut targets: Vec<(String, Vec<Profile>)> = Vec::new();
        for id in &self.selected {
            let Some(p) = profiles.get(*id) else { continue };
            match profiles.chain(*id) {
                Ok(chain) => targets.push((p.display_name(), chain)),
                Err(_) => continue,
            }
        }
        targets.sort_by_key(|t| t.0.to_lowercase());
        // "Retry failed": keep the earlier results, rerun just those hosts.
        let indices: Vec<usize> = match &only {
            Some(list) => list.clone(),
            None => {
                self.runs =
                    targets.iter().map(|(n, _)| HostRun { name: n.clone(), state: HostState::Waiting }).collect();
                (0..targets.len()).collect()
            }
        };
        self.run_id += 1;
        self.stop = Arc::new(AtomicBool::new(false));
        let command = if self.as_root {
            format!("sudo -n -- sh -c {}", shell_quote(&self.command))
        } else {
            self.command.clone()
        };
        let (run, sem) = (self.run_id, Arc::new(tokio::sync::Semaphore::new(self.parallel.max(1))));
        let timeout = Duration::from_secs(self.timeout_secs.max(1));
        for index in indices {
            let Some(name) = self.runs.get(index).map(|r| r.name.clone()) else { continue };
            let Some((_, chain)) = targets.iter().find(|(n, _)| *n == name).cloned() else { continue };
            self.runs[index].state = HostState::Waiting;
            let (shared2, sem, stop, command) = (shared.clone(), sem.clone(), self.stop.clone(), command.clone());
            shared.rt.spawn(async move {
                let ev = shared2.events.clone();
                let _permit = sem.acquire().await;
                let send = |msg| ev.send(MULTI_RUN, EventKind::Multi(msg));
                if stop.load(Ordering::Relaxed) {
                    send(MultiMsg::Finished { run, index, result: Err("skipped".into()), elapsed: Duration::ZERO });
                    return;
                }
                send(MultiMsg::Started { run, index });
                let t0 = Instant::now();
                let prompter = Arc::new(UiPrompter { events: ev.clone(), session: MULTI_RUN });
                let mut ctx = ConnectContext::new(shared2.known_hosts.clone(), shared2.secrets.clone(), prompter);
                ctx.wireguard = shared2.wireguard.clone();
                ctx.stored_keys = shared2.stored_keys.clone();
                let work = async {
                    let conn = ssh::connect(chain, &ctx).await?;
                    let out = conn.exec(&command, None).await;
                    conn.disconnect().await;
                    out
                };
                let result = match tokio::time::timeout(timeout, work).await {
                    Ok(r) => r.map_err(|e| format!("{e:#}")),
                    Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
                };
                send(MultiMsg::Finished { run, index, result, elapsed: t0.elapsed() });
            });
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, profiles: &ProfileStore, snippets: &SnippetStore, shared: &Shared) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut start = false;
        let mut retry = None;
        let shown = egui::Window::new("Run on many hosts")
            .open(&mut open)
            .default_width(900.0)
            .default_height(600.0)
            .fit_to_screen(ctx)
            .show(ctx, |ui| {
                let busy = self.running();
                egui::Panel::left("multi-hosts").resizable(true).default_size(240.0).show(ui, |ui| {
                    host_picker(ui, profiles, &mut self.selected, &mut self.filter);
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Command").strong());
                    ui.menu_button("From snippet…", |ui| {
                        if let Some((sn, _)) = snippets::menu(ui, snippets, false) {
                            self.command = sn.command;
                        }
                    });
                });
                ui.add(
                    egui::TextEdit::multiline(&mut self.command)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("uptime && df -h /"),
                );
                let missing = snippets::placeholders(&self.command);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.as_root, "As root (sudo -n)").on_hover_text(
                        "Needs passwordless sudo; hosts that need a password fail instead of asking each time",
                    );
                    ui.label("Parallel");
                    ui.add(egui::DragValue::new(&mut self.parallel).range(1..=64));
                    ui.label("Timeout (s)");
                    ui.add(egui::DragValue::new(&mut self.timeout_secs).range(5..=3600));
                });
                if !missing.is_empty() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Replace the placeholders first: {}", missing.join(", ")),
                    );
                }
                ui.horizontal(|ui| {
                    let can =
                        !busy && !self.selected.is_empty() && !self.command.trim().is_empty() && missing.is_empty();
                    if ui
                        .add_enabled(can, egui::Button::new(format!("Run on {} host(s)", self.selected.len())))
                        .clicked()
                    {
                        start = true;
                    }
                    if busy && ui.button("Stop starting new hosts").clicked() {
                        self.stop.store(true, Ordering::Relaxed);
                    }
                    let failed: Vec<usize> = self
                        .runs
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| {
                            matches!(r.state, HostState::Failed(..))
                                || matches!(&r.state, HostState::Done(o, _) if !o.success())
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if !busy && !failed.is_empty() && ui.button(format!("Retry {} failed", failed.len())).clicked() {
                        retry = Some(failed);
                    }
                    ui.checkbox(&mut self.only_problems, "Only show problems");
                    if !self.runs.is_empty() && ui.button("Copy all output").clicked() {
                        ui.ctx().copy_text(self.report());
                    }
                });
                ui.separator();
                self.results_ui(ui);
                if busy {
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
            });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        if start {
            self.start(profiles, shared, None);
        } else if let Some(list) = retry {
            self.start(profiles, shared, Some(list));
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        let ok = self.runs.iter().filter(|r| matches!(&r.state, HostState::Done(o, _) if o.success())).count();
        let bad = self
            .runs
            .iter()
            .filter(|r| {
                matches!(r.state, HostState::Failed(..)) || matches!(&r.state, HostState::Done(o, _) if !o.success())
            })
            .count();
        if !self.runs.is_empty() {
            ui.label(format!("{ok} succeeded · {bad} failed · {} pending", self.runs.len() - ok - bad));
        }
        egui::ScrollArea::vertical().id_salt("multi-results").auto_shrink([false, false]).show(ui, |ui| {
            for r in &self.runs {
                let problem = matches!(r.state, HostState::Failed(..))
                    || matches!(&r.state, HostState::Done(o, _) if !o.success());
                if self.only_problems && !problem {
                    continue;
                }
                let (status, color) = match &r.state {
                    HostState::Waiting => ("waiting".to_string(), ui.visuals().weak_text_color()),
                    HostState::Running(t) => {
                        (format!("running {}s", t.elapsed().as_secs()), ui.visuals().warn_fg_color)
                    }
                    HostState::Done(o, d) if o.success() => {
                        (format!("✔ ok in {:.1}s", d.as_secs_f32()), crate::look::sem(ui.ctx()).good)
                    }
                    HostState::Done(o, d) => (
                        format!(
                            "✖ exit {} in {:.1}s",
                            o.status.map(|s| s.to_string()).unwrap_or("?".into()),
                            d.as_secs_f32()
                        ),
                        ui.visuals().error_fg_color,
                    ),
                    HostState::Failed(_, d) => {
                        (format!("✖ failed after {:.1}s", d.as_secs_f32()), ui.visuals().error_fg_color)
                    }
                    HostState::Skipped => ("skipped".into(), ui.visuals().weak_text_color()),
                };
                let header = egui::text::LayoutJob::simple_singleline(
                    format!("{}   {status}", r.name),
                    egui::FontId::proportional(14.0),
                    color,
                );
                egui::CollapsingHeader::new(header).id_salt(("res", &r.name)).default_open(problem).show(ui, |ui| {
                    match &r.state {
                        HostState::Done(o, _) => {
                            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                            if !o.stderr.is_empty() {
                                text.push_str(&format!("\n[stderr]\n{}", String::from_utf8_lossy(&o.stderr)));
                            }
                            ui.add(egui::Label::new(RichText::new(text.trim_end()).monospace()).selectable(true));
                        }
                        HostState::Failed(e, _) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        _ => {}
                    }
                });
            }
        });
    }

    /// Plain-text report of every host's result.
    fn report(&self) -> String {
        let mut out = format!("$ {}\n\n", self.command);
        for r in &self.runs {
            match &r.state {
                HostState::Done(o, _) => {
                    out.push_str(&format!(
                        "=== {} (exit {}) ===\n",
                        r.name,
                        o.status.map(|s| s.to_string()).unwrap_or("?".into())
                    ));
                    out.push_str(&String::from_utf8_lossy(&o.stdout));
                    if !o.stderr.is_empty() {
                        out.push_str(&format!("[stderr]\n{}", String::from_utf8_lossy(&o.stderr)));
                    }
                }
                HostState::Failed(e, _) => out.push_str(&format!("=== {} (failed) ===\n{e}\n", r.name)),
                _ => out.push_str(&format!("=== {} (not run) ===\n", r.name)),
            }
            out.push('\n');
        }
        out
    }

    /// Open with a command, and preselect hosts if none are chosen yet.
    pub fn show_with(&mut self, command: String, preselect: Option<ProfileId>) {
        self.open = true;
        if !command.is_empty() {
            self.command = command;
        }
        if self.selected.is_empty()
            && let Some(id) = preselect
        {
            self.selected.insert(id);
        }
    }
}

fn tick(selected: &mut BTreeSet<ProfileId>, id: ProfileId, on: bool) {
    if on {
        selected.insert(id);
    } else {
        selected.remove(&id);
    }
}

/// Hosts by group with checkboxes (a group's box ticks all its members).
/// Once clicked, ↑/↓ (or a name's first letters) move, Space ticks and
/// Ctrl+A ticks every host shown (or unticks them, if all are ticked).
pub fn host_picker(
    ui: &mut egui::Ui,
    profiles: &ProfileStore,
    selected: &mut BTreeSet<ProfileId>,
    filter: &mut String,
) {
    ui.label(RichText::new("Hosts").strong());
    ui.add(egui::TextEdit::singleline(filter).hint_text("Filter").desired_width(f32::INFINITY));
    let groups: Vec<(String, Vec<&Profile>)> = profiles
        .groups()
        .into_iter()
        .map(|group| {
            let members: Vec<&Profile> = profiles
                .profiles
                .iter()
                .filter(|p| p.group.trim() == group && p.rdp.is_none())
                .filter(|p| filter.is_empty() || p.display_name().to_lowercase().contains(&filter.to_lowercase()))
                .collect();
            (group, members)
        })
        .filter(|(_, m)| !m.is_empty())
        .collect();
    let order: Vec<ProfileId> = groups.iter().flat_map(|(_, m)| m.iter().map(|p| p.id)).collect();
    // Where the keyboard is, kept in egui's memory for this list.
    let key = ui.id().with("host-picker-keys");
    let mut cursor: Selection<ProfileId> = ui.data_mut(|d| d.get_temp(key)).unwrap_or_default();
    if cursor.focused
        && !ui.ctx().memory(|m| m.focused().is_some())
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A))
    {
        let all = order.iter().all(|id| selected.contains(id));
        for id in &order {
            tick(selected, *id, !all);
        }
    }
    let (act, scroll) =
        cursor.keys(ui.ctx(), &order, |id| profiles.get(*id).map(|p| p.display_name()).unwrap_or_default());
    if let Some(KeyAction::Toggle | KeyAction::Open(_)) = act
        && let Some(&id) = cursor.selected.first()
    {
        let on = !selected.contains(&id);
        tick(selected, id, on);
    }
    let scroll_to = scroll.and_then(|i| order.get(i).copied());
    let list = egui::ScrollArea::vertical().id_salt("multi-host-list").show(ui, |ui| {
        for (group, members) in groups {
            let all = members.iter().all(|p| selected.contains(&p.id));
            let mut all_now = all;
            let label = if group.is_empty() { "Ungrouped".to_string() } else { group.clone() };
            if ui.checkbox(&mut all_now, RichText::new(label).strong()).changed() {
                for p in &members {
                    tick(selected, p.id, all_now);
                }
            }
            ui.indent(("g", &group), |ui| {
                for p in members {
                    let mut on = selected.contains(&p.id);
                    let r = ui.checkbox(&mut on, p.display_name()).on_hover_text(p.destination());
                    if r.changed() {
                        tick(selected, p.id, on);
                    }
                    if r.clicked() {
                        cursor.only(&p.id);
                        cursor.focused = true;
                    }
                    if cursor.focused && cursor.contains(&p.id) {
                        ui.painter().rect_stroke(
                            r.rect.expand(1.0),
                            2.0,
                            ui.visuals().selection.stroke,
                            egui::StrokeKind::Outside,
                        );
                    }
                    if scroll_to == Some(p.id) {
                        r.scroll_to_me(None);
                    }
                }
            });
        }
    });
    cursor.track_focus(ui, list.inner_rect);
    ui.data_mut(|d| d.insert_temp(key, cursor));
}
