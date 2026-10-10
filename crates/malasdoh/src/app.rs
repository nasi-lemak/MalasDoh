//! The main window: connection list, session tabs, and the dialogs that answer
//! a connection's questions.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::profile::{AuthMethod, Profile, ProfileId, ProfileStore};
use malasdoh_core::secrets::{OsKeychain, SecretKey, SecretStore};
use malasdoh_core::ssh::{SecretAnswer, SecretPrompt};
use malasdoh_core::validate::{CheckWhen, ValidationRule, builtin_rules};
use malasdoh_core::{AppPaths, ssh_config};

use crate::bridge::{AppEvent, EventKind, Events, MULTI_RUN, Question, SessionId};
use crate::multirun::MultiRun;
use crate::session::{ConnState, Pane, Session, Shared};
use crate::settings::Settings;
use crate::snippets::{self, Snippet, SnippetStore, Use};

pub struct App {
    shared: Shared,
    _rt: tokio::runtime::Runtime,
    rx: mpsc::Receiver<AppEvent>,
    settings: Settings,
    profiles: ProfileStore,
    sessions: Vec<Session>,
    active: Option<SessionId>,
    next_session: SessionId,
    filter: String,
    form: Option<ProfileForm>,
    confirm_delete: Option<ProfileId>,
    confirm_close: Option<SessionId>,
    show_settings: bool,
    answer: AnswerState,
    toast: Option<(String, bool, Instant)>,
    close_warned: Option<Instant>,
    settings_tab: SettingsTab,
    snippets: SnippetStore,
    show_snippets: bool,
    pending_snippet: Option<PendingSnippet>,
    multi: MultiRun,
}

/// A snippet waiting for its `{{placeholders}}` to be filled in.
struct PendingSnippet {
    session: Option<SessionId>,
    snippet: Snippet,
    values: Vec<(String, String)>,
    how: Use,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    General,
    Checks,
}

/// Editor for the config-check rules.
fn checks_ui(ui: &mut egui::Ui, rules: &mut Vec<ValidationRule>) {
    ui.label(
        "When you save a file whose path matches a rule, its check runs. \
         \"Before\" checks a copy and writes nothing if it fails; \"After\" writes, checks, and puts the original back on failure. \
         {file} is the file's path. The first matching rule wins.",
    );
    ui.add_space(4.0);
    let mut remove = None;
    let mut swap = None;
    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
        egui::Grid::new("rules").striped(true).num_columns(8).spacing([6.0, 4.0]).show(ui, |ui| {
            for h in ["On", "Name", "Path pattern", "Check command", "When", "Then reload", "", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for (i, r) in rules.iter_mut().enumerate() {
                ui.checkbox(&mut r.enabled, "");
                ui.add(egui::TextEdit::singleline(&mut r.name).desired_width(140.0));
                ui.add(
                    egui::TextEdit::singleline(&mut r.pattern)
                        .desired_width(170.0)
                        .hint_text("/etc/nginx/** or *.json"),
                );
                ui.add(egui::TextEdit::singleline(&mut r.check).desired_width(250.0).font(egui::TextStyle::Monospace));
                egui::ComboBox::from_id_salt(("when", i))
                    .width(70.0)
                    .selected_text(match r.when {
                        CheckWhen::BeforeWrite => "Before",
                        CheckWhen::AfterWrite => "After",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut r.when, CheckWhen::BeforeWrite, "Before writing (on a copy)");
                        ui.selectable_value(&mut r.when, CheckWhen::AfterWrite, "After writing (roll back)");
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut r.reload)
                        .desired_width(190.0)
                        .hint_text("systemctl reload …")
                        .font(egui::TextStyle::Monospace),
                );
                if ui
                    .add_enabled(i > 0, egui::Button::new("⬆").small())
                    .on_hover_text("Move up (earlier rules win)")
                    .clicked()
                {
                    swap = Some(i);
                }
                if ui.small_button("✖").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    });
    if let Some(i) = swap {
        rules.swap(i - 1, i);
    }
    if let Some(i) = remove {
        rules.remove(i);
    }
    ui.horizontal(|ui| {
        if ui.button("+ Add rule").clicked() {
            rules.push(ValidationRule { name: "custom".into(), ..Default::default() });
        }
        if ui.button("Reset to defaults").clicked() {
            *rules = builtin_rules();
        }
    });
    ui.label(
        RichText::new("Built-in checks: builtin:json, builtin:toml, builtin:yaml (no server tools needed).")
            .weak()
            .small(),
    );
}

/// Text typed into the current question dialog.
#[derive(Default)]
struct AnswerState {
    secret: String,
    remember: bool,
    fields: Vec<String>,
}

struct ProfileForm {
    profile: Profile,
    port: String,
    is_new: bool,
    error: Option<String>,
    forget_password: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: AppPaths) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("malasdoh-net")
            .build()
            .expect("tokio runtime");
        let (events, rx) = Events::new(cc.egui_ctx.clone());
        let settings = Settings::load(&paths.settings_file());
        cc.egui_ctx.set_visuals(if settings.dark_mode { egui::Visuals::dark() } else { egui::Visuals::light() });
        let mut toast = None;
        let profiles = ProfileStore::load(&paths.profiles_file()).unwrap_or_else(|e| {
            toast = Some((format!("Could not read saved connections: {e:#}"), true, Instant::now()));
            ProfileStore::default()
        });
        let snippets_path = paths.config_dir.join("snippets.toml");
        // Copies made for external editors are kept a week, then tidied away.
        let cache = paths.edit_cache_dir();
        rt.spawn_blocking(move || {
            malasdoh_core::paths::remove_older_than(&cache, std::time::Duration::from_secs(7 * 86400))
        });
        let shared = Shared {
            rt: rt.handle().clone(),
            events,
            secrets: Arc::new(OsKeychain) as Arc<dyn SecretStore>,
            known_hosts: KnownHosts::standard(paths.known_hosts_file()),
            paths,
        };
        Self {
            shared,
            _rt: rt,
            rx,
            settings,
            profiles,
            sessions: Vec::new(),
            active: None,
            next_session: 1,
            filter: String::new(),
            form: None,
            confirm_delete: None,
            confirm_close: None,
            show_settings: false,
            answer: AnswerState::default(),
            toast,
            close_warned: None,
            settings_tab: SettingsTab::General,
            snippets: SnippetStore::load(&snippets_path),
            show_snippets: false,
            pending_snippet: None,
            multi: MultiRun::default(),
        }
    }

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), error, Instant::now()));
    }

    fn save_profiles(&mut self) {
        if let Err(e) = self.profiles.save(&self.shared.paths.profiles_file()) {
            self.notify(format!("Could not save connections: {e:#}"), true);
        }
    }

    fn connect(&mut self, id: ProfileId) {
        let chain = match self.profiles.chain(id) {
            Ok(c) => c,
            Err(e) => return self.notify(format!("{e:#}"), true),
        };
        let sid = self.next_session;
        self.next_session += 1;
        let mut s = Session::new(sid, chain);
        s.connect(&self.shared);
        self.sessions.push(s);
        self.active = Some(sid);
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if ev.session == MULTI_RUN {
                match ev.kind {
                    EventKind::Question(q) => self.multi.questions.push_back(q),
                    EventKind::Multi(m) => self.multi.handle(m),
                    _ => {}
                }
                continue;
            }
            let Some(s) = self.sessions.iter_mut().find(|s| s.id == ev.session) else { continue };
            match ev.kind {
                EventKind::Status(msg) => {
                    if let ConnState::Connecting(_) = s.state {
                        s.state = ConnState::Connecting(msg);
                    }
                }
                EventKind::Question(q) => s.questions.push_back(q),
                EventKind::Session(msg) => s.handle(msg, &self.shared, &self.settings),
                EventKind::Multi(_) => {}
            }
        }
    }

    fn import_ssh_config(&mut self) {
        let Some(path) = malasdoh_core::paths::user_ssh_dir().map(|d| d.join("config")) else {
            return self.notify("No home directory found", true);
        };
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        match ssh_config::import_file(&path, &user) {
            Ok(found) => {
                let mut added = 0;
                // Map imported ids to existing ones so re-importing is harmless.
                let mut remap = std::collections::HashMap::new();
                for p in &found {
                    if let Some(existing) = self.profiles.profiles.iter().find(|e| e.name == p.name && e.host == p.host)
                    {
                        remap.insert(p.id, existing.id);
                    }
                }
                for mut p in found {
                    if remap.contains_key(&p.id) {
                        continue;
                    }
                    p.jump = p.jump.map(|j| remap.get(&j).copied().unwrap_or(j));
                    if p.user.is_empty() {
                        p.user = user.clone();
                    }
                    if self.profiles.upsert(p).is_ok() {
                        added += 1;
                    }
                }
                self.save_profiles();
                self.notify(format!("Imported {added} connection(s) from {}", path.display()), false);
            }
            Err(e) => self.notify(format!("Could not read {}: {e}", path.display()), true),
        }
    }

    // -----------------------------------------------------------------------

    fn menu_ui(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Connection", |ui| {
                if ui.button("New connection…").clicked() {
                    self.form = Some(ProfileForm::new(Profile::default(), true));
                    ui.close();
                }
                if ui.button("Import from ~/.ssh/config").clicked() {
                    self.import_ssh_config();
                    ui.close();
                }
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.show_settings = true;
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Tools", |ui| {
                if ui.button("Run on many hosts…").clicked() {
                    self.multi.open = true;
                    ui.close();
                }
                if ui.button("Snippets…").clicked() {
                    self.show_snippets = true;
                    ui.close();
                }
            });
            ui.separator();
            let mut switch = None;
            let mut close = None;
            for s in &self.sessions {
                let selected = self.active == Some(s.id);
                let mut job = egui::text::LayoutJob::default();
                let body = egui::TextStyle::Button.resolve(ui.style());
                job.append("• ", 0.0, egui::TextFormat::simple(body.clone(), s.status_color()));
                let name =
                    if s.has_unsaved() { format!("{} *", s.profile.display_name()) } else { s.profile.display_name() };
                job.append(&name, 0.0, egui::TextFormat::simple(body, ui.visuals().text_color()));
                if ui.selectable_label(selected, job).clicked() {
                    switch = Some(s.id);
                }
                if selected && ui.small_button("✖").on_hover_text("Close this connection").clicked() {
                    close = Some(s.id);
                }
            }
            if let Some(id) = switch {
                self.active = Some(id);
            }
            if let Some(id) = close {
                if self.sessions.iter().any(|s| s.id == id && s.has_unsaved()) {
                    self.confirm_close = Some(id);
                } else {
                    self.close_session(id);
                }
            }
        });
    }

    fn close_session(&mut self, id: SessionId) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[i].disconnect(&self.shared);
            self.sessions.remove(i);
        }
        if self.active == Some(id) {
            self.active = self.sessions.last().map(|s| s.id);
        }
    }

    fn sidebar_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Connections");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("+").on_hover_text("New connection").clicked() {
                    self.form = Some(ProfileForm::new(Profile::default(), true));
                }
            });
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search").desired_width(f32::INFINITY));
        ui.separator();
        if self.profiles.profiles.is_empty() {
            ui.label("No saved connections yet.");
            if ui.button("New connection…").clicked() {
                self.form = Some(ProfileForm::new(Profile::default(), true));
            }
            if ui.button("Import from ~/.ssh/config").clicked() {
                self.import_ssh_config();
            }
            return;
        }
        let filter = self.filter.to_lowercase();
        let mut connect = None;
        let mut edit = None;
        let mut duplicate = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for group in self.profiles.groups() {
                let mut members: Vec<&Profile> = self
                    .profiles
                    .profiles
                    .iter()
                    .filter(|p| p.group.trim() == group)
                    .filter(|p| {
                        filter.is_empty()
                            || p.display_name().to_lowercase().contains(&filter)
                            || p.host.to_lowercase().contains(&filter)
                    })
                    .collect();
                if members.is_empty() {
                    continue;
                }
                members.sort_by_key(|p| p.display_name().to_lowercase());
                let title = if group.is_empty() { "Ungrouped".to_string() } else { group.clone() };
                egui::CollapsingHeader::new(title).default_open(true).show(ui, |ui| {
                    for p in members {
                        let r = ui.selectable_label(false, format!("🖥 {}", p.display_name())).on_hover_text(format!(
                            "{}\n{}{}",
                            p.destination(),
                            p.auth.label(),
                            if p.notes.is_empty() { String::new() } else { format!("\n{}", p.notes) }
                        ));
                        if r.double_clicked() {
                            connect = Some(p.id);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Connect").clicked() {
                                connect = Some(p.id);
                                ui.close();
                            }
                            if ui.button("Edit…").clicked() {
                                edit = Some(p.id);
                                ui.close();
                            }
                            if ui.button("Duplicate").clicked() {
                                duplicate = Some(p.id);
                                ui.close();
                            }
                            if ui.button("Copy ssh command").clicked() {
                                let port = if p.port == 22 { String::new() } else { format!(" -p {}", p.port) };
                                ui.ctx().copy_text(format!("ssh {}@{}{port}", p.user, p.host));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Delete…").clicked() {
                                self.confirm_delete = Some(p.id);
                                ui.close();
                            }
                        });
                    }
                });
            }
        });
        ui.label(RichText::new("Double-click to connect").weak().small());
        if let Some(id) = connect {
            self.connect(id);
        }
        if let Some(p) = edit.and_then(|id| self.profiles.get(id).cloned()) {
            self.form = Some(ProfileForm::new(p, false));
        }
        if let Some(mut p) = duplicate.and_then(|id| self.profiles.get(id).cloned()) {
            p.id = ProfileId::new();
            p.name = format!("{} (copy)", p.display_name());
            self.form = Some(ProfileForm::new(p, true));
        }
    }

    fn session_ui(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.active.and_then(|id| self.sessions.iter().position(|s| s.id == id)) else {
            return self.welcome_ui(ui);
        };
        let shared = self.shared.clone();
        let settings = self.settings.clone();
        let mut forget = false;
        let s = &mut self.sessions[idx];
        s.check_alive();
        s.poll_external(&shared, &settings);

        // Connection status bar.
        let mut reconnect = false;
        match &s.state {
            ConnState::Connecting(msg) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(msg);
                });
            }
            ConnState::Connected => {}
            ConnState::Failed { error, forgettable_key } => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new("Connection failed").strong());
                    ui.label(error);
                    ui.horizontal(|ui| {
                        reconnect = ui.button("Try again").clicked();
                        if *forgettable_key
                            && ui
                                .button("Remove the old key and try again")
                                .on_hover_text("Only do this if you know the server's key really changed")
                                .clicked()
                        {
                            forget = true;
                            reconnect = true;
                        }
                    });
                });
            }
            ConnState::Disconnected => {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, "Disconnected. Open files keep your edits.");
                    reconnect = ui.button("Reconnect").clicked();
                });
            }
        }
        if forget {
            s.forget_host_key(&shared);
        }
        if reconnect {
            s.connect(&shared);
        }

        if s.state != ConnState::Connected && s.terms.is_empty() && s.editors.is_empty() {
            return;
        }

        egui::Panel::left(egui::Id::new(("browser", s.id))).resizable(true).default_size(320.0).show(ui, |ui| {
            let actions = s.browser.ui(ui, settings.confirm_delete);
            for a in actions {
                s.browser_action(a, &shared);
            }
        });

        let mut chosen_snippet = None;
        // Pane tabs: terminals, then open files.
        ui.horizontal_wrapped(|ui| {
            let mut close_term = None;
            let mut close_editor = None;
            for (n, t) in s.terms.iter().enumerate() {
                let label = format!("⌨ Terminal {}", n + 1);
                if ui.selectable_label(s.active == Some(Pane::Term(t.id)), label).clicked() {
                    s.active = Some(Pane::Term(t.id));
                }
                if s.active == Some(Pane::Term(t.id)) && ui.small_button("✖").clicked() {
                    close_term = Some(t.id);
                }
            }
            for e in &s.editors {
                if ui
                    .selectable_label(s.active == Some(Pane::Editor(e.id)), format!("📝 {}", e.title()))
                    .on_hover_text(&e.session.path)
                    .clicked()
                {
                    s.active = Some(Pane::Editor(e.id));
                }
                if s.active == Some(Pane::Editor(e.id)) && ui.small_button("✖").clicked() {
                    close_editor = Some(e.id);
                }
            }
            if s.state == ConnState::Connected && ui.button("+ Terminal").clicked() {
                s.open_terminal(&shared, &settings);
            }
            let fw = format!("🔌 Forwards {}/{}", s.forwards.running(), s.forwards.len());
            if ui.selectable_label(s.forwards.open, fw).on_hover_text("Port forwarding (-L, -R, SOCKS)").clicked() {
                s.forwards.open = !s.forwards.open;
            }
            ui.menu_button("☰ Snippets", |ui| {
                let can_type = s.terms.iter().any(|t| t.is_live());
                if let Some((sn, how)) = snippets::menu(ui, &self.snippets, can_type) {
                    chosen_snippet = Some((sn, how));
                }
            });
            if let Some(id) = close_term {
                s.close_terminal(id);
            }
            if let Some(id) = close_editor {
                if s.editors.iter().any(|e| e.id == id && e.is_modified()) {
                    s.toast = Some((
                        "This file has unsaved changes. Save or reload it before closing.".into(),
                        true,
                        Instant::now(),
                    ));
                } else {
                    s.close_editor(id);
                }
            }
        });
        ui.separator();
        s.pane_ui(ui, &shared, &settings);
        let save_forwards = s.forwards_ui(ui.ctx(), &shared).map(|specs| (s.profile.id, specs));
        let (sid, pid) = (s.id, s.profile.id);

        if let Some((msg, err, _)) = s.toast.take() {
            self.toast = Some((msg, err, Instant::now()));
        }
        if let Some((snippet, how)) = chosen_snippet {
            self.use_snippet(Some(sid), Some(pid), snippet, how, Vec::new());
        }
        if let Some((id, specs)) = save_forwards
            && let Some(mut p) = self.profiles.get(id).cloned()
        {
            p.forwards = specs;
            if self.profiles.upsert(p).is_ok() {
                self.save_profiles();
                self.notify("Saved port forwards to the connection.", false);
            }
        }
    }

    /// Use a snippet, asking for its placeholders first if it has any.
    fn use_snippet(
        &mut self,
        session: Option<SessionId>,
        profile: Option<ProfileId>,
        snippet: Snippet,
        how: Use,
        values: Vec<(String, String)>,
    ) {
        let names = snippets::placeholders(&snippet.command);
        let filled: std::collections::HashMap<String, String> = values.iter().cloned().collect();
        if names.iter().any(|n| !filled.contains_key(n)) {
            let values = names.into_iter().map(|n| (n, String::new())).collect();
            self.pending_snippet = Some(PendingSnippet { session, snippet, values, how });
            return;
        }
        let command = snippets::fill(&snippet.command, &filled);
        match how {
            Use::Multi => self.multi.show_with(command, profile),
            Use::Insert | Use::Run => {
                if let Some(s) = session.and_then(|id| self.sessions.iter_mut().find(|s| s.id == id)) {
                    let mut bytes = command.into_bytes();
                    if how == Use::Run {
                        bytes.push(b'\r');
                    }
                    s.send_to_terminal(bytes);
                }
            }
        }
    }

    fn snippet_windows_ui(&mut self, ctx: &egui::Context) {
        if self.show_snippets {
            let mut open = true;
            let mut changed = false;
            egui::Window::new("Snippets").open(&mut open).default_width(760.0).show(ctx, |ui| {
                changed = snippets::manager_ui(ui, &mut self.snippets);
            });
            self.show_snippets = open;
            if changed && let Err(e) = self.snippets.save(&self.shared.paths.config_dir.join("snippets.toml")) {
                self.notify(format!("Could not save snippets: {e:#}"), true);
            }
        }
        let mut result = None;
        if let Some(p) = &mut self.pending_snippet {
            egui::Modal::new("snippet-values".into()).show(ctx, |ui| {
                ui.heading(&p.snippet.name);
                ui.monospace(&p.snippet.command);
                egui::Grid::new("snippet-values-grid").num_columns(2).show(ui, |ui| {
                    for (i, (name, value)) in p.values.iter_mut().enumerate() {
                        ui.label(name.as_str());
                        let r = ui.text_edit_singleline(value);
                        if i == 0 && !r.has_focus() && value.is_empty() {
                            r.request_focus();
                        }
                        ui.end_row();
                    }
                });
                let filled: std::collections::HashMap<String, String> = p.values.iter().cloned().collect();
                ui.label(RichText::new(snippets::fill(&p.snippet.command, &filled)).monospace().weak());
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        result = Some(false);
                    }
                });
            });
        }
        match result {
            Some(true) => {
                let p = self.pending_snippet.take().unwrap();
                let profile = p.session.and_then(|id| self.sessions.iter().find(|s| s.id == id)).map(|s| s.profile.id);
                self.use_snippet(p.session, profile, p.snippet, p.how, p.values);
            }
            Some(false) => self.pending_snippet = None,
            None => {}
        }
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.heading("MalasDoh");
            ui.label("SSH terminal, file browser and safe remote editing.");
            ui.add_space(20.0);
            if ui.button("New connection…").clicked() {
                self.form = Some(ProfileForm::new(Profile::default(), true));
            }
            if ui.button("Import from ~/.ssh/config").clicked() {
                self.import_ssh_config();
            }
            ui.add_space(20.0);
            ui.label(
                RichText::new("Every save shows a diff, checks nobody else changed the file, and keeps a backup.")
                    .weak(),
            );
        });
    }

    // -----------------------------------------------------------------------

    fn question_ui(&mut self, ctx: &egui::Context) {
        // Questions from the active session first, then any other.
        let order: Vec<usize> = {
            let mut v: Vec<usize> = (0..self.sessions.len()).collect();
            v.sort_by_key(|&i| Some(self.sessions[i].id) != self.active);
            v
        };
        // A session's questions first, then those from "Run on many hosts".
        let target = order.into_iter().find(|&i| !self.sessions[i].questions.is_empty());
        if target.is_none() && self.multi.questions.is_empty() {
            return;
        }
        let session_name = match target {
            Some(i) => self.sessions[i].profile.display_name(),
            None => "Run on many hosts".to_string(),
        };
        let q = match target {
            Some(i) => self.sessions[i].questions.front().unwrap(),
            None => self.multi.questions.front().unwrap(),
        };
        let mut done: Option<bool> = None;
        let a = &mut self.answer;
        egui::Modal::new("question".into()).show(ctx, |ui| {
            ui.set_max_width(520.0);
            match q {
                Question::HostKey(p, _) => {
                    ui.heading("New server");
                    ui.label(format!("This is the first time you connect to {}:{} ({session_name}).", p.host, p.port));
                    ui.label("Check that this fingerprint is the server's before trusting it:");
                    ui.add_space(4.0);
                    ui.monospace(&p.algorithm);
                    ui.add(egui::Label::new(RichText::new(&p.fingerprint).monospace().strong()).selectable(true));
                    if !p.other_keys.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("You already trust a different key type for this host ({}). The server may have added a new key, or this may not be the same server.", p.other_keys.join(", ")),
                        );
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Trust and connect").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                }
                Question::Secret(p, _) => {
                    let (title, detail, retry) = match p {
                        SecretPrompt::Password { user, host, retry } => ("Password", format!("Password for {user}@{host}"), *retry),
                        SecretPrompt::Passphrase { key_path, retry } => ("Key passphrase", format!("Passphrase for {key_path}"), *retry),
                    };
                    ui.heading(title);
                    if retry {
                        ui.colored_label(ui.visuals().error_fg_color, "That did not work. Try again.");
                    }
                    ui.label(detail);
                    let r = ui.add(egui::TextEdit::singleline(&mut a.secret).password(true));
                    r.request_focus();
                    ui.checkbox(&mut a.remember, "Remember in the system keychain");
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        done = Some(true);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                }
                Question::Keyboard(p, _) => {
                    ui.heading(if p.name.is_empty() { "The server asks" } else { &p.name });
                    if !p.instructions.is_empty() {
                        ui.label(&p.instructions);
                    }
                    a.fields.resize(p.prompts.len(), String::new());
                    for (i, (prompt, echo)) in p.prompts.iter().enumerate() {
                        ui.label(prompt);
                        let r = ui.add(egui::TextEdit::singleline(&mut a.fields[i]).password(!echo));
                        if i == 0 {
                            r.request_focus();
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                }
                Question::Sudo { host, retry, .. } => {
                    ui.heading("sudo password");
                    if *retry {
                        ui.colored_label(ui.visuals().error_fg_color, "sudo did not accept that password.");
                    }
                    ui.label(format!("Your password on {host}, for sudo. It is kept in memory for this connection only."));
                    let r = ui.add(egui::TextEdit::singleline(&mut a.secret).password(true));
                    r.request_focus();
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        done = Some(true);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                }
            }
        });
        let Some(ok) = done else { return };
        let q = match target {
            Some(i) => self.sessions[i].questions.pop_front().unwrap(),
            None => self.multi.questions.pop_front().unwrap(),
        };
        let a = std::mem::take(&mut self.answer);
        match q {
            Question::HostKey(_, tx) => {
                let _ = tx.send(ok);
            }
            Question::Secret(_, tx) => {
                let _ = tx.send(ok.then_some(SecretAnswer { secret: a.secret, remember: a.remember }));
            }
            Question::Keyboard(_, tx) => {
                let _ = tx.send(ok.then_some(a.fields));
            }
            Question::Sudo { reply, .. } => {
                let _ = reply.send(ok.then_some(a.secret));
            }
        }
    }

    fn form_ui(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.form else { return };
        let mut result: Option<bool> = None;
        let others: Vec<(ProfileId, String)> = self
            .profiles
            .profiles
            .iter()
            .filter(|p| p.id != form.profile.id)
            .map(|p| (p.id, p.display_name()))
            .collect();
        let groups = self.profiles.groups();
        egui::Modal::new("profile-form".into()).show(ctx, |ui| {
            ui.set_min_width(460.0);
            ui.heading(if form.is_new { "New connection" } else { "Edit connection" });
            egui::Grid::new("profile-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                let p = &mut form.profile;
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut p.name).hint_text("prod-web-1"));
                ui.end_row();
                ui.label("Host");
                ui.add(egui::TextEdit::singleline(&mut p.host).hint_text("10.0.0.5 or web.example.com"));
                ui.end_row();
                ui.label("Port");
                ui.add(egui::TextEdit::singleline(&mut form.port).desired_width(60.0));
                ui.end_row();
                ui.label("User");
                ui.text_edit_singleline(&mut p.user);
                ui.end_row();
                ui.label("Authentication");
                ui.vertical(|ui| {
                    egui::ComboBox::from_id_salt("auth").selected_text(p.auth.label()).show_ui(ui, |ui| {
                        for option in [
                            AuthMethod::Auto,
                            AuthMethod::Agent,
                            AuthMethod::Key { path: String::new() },
                            AuthMethod::Password,
                        ] {
                            let same = std::mem::discriminant(&option) == std::mem::discriminant(&p.auth);
                            if ui.selectable_label(same, option.label()).clicked() && !same {
                                p.auth = option;
                            }
                        }
                    });
                    match &mut p.auth {
                        AuthMethod::Key { path } => {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(path)
                                        .hint_text("~/.ssh/id_ed25519")
                                        .desired_width(260.0),
                                );
                                if ui.button("Browse…").clicked()
                                    && let Some(f) =
                                        rfd::FileDialog::new().set_title("Choose a private key").pick_file()
                                {
                                    *path = f.display().to_string();
                                }
                            });
                            ui.label(RichText::new("OpenSSH, PEM or PuTTY .ppk keys").weak().small());
                        }
                        AuthMethod::Auto => {
                            ui.label(RichText::new("SSH agent, then ~/.ssh/id_* keys, then password").weak().small());
                        }
                        _ => {}
                    }
                    if !form.is_new {
                        ui.checkbox(&mut form.forget_password, "Forget saved password");
                    }
                });
                ui.end_row();
                ui.label("Jump host");
                let current = p
                    .jump
                    .and_then(|j| others.iter().find(|(id, _)| *id == j))
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| "None (direct)".into());
                egui::ComboBox::from_id_salt("jump").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut p.jump, None, "None (direct)");
                    for (id, name) in &others {
                        ui.selectable_value(&mut p.jump, Some(*id), name);
                    }
                });
                ui.end_row();
                ui.label("Group");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut p.group).hint_text("Production").desired_width(200.0));
                    egui::ComboBox::from_id_salt("groups").selected_text("…").width(30.0).show_ui(ui, |ui| {
                        for g in groups.iter().filter(|g| !g.is_empty()) {
                            if ui.selectable_label(false, g).clicked() {
                                p.group = g.clone();
                            }
                        }
                    });
                });
                ui.end_row();
                ui.label("Start folder");
                ui.add(egui::TextEdit::singleline(&mut p.start_dir).hint_text("home directory"));
                ui.end_row();
                ui.label("Backups");
                ui.checkbox(&mut p.remote_backup, "Also keep a .bak copy next to edited files on the server");
                ui.end_row();
                ui.label("Notes");
                ui.add(egui::TextEdit::multiline(&mut p.notes).desired_rows(2));
                ui.end_row();
            });
            let p = &mut form.profile;
            egui::CollapsingHeader::new(format!("Port forwards ({})", p.forwards.len())).show(ui, |ui| {
                let mut remove = None;
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for (i, f) in p.forwards.iter_mut().enumerate() {
                        egui::Grid::new(("fwd", i)).num_columns(2).show(ui, |ui| crate::forwards::spec_editor(ui, f));
                        if let Err(e) = f.validate() {
                            ui.colored_label(ui.visuals().warn_fg_color, e.to_string());
                        }
                        if ui.small_button("Remove this forward").clicked() {
                            remove = Some(i);
                        }
                        ui.separator();
                    }
                });
                if let Some(i) = remove {
                    p.forwards.remove(i);
                }
                if ui.button("+ Add forward").clicked() {
                    p.forwards.push(Default::default());
                }
            });
            if let Some(e) = &form.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.horizontal(|ui| {
                if ui.button(if form.is_new { "Save" } else { "Save changes" }).clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    result = Some(false);
                }
            });
        });
        match result {
            Some(false) => self.form = None,
            Some(true) => {
                let form = self.form.as_mut().unwrap();
                match form.port.trim().parse::<u16>() {
                    Ok(port) => form.profile.port = port,
                    Err(_) => {
                        form.error = Some("Port must be a number between 1 and 65535".into());
                        return;
                    }
                }
                form.profile.host = form.profile.host.trim().to_string();
                form.profile.user = form.profile.user.trim().to_string();
                let profile = form.profile.clone();
                let forget = form.forget_password;
                match self.profiles.upsert(profile.clone()) {
                    Ok(()) => {
                        if forget {
                            let secrets = self.shared.secrets.clone();
                            self.shared.rt.spawn_blocking(move || secrets.delete(&SecretKey::Password(profile.id)));
                        }
                        self.form = None;
                        self.save_profiles();
                    }
                    Err(e) => form.error = Some(format!("{e:#}")),
                }
            }
            None => {}
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.confirm_delete {
            let name = self.profiles.get(id).map(|p| p.display_name()).unwrap_or_default();
            let mut close = false;
            egui::Modal::new("confirm-delete-profile".into()).show(ctx, |ui| {
                ui.heading(format!("Delete “{name}”?"));
                ui.label("The saved connection is removed. Nothing on the server is touched.");
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        let orphaned = self.profiles.remove(id);
                        let secrets = self.shared.secrets.clone();
                        self.shared.rt.spawn_blocking(move || secrets.delete(&SecretKey::Password(id)));
                        self.save_profiles();
                        if !orphaned.is_empty() {
                            self.notify(
                                format!(
                                    "{} connection(s) used it as a jump host and now connect directly",
                                    orphaned.len()
                                ),
                                false,
                            );
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_delete = None;
            }
        }
        if let Some(id) = self.confirm_close {
            let mut result = None;
            egui::Modal::new("confirm-close".into()).show(ctx, |ui| {
                ui.heading("Unsaved changes");
                ui.label("Some open files have changes that are not saved to the server. Close anyway and lose them?");
                ui.horizontal(|ui| {
                    if ui.button("Close and discard").clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => {
                    self.close_session(id);
                    self.confirm_close = None;
                }
                Some(false) => self.confirm_close = None,
                None => {}
            }
        }
    }

    fn settings_ui(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let before = self.settings.clone();
        let mut open = true;
        let paths = self.shared.paths.clone();
        let tab = &mut self.settings_tab;
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(true).default_width(1040.0).show(
            ctx,
            |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(tab, SettingsTab::General, "General");
                    ui.selectable_value(tab, SettingsTab::Checks, "Config checks");
                });
                ui.separator();
                let s = &mut self.settings;
                match tab {
                    SettingsTab::General => {
                        ui.add(egui::Slider::new(&mut s.terminal_font_size, 9.0..=28.0).text("Terminal font size"));
                        ui.add(egui::Slider::new(&mut s.editor_font_size, 9.0..=28.0).text("Editor font size"));
                        ui.checkbox(&mut s.review_before_save, "Show changes and ask before saving a remote file");
                        ui.checkbox(&mut s.confirm_delete, "Ask before deleting remote files");
                        ui.checkbox(&mut s.dark_mode, "Dark mode");
                        ui.label("External editor command ({file} = path; empty = system default):");
                        ui.add(egui::TextEdit::singleline(&mut s.external_editor).hint_text("code --wait {file}"));
                        ui.separator();
                        ui.label(
                            RichText::new(format!("Settings and connections: {}", paths.config_dir.display()))
                                .weak()
                                .small(),
                        );
                        ui.label(
                            RichText::new(format!("Local backups: {}", paths.backups_dir().display())).weak().small(),
                        );
                    }
                    SettingsTab::Checks => checks_ui(ui, &mut s.validation_rules),
                }
            },
        );
        self.show_settings = open;
        if self.settings != before {
            ctx.set_visuals(if self.settings.dark_mode { egui::Visuals::dark() } else { egui::Visuals::light() });
            if let Err(e) = self.settings.save(&self.shared.paths.settings_file()) {
                self.notify(format!("Could not save settings: {e:#}"), true);
            }
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((msg, err, at)) = &self.toast {
                if at.elapsed() > Duration::from_secs(if *err { 15 } else { 6 }) {
                    self.toast = None;
                } else {
                    let color = if *err { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    ui.colored_label(color, msg);
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("MalasDoh {}", env!("CARGO_PKG_VERSION"))).weak().small());
            });
        });
    }
}

impl ProfileForm {
    fn new(profile: Profile, is_new: bool) -> Self {
        Self { port: profile.port.to_string(), profile, is_new, error: None, forget_password: false }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        let ctx = ui.ctx().clone();

        // Closing the window with unsaved edits needs a second click.
        if ctx.input(|i| i.viewport().close_requested())
            && self.sessions.iter().any(|s| s.has_unsaved())
            && self.close_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(10))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_warned = Some(Instant::now());
            self.notify("Some files have unsaved changes. Close the window again to quit and lose them.", true);
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_ui(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        egui::Panel::left("connections").resizable(true).default_size(230.0).show(ui, |ui| self.sidebar_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| self.session_ui(ui));

        self.form_ui(&ctx);
        self.confirm_ui(&ctx);
        self.settings_ui(&ctx);
        self.snippet_windows_ui(&ctx);
        self.multi.ui(&ctx, &self.profiles, &self.snippets, &self.shared);
        self.question_ui(&ctx);

        // Keep polling while something is in flight.
        let busy = self.sessions.iter().any(|s| {
            matches!(s.state, ConnState::Connecting(_)) || !s.browser.transfers.iter().all(|t| t.result.is_some())
        });
        if busy {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else if !self.sessions.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}
