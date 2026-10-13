//! The main window: connection list, session tabs, and the dialogs that answer
//! a connection's questions.

mod backup;
mod teleport;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::profile::{AuthMethod, Profile, ProfileId, ProfileStore};
use malasdoh_core::secrets::{OsKeychain, SecretKey, SecretStore};
use malasdoh_core::ssh::{SecretAnswer, SecretPrompt};
use malasdoh_core::validate::{CheckWhen, ValidationRule, builtin_rules};
use malasdoh_core::{AppPaths, ssh_config};

use crate::bridge::{AppEvent, EventKind, Events, MULTI_RUN, Question, SessionId, SudoAnswer};
use crate::compare::Compares;
use crate::journal_ui::JournalWindow;
use crate::keys_ui::KeysWindow;
use crate::multiedit::MultiEdit;
use crate::multirun::MultiRun;
use crate::restore::LastSession;
use crate::session::{ConnState, Pane, Session, Shared};
use crate::settings::Settings;
use crate::snippets::{self, Snippet, SnippetStore, Use};
use malasdoh_core::journal::Journal;

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
    multi_edit: MultiEdit,
    show_sidebar: bool,
    last_local_dir: Option<std::path::PathBuf>,
    cross: Option<CrossCopy>,
    compares: Compares,
    journal: Journal,
    journal_win: JournalWindow,
    keys_win: KeysWindow,
    teleport: malasdoh_core::teleport::TeleportStore,
    teleport_win: crate::teleport_ui::TeleportWindow,
    export_dlg: Option<backup::ExportDialog>,
    import_dlg: Option<backup::ImportDialog>,
    /// "Type into several terminals": the sessions that get what is typed.
    broadcast: Option<Vec<SessionId>>,
    /// What was open when MalasDoh last closed, until it is reopened.
    last: LastSession,
    /// The last state written to disk, so it is only written when it changes.
    last_written: String,
    last_persist: Instant,
    last_draft_save: Instant,
    /// A connection was opened in this run; before that, the saved session
    /// is left alone so quitting at the start screen does not lose it.
    touched: bool,
}

/// "Copy to another server": what, from where, and to where.
struct CrossCopy {
    from: SessionId,
    paths: Vec<String>,
    to: Option<SessionId>,
    dest: String,
    /// Overwrite items that already exist at the destination.
    replace: bool,
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
        RichText::new(
            "Built-in checks: builtin:json, builtin:toml, builtin:yaml (no server tools needed); \
             builtin:postgresql, and builtin:postgresql-reload as a reload.",
        )
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
    forget_sudo: bool,
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
        let font_error = apply_fonts(&cc.egui_ctx, &settings.mono_font_file).err();
        let mut toast = font_error.map(|e| (format!("Could not use the terminal font: {e}"), true, Instant::now()));
        let profiles = ProfileStore::load(&paths.profiles_file()).unwrap_or_else(|e| {
            toast = Some((format!("Could not read saved connections: {e:#}"), true, Instant::now()));
            ProfileStore::default()
        });
        let snippets_path = paths.config_dir.join("snippets.toml");
        let paths_journal = paths.journal_file();
        let paths_teleport = paths.teleport_file();
        // Copies made for external editors are kept a week, then tidied away.
        let cache = paths.edit_cache_dir();
        rt.spawn_blocking(move || {
            malasdoh_core::paths::remove_older_than(&cache, std::time::Duration::from_secs(7 * 86400))
        });
        let shared = Shared {
            drafts: malasdoh_core::drafts::Drafts::new(paths.drafts_dir()),
            rt: rt.handle().clone(),
            events,
            secrets: Arc::new(OsKeychain) as Arc<dyn SecretStore>,
            known_hosts: KnownHosts::standard(paths.known_hosts_file(), vec![paths.teleport_dir().join("known_hosts")]),
            paths,
        };
        let mut app = Self {
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
            multi_edit: MultiEdit::default(),
            show_sidebar: true,
            last_local_dir: None,
            cross: None,
            compares: Compares::default(),
            journal: Journal::new(paths_journal),
            journal_win: JournalWindow::default(),
            keys_win: KeysWindow::default(),
            teleport: malasdoh_core::teleport::TeleportStore::load(paths_teleport),
            teleport_win: Default::default(),
            broadcast: None,
            export_dlg: None,
            import_dlg: None,
            last: LastSession::default(),
            last_written: String::new(),
            last_persist: Instant::now(),
            last_draft_save: Instant::now(),
            touched: false,
        };
        let mut last = LastSession::load(&app.shared.paths.config_dir.join("session.toml"));
        last.retain_profiles(|id| app.profiles.get(id).is_some());
        app.last_written = last.to_toml();
        app.last = last;
        if app.settings.reopen_on_start {
            app.reopen_last();
        }
        app
    }

    fn reopen_last(&mut self) {
        let last = std::mem::take(&mut self.last);
        let mut active = None;
        for (i, tab) in last.tabs.into_iter().enumerate() {
            self.connect(tab.profile);
            if let Some(s) = self.sessions.last_mut()
                && s.profile.id == tab.profile
            {
                if last.active == Some(i) {
                    active = Some(s.id);
                }
                s.restore = Some(tab);
            }
        }
        if active.is_some() {
            self.active = active;
        }
    }

    fn snapshot(&self) -> LastSession {
        LastSession {
            tabs: self.sessions.iter().map(|s| s.snapshot()).collect(),
            active: self.active.and_then(|id| self.sessions.iter().position(|s| s.id == id)),
        }
    }

    /// Write what is open, if it changed. Cheap enough to call every few
    /// seconds; also called on exit.
    fn persist_session(&mut self) {
        self.last_persist = Instant::now();
        if !self.touched {
            return;
        }
        let text = self.snapshot().to_toml();
        if text != self.last_written {
            let path = self.shared.paths.config_dir.join("session.toml");
            match malasdoh_core::paths::write_atomic(&path, text.as_bytes()) {
                Ok(()) => self.last_written = text,
                Err(e) => tracing::warn!("could not save the open session: {e:#}"),
            }
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
        self.touched = true;
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            let ev = match ev {
                AppEvent { kind: EventKind::Compare(m), .. } => {
                    self.compares.handle(m);
                    continue;
                }
                AppEvent { kind: EventKind::Keys(m), .. } => {
                    self.keys_win.handle(m, &self.sessions);
                    continue;
                }
                AppEvent { kind: EventKind::Teleport(m), .. } => {
                    let actions = self.teleport_win.handle(m, &self.teleport);
                    self.teleport_actions(actions);
                    continue;
                }
                AppEvent { kind: EventKind::Session(crate::session::SessionMsg::TeleportLogin(c)), session } => {
                    self.teleport_win.ask_login(c, session);
                    continue;
                }
                ev => ev,
            };
            if ev.session == MULTI_RUN {
                match ev.kind {
                    EventKind::Question(q) => self.multi.questions.push_back(q),
                    EventKind::Multi(m) => self.multi.handle(m),
                    EventKind::MultiEdit(m) => self.multi_edit.handle(m),
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
                EventKind::Multi(_)
                | EventKind::MultiEdit(_)
                | EventKind::Compare(_)
                | EventKind::Keys(_)
                | EventKind::Teleport(_) => {}
            }
        }
    }

    /// Add imported connections, skipping ones already here (same user,
    /// host and port), so importing twice is harmless.
    fn add_imported(&mut self, found: anyhow::Result<Vec<Profile>>, from: &str) {
        let found = match found {
            Ok(f) => f,
            Err(e) => return self.notify(format!("Could not import from {from}: {e:#}"), true),
        };
        if found.is_empty() {
            return self.notify(format!("No saved SSH sessions found in {from}."), false);
        }
        let (mut added, mut skipped) = (0, 0);
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        for mut p in found {
            let exists = self
                .profiles
                .profiles
                .iter()
                .any(|e| e.host == p.host && e.port == p.port && (e.user == p.user || p.user.is_empty()));
            if exists {
                skipped += 1;
                continue;
            }
            if p.user.is_empty() {
                p.user = user.clone();
            }
            if self.profiles.upsert(p).is_ok() {
                added += 1;
            }
        }
        self.save_profiles();
        let mut msg = format!("Imported {added} connection(s) from {from}.");
        if skipped > 0 {
            msg.push_str(&format!(" {skipped} were already here."));
        }
        msg.push_str(" Saved passwords are not imported.");
        self.notify(msg, false);
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
                if ui.button("Import from PuTTY").clicked() {
                    let found = malasdoh_core::import::putty();
                    self.add_imported(found, "PuTTY");
                    ui.close();
                }
                if ui.button("Import from WinSCP").on_hover_text("Its registry settings or WinSCP.ini").clicked() {
                    let found = malasdoh_core::import::winscp();
                    self.add_imported(found, "WinSCP");
                    ui.close();
                }
                if ui.button("Import a WinSCP.ini file…").clicked() {
                    if let Some(f) = rfd::FileDialog::new().add_filter("WinSCP.ini", &["ini"]).pick_file() {
                        let found = std::fs::read_to_string(&f)
                            .map(|t| malasdoh_core::import::parse_winscp_ini(&t))
                            .map_err(|e| anyhow::anyhow!("reading {}: {e}", f.display()));
                        self.add_imported(found, "WinSCP.ini");
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button("Export connections…").on_hover_text("To one passphrase-encrypted file").clicked() {
                    self.start_export();
                    ui.close();
                }
                if ui.button("Import connections…").on_hover_text("From a MalasDoh export").clicked() {
                    self.start_import();
                    ui.close();
                }
                ui.separator();
                if ui.button("Teleport…").on_hover_text("Log in to a Teleport cluster and use its servers").clicked()
                {
                    self.teleport_win.open = true;
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
            if ui.selectable_label(self.show_sidebar, "☰").on_hover_text("Show or hide the connection list").clicked()
            {
                self.show_sidebar = !self.show_sidebar;
            }
            ui.menu_button("Tools", |ui| {
                if ui.button("Run on many hosts…").clicked() {
                    self.multi.open = true;
                    ui.close();
                }
                if ui
                    .button("Edit a file on many servers…")
                    .on_hover_text("Change it once, check what it does on each server, then save to all")
                    .clicked()
                {
                    self.multi_edit.open = true;
                    ui.close();
                }
                if ui.button("Snippets…").clicked() {
                    self.show_snippets = true;
                    ui.close();
                }
                if ui
                    .button("Type into several terminals…")
                    .on_hover_text("What you type in one terminal also goes to the other servers you pick")
                    .clicked()
                {
                    self.broadcast =
                        Some(self.sessions.iter().filter(|s| s.state == ConnState::Connected).map(|s| s.id).collect());
                    ui.close();
                }
                if ui
                    .button("SSH keys…")
                    .on_hover_text("Make a key, and put it on a server (like ssh-copy-id)")
                    .clicked()
                {
                    self.keys_win.show(self.active);
                    ui.close();
                }
                if ui
                    .button("Change journal…")
                    .on_hover_text("Every file saved from MalasDoh, with what changed")
                    .clicked()
                {
                    self.journal_win.show(&self.journal, None);
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

        if s.local.is_some() {
            egui::Panel::left(egui::Id::new(("local", s.id))).resizable(true).default_size(300.0).show(ui, |ui| {
                s.local_ui(ui, &shared);
            });
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
            let mut activate = None;
            for (n, t) in s.terms.iter().enumerate() {
                let label = if t.lasting.is_some() {
                    format!("⟳ Terminal {}", n + 1)
                } else {
                    format!("⌨ Terminal {}", n + 1)
                };
                let r = ui.selectable_label(s.active == Some(Pane::Term(t.id)), label);
                let r = match &t.lasting {
                    Some(name) => r.on_hover_text(format!(
                        "Lasting terminal ({name}): it keeps running on the server if the connection drops, \
                         and comes back on reconnect. In tmux, scroll back with Ctrl+B then [ (q to leave)."
                    )),
                    None => r,
                };
                if r.clicked() {
                    activate = Some(Pane::Term(t.id));
                }
                if s.active == Some(Pane::Term(t.id)) && ui.small_button("✖").clicked() {
                    close_term = Some(t.id);
                }
            }
            let mut close_log = None;
            for l in &s.logs {
                if ui.selectable_label(s.active == Some(Pane::Log(l.id)), l.title()).on_hover_text(&l.path).clicked() {
                    activate = Some(Pane::Log(l.id));
                }
                if s.active == Some(Pane::Log(l.id)) && ui.small_button("✖").on_hover_text("Stop following").clicked()
                {
                    close_log = Some(l.id);
                }
            }
            if let Some(id) = close_log {
                s.close_log(id);
            }
            for e in &s.editors {
                if ui
                    .selectable_label(s.active == Some(Pane::Editor(e.id)), format!("📝 {}", e.title()))
                    .on_hover_text(&e.session.path)
                    .clicked()
                {
                    activate = Some(Pane::Editor(e.id));
                }
                if s.active == Some(Pane::Editor(e.id)) && ui.small_button("✖").clicked() {
                    close_editor = Some(e.id);
                }
            }
            // Clicking the pane shown on the split side swaps the two.
            if let Some(p) = activate {
                if s.split == Some(p) {
                    s.split = s.active;
                }
                s.active = Some(p);
            }
            if ui
                .selectable_label(s.split.is_some(), "Split")
                .on_hover_text("Two panes side by side: terminals, files or logs")
                .clicked()
            {
                s.toggle_split(&shared, &settings);
            }
            if ui
                .selectable_label(s.local.is_some(), "🖥 Local files")
                .on_hover_text("Show this computer's files beside the server's")
                .clicked()
            {
                match s.local.take() {
                    Some(l) => self.last_local_dir = Some(l.path),
                    None => s.local = Some(crate::local::LocalBrowser::new(self.last_local_dir.clone())),
                }
            }
            if s.state == ConnState::Connected {
                if ui.button("+ Terminal").clicked() {
                    s.open_terminal(&shared, &settings);
                }
                let menu = ui.menu_button("⏷", |ui| {
                    if ui
                        .button("⟳ Lasting terminal")
                        .on_hover_text(
                            "Runs in tmux (or screen) on the server: long jobs like pg_upgrade, a dump or a \
                             release upgrade keep going if the connection drops, and the tab goes back to them \
                             when you reconnect, even after restarting MalasDoh.",
                        )
                        .clicked()
                    {
                        s.open_lasting_terminal(None, &shared, &settings);
                        ui.close();
                    }
                    ui.separator();
                    match &s.lasting_list {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Looking for lasting terminals…");
                            });
                        }
                        Some(list) => {
                            let open: Vec<&str> = s.terms.iter().filter_map(|t| t.lasting.as_deref()).collect();
                            let others: Vec<&(String, bool)> =
                                list.iter().filter(|(n, _)| !open.contains(&n.as_str())).collect();
                            if others.is_empty() {
                                ui.label(RichText::new("No other lasting terminals on this server").weak());
                            }
                            let mut pick = None;
                            for (name, attached) in others {
                                let label = if *attached {
                                    format!("Reattach {name} (open elsewhere)")
                                } else {
                                    format!("Reattach {name}")
                                };
                                if ui.button(label).clicked() {
                                    pick = Some(name.clone());
                                    ui.close();
                                }
                            }
                            if let Some(name) = pick {
                                s.open_lasting_terminal(Some(name), &shared, &settings);
                            }
                        }
                    }
                });
                if menu.response.on_hover_text("More terminals: lasting, or reattach one").clicked() {
                    s.list_lasting(&shared);
                }
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
        if let Some(paths) = s.cross_copy.take() {
            self.cross = Some(CrossCopy { from: sid, paths, to: None, dest: String::new(), replace: false });
        }
        let compare = s.compare.take();
        let bookmarks = std::mem::take(&mut s.bookmarks_changed).then(|| s.profile.bookmarks.clone());
        if let Some((path, sudo)) = s.edit_many.take() {
            self.multi_edit.show_with(path, pid, sudo);
        }
        if let Some(path) = s.history.take() {
            self.journal_win.show(&self.journal, Some(path));
        }
        // Bookmarks belong to the saved connection.
        if let Some(b) = bookmarks
            && let Some(mut p) = self.profiles.get(pid).cloned()
        {
            p.bookmarks = b;
            if self.profiles.upsert(p).is_ok() {
                self.save_profiles();
            }
        }
        if let Some(req) = compare {
            self.compares.request(req, sid, &self.sessions, &self.shared);
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
            if !self.last.tabs.is_empty() {
                let names: Vec<String> = self
                    .last
                    .tabs
                    .iter()
                    .filter_map(|t| self.profiles.get(t.profile).map(|p| p.display_name()))
                    .collect();
                if ui
                    .button(RichText::new("Reopen last session").strong())
                    .on_hover_text("Reconnect and reopen the folders, files and terminals you had open")
                    .clicked()
                {
                    self.reopen_last();
                }
                ui.label(RichText::new(names.join(", ")).weak());
                ui.add_space(10.0);
            }
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
            None => "many servers".to_string(),
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
                    if let Some(w) = &p.warning {
                        ui.add_space(4.0);
                        ui.colored_label(ui.visuals().warn_fg_color, w);
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
                        SecretPrompt::ProxyPassword { user, proxy, retry } => ("Proxy password", format!("Password for {user} at the {proxy}"), *retry),
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
                Question::Sudo { host, retry, note, .. } => {
                    ui.heading("sudo password");
                    if *retry {
                        ui.colored_label(ui.visuals().error_fg_color, "sudo did not accept that password.");
                    } else if let Some(n) = note {
                        ui.colored_label(ui.visuals().warn_fg_color, n);
                    }
                    ui.label(format!("Your password on {host}, for sudo."));
                    let r = ui.add(egui::TextEdit::singleline(&mut a.secret).password(true));
                    r.request_focus();
                    ui.checkbox(&mut a.remember, "Remember in the system keychain").on_hover_text(
                        "Saved for this connection once sudo accepts it, so editing as root, root browsing and \
                         editing on many servers do not ask again. Otherwise it is kept in memory until you disconnect.",
                    );
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
                let _ = reply.send(ok.then_some(SudoAnswer { password: a.secret, remember: a.remember }));
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
                    if matches!(p.auth, AuthMethod::Password | AuthMethod::Auto) {
                        ui.checkbox(&mut p.sudo_uses_login_password, "Use the saved password for sudo too")
                            .on_hover_text(
                                "Tried before asking for the sudo password. Leave off if they differ: \
                                 a wrong sudo password is logged on the server.",
                            );
                    }
                    if !form.is_new {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut form.forget_password, "Forget saved password");
                            ui.checkbox(&mut form.forget_sudo, "Forget saved sudo password");
                        });
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
                ui.label("Startup command");
                ui.add(egui::TextEdit::singleline(&mut p.startup_command).hint_text("e.g. sudo -i, or cd /var/www"))
                    .on_hover_text("Typed into each new terminal after login");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut p.follow_terminal, "File browser follows the terminal's folder").on_hover_text(
                    "After each command the browser moves to the terminal's current folder (bash and zsh, or shells that report it themselves)",
                );
                ui.end_row();
                ui.label("Backups");
                ui.checkbox(&mut p.remote_backup, "Also keep a .bak copy next to edited files on the server");
                ui.end_row();
                ui.label("Proxy");
                proxy_editor(ui, &mut p.proxy);
                ui.end_row();
                ui.label("Keepalive");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.keepalive_secs).range(0..=3600).suffix(" s"));
                    ui.label(RichText::new("0 = off. Keeps idle sessions open through firewalls.").weak());
                });
                ui.end_row();
                ui.label("Agent");
                ui.checkbox(&mut p.forward_agent, "Forward my SSH agent (ssh -A)").on_hover_text(
                    "Lets programs on this server use your keys, e.g. git pull. Only for servers you trust: \
                     while you are connected, anyone with root there can use your keys too.",
                );
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
                let (forget, forget_sudo) = (form.forget_password, form.forget_sudo);
                match self.profiles.upsert(profile.clone()) {
                    Ok(()) => {
                        if forget {
                            let secrets = self.shared.secrets.clone();
                            self.shared.rt.spawn_blocking(move || secrets.delete(&SecretKey::Password(profile.id)));
                        }
                        if forget_sudo {
                            let secrets = self.shared.secrets.clone();
                            self.shared.rt.spawn_blocking(move || secrets.delete(&SecretKey::SudoPassword(profile.id)));
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

    fn cross_copy_ui(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.cross else { return };
        let targets: Vec<(SessionId, String, String)> = self
            .sessions
            .iter()
            .filter(|s| s.id != c.from && s.state == ConnState::Connected && s.fs().is_some())
            .map(|s| (s.id, s.profile.display_name(), s.browser.path.clone()))
            .collect();
        let mut result = None;
        egui::Modal::new("cross-copy".into()).show(ctx, |ui| {
            ui.set_min_width(460.0);
            ui.heading("Copy to another server");
            for p in c.paths.iter().take(8) {
                ui.monospace(p);
            }
            if c.paths.len() > 8 {
                ui.label(format!("…and {} more", c.paths.len() - 8));
            }
            if targets.is_empty() {
                ui.colored_label(ui.visuals().warn_fg_color, "Connect to the destination server in another tab first.");
            }
            for (id, name, dir) in &targets {
                if ui.radio(c.to == Some(*id), name).clicked() {
                    c.to = Some(*id);
                    c.dest = dir.clone();
                }
            }
            ui.horizontal(|ui| {
                ui.label("Into folder");
                ui.add(egui::TextEdit::singleline(&mut c.dest).desired_width(320.0));
            });
            ui.checkbox(&mut c.replace, "Replace items that already exist there");
            ui.label(RichText::new("Files pass through this computer (a private temporary folder), so both servers only need to reach you.").weak().small());
            ui.horizontal(|ui| {
                let ok = c.to.is_some() && !c.dest.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Copy")).clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    result = Some(false);
                }
            });
        });
        match result {
            Some(true) => {
                let c = self.cross.take().unwrap();
                self.start_cross_copy(c);
            }
            Some(false) => self.cross = None,
            None => {}
        }
    }

    fn start_cross_copy(&mut self, c: CrossCopy) {
        let Some(to) = c.to else { return };
        let src =
            self.sessions.iter().find(|s| s.id == c.from).and_then(|s| s.fs().map(|fs| (fs, s.profile.display_name())));
        let Some((src_fs, src_name)) = src else { return self.notify("The source connection is gone", true) };
        let Some(dst) = self.sessions.iter_mut().find(|s| s.id == to) else { return };
        let Some(dst_fs) = dst.fs() else { return };
        let dest = c.dest.trim().to_string();
        let replace = c.replace;
        let tmp_root = self.shared.paths.edit_cache_dir();
        let events = self.shared.events.clone();
        for p in c.paths {
            let (index, progress) =
                dst.add_transfer(format!("⬇⬆ {} from {src_name}", malasdoh_core::sftp::file_name(&p)));
            let (src_fs, dst_fs, dest, tmp) = (
                src_fs.clone(),
                dst_fs.clone(),
                dest.clone(),
                tmp_root.join(format!("copy-{}", uuid::Uuid::new_v4().simple())),
            );
            let events = events.clone();
            self.shared.rt.spawn(async move {
                let result = async {
                    let target = malasdoh_core::sftp::join(&dest, malasdoh_core::sftp::file_name(&p));
                    if !replace && dst_fs.exists(&target).await? {
                        anyhow::bail!("{target} already exists (tick \"Replace\" to overwrite it)");
                    }
                    malasdoh_core::paths::create_private_dir(&tmp)?;
                    let local = src_fs.download(&p, &tmp, &progress).await?;
                    let done = dst_fs.upload(&local, &dest, &progress).await?;
                    anyhow::Ok(format!("Copied to {done}"))
                }
                .await;
                let _ = std::fs::remove_dir_all(&tmp);
                events.session(
                    to,
                    crate::session::SessionMsg::TransferDone { index, result: result.map_err(|e| format!("{e:#}")) },
                );
            });
        }
        self.active = Some(to);
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
                        self.shared.rt.spawn_blocking(move || {
                            secrets.delete(&SecretKey::Password(id));
                            secrets.delete(&SecretKey::SudoPassword(id));
                        });
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
                ui.label("Some open files have changes that are not saved to the server.");
                ui.horizontal(|ui| {
                    if ui
                        .button("Close, keep the edits")
                        .on_hover_text("They are offered again the next time you connect")
                        .clicked()
                    {
                        result = Some(false);
                    }
                    if ui.button("Close and discard").clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        result = None;
                        self.confirm_close = None;
                    }
                });
            });
            if let Some(discard) = result {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                    if discard {
                        s.discard_drafts(&self.shared);
                    } else {
                        s.autosave_drafts(&self.shared);
                    }
                }
                self.close_session(id);
                self.confirm_close = None;
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
                        ui.checkbox(&mut s.reopen_on_start, "Reopen last session's connections and files on start");
                        ui.checkbox(&mut s.dark_mode, "Dark mode");
                        ui.separator();
                        ui.label(RichText::new("Terminal").strong());
                        ui.horizontal(|ui| {
                            ui.label("Colours");
                            egui::ComboBox::from_id_salt("term-theme").selected_text(&s.terminal_theme).show_ui(
                                ui,
                                |ui| {
                                    for t in crate::themes::THEMES {
                                        ui.selectable_value(&mut s.terminal_theme, t.name.to_string(), t.name);
                                    }
                                },
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Font file");
                            ui.add(
                                egui::TextEdit::singleline(&mut s.mono_font_file)
                                    .hint_text("built-in (Hack)")
                                    .desired_width(260.0),
                            )
                            .on_hover_text("A monospace .ttf or .otf, used by terminals and the editor");
                            if ui.button("Browse…").clicked()
                                && let Some(f) =
                                    rfd::FileDialog::new().add_filter("Fonts", &["ttf", "otf", "ttc"]).pick_file()
                            {
                                s.mono_font_file = f.display().to_string();
                            }
                            if !s.mono_font_file.is_empty() && ui.small_button("Reset").clicked() {
                                s.mono_font_file.clear();
                            }
                        });
                        ui.checkbox(&mut s.copy_on_select, "Copy text as soon as it is selected");
                        ui.checkbox(&mut s.confirm_paste, "Ask before pasting several lines into a shell");
                        ui.label(
                            RichText::new(
                                "Right click copies the selection or pastes; middle click pastes; Ctrl+click opens a \
                                 link; Shift+drag selects in programs that use the mouse.",
                            )
                            .weak()
                            .small(),
                        );
                        ui.separator();
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
            if self.settings.mono_font_file != before.mono_font_file
                && let Err(e) = apply_fonts(ctx, &self.settings.mono_font_file)
            {
                self.notify(format!("Could not use that font: {e}"), true);
            }
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
        Self {
            port: profile.port.to_string(),
            profile,
            is_new,
            error: None,
            forget_password: false,
            forget_sudo: false,
        }
    }
}

impl App {
    fn broadcast_ui(&mut self, ui: &mut egui::Ui) {
        let Some(set) = &mut self.broadcast else { return };
        let mut stop = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("⌨ Typing goes to:").strong().color(egui::Color32::WHITE));
            for s in self.sessions.iter().filter(|s| s.state == ConnState::Connected) {
                let mut on = set.contains(&s.id);
                let name = egui::RichText::new(s.profile.display_name()).color(egui::Color32::WHITE);
                if ui.checkbox(&mut on, name).changed() {
                    if on {
                        set.push(s.id);
                    } else {
                        set.retain(|x| *x != s.id);
                    }
                }
            }
            if ui.button("Stop").clicked() {
                stop = true;
            }
        });
        if stop {
            self.broadcast = None;
        }
    }

    /// Send what was typed in a chosen session's terminal to the current
    /// terminal of every other chosen session.
    fn send_broadcast(&mut self) {
        let typed: Vec<(SessionId, Vec<u8>)> = self
            .sessions
            .iter_mut()
            .flat_map(|s| {
                let id = s.id;
                std::mem::take(&mut s.typed).into_iter().map(move |(_, b)| (id, b))
            })
            .collect();
        let Some(set) = &self.broadcast else { return };
        for (from, bytes) in typed {
            if !set.contains(&from) {
                continue;
            }
            for s in self.sessions.iter().filter(|s| s.id != from && set.contains(&s.id)) {
                s.echo_input(&bytes);
            }
        }
    }
}

/// Direct, SOCKS5, HTTP CONNECT, or a command (OpenSSH's ProxyCommand).
fn proxy_editor(ui: &mut egui::Ui, proxy: &mut malasdoh_core::proxy::Proxy) {
    use malasdoh_core::proxy::Proxy;
    ui.vertical(|ui| {
        let kind = match proxy {
            Proxy::None => "Direct",
            Proxy::Socks5 { .. } => "SOCKS5",
            Proxy::Http { .. } => "HTTP CONNECT",
            Proxy::Command { .. } => "Command",
            Proxy::TeleportTls { .. } => "Teleport",
        };
        egui::ComboBox::from_id_salt("proxy-kind").selected_text(kind).show_ui(ui, |ui| {
            if ui.selectable_label(kind == "Direct", "Direct").clicked() {
                *proxy = Proxy::None;
            }
            if ui.selectable_label(kind == "SOCKS5", "SOCKS5").clicked() && kind != "SOCKS5" {
                *proxy = Proxy::Socks5 { host: String::new(), port: 1080, user: String::new() };
            }
            if ui.selectable_label(kind == "HTTP CONNECT", "HTTP CONNECT").clicked() && kind != "HTTP CONNECT" {
                *proxy = Proxy::Http { host: String::new(), port: 3128, user: String::new() };
            }
            if ui.selectable_label(kind == "Command", "Command (ProxyCommand)").clicked() && kind != "Command" {
                *proxy = Proxy::Command { command: "nc -X connect -x proxy:3128 %h %p".into() };
            }
        });
        match proxy {
            Proxy::None => {}
            Proxy::Socks5 { host, port, user } | Proxy::Http { host, port, user } => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(host).hint_text("proxy host").desired_width(160.0));
                    ui.add(egui::DragValue::new(port).range(1..=65535));
                    ui.add(egui::TextEdit::singleline(user).hint_text("user (optional)").desired_width(110.0))
                        .on_hover_text("The password is asked for when connecting, and kept in the OS keychain if you tick remember");
                });
            }
            Proxy::Command { command } => {
                ui.add(egui::TextEdit::singleline(command).desired_width(320.0)).on_hover_text(
                    "Run on this computer; its input and output carry the SSH connection. %h, %p, %r: host, port, user",
                );
            }
            Proxy::TeleportTls { host, port, .. } => {
                ui.label(format!("Teleport proxy at {host}:{port} (set up from the Connection menu, Teleport…)"));
            }
        }
        if !proxy.is_none() {
            ui.label(RichText::new("Used when this is the first hop (a jump host's own proxy is used otherwise).").weak().small());
        }
    });
}

/// Use `file` as the monospace font (terminal, editor), or the built-in one.
fn apply_fonts(ctx: &egui::Context, file: &str) -> Result<(), String> {
    let mut fonts = egui::FontDefinitions::default();
    if !file.trim().is_empty() {
        let path = malasdoh_core::paths::expand_tilde(file.trim());
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // egui would only fail later, on first use; check it parses now.
        if bytes.len() < 12 || !matches!(&bytes[..4], [0, 1, 0, 0] | b"OTTO" | b"true" | b"ttcf") {
            return Err(format!("{} is not a TrueType or OpenType font", path.display()));
        }
        fonts.font_data.insert("user-mono".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        // First choice; the built-in fonts stay as fallbacks for missing glyphs.
        fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "user-mono".into());
    }
    ctx.set_fonts(fonts);
    Ok(())
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_session();
        for s in &mut self.sessions {
            s.autosave_drafts(&self.shared);
        }
    }

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
            self.notify(
                "Some files have unsaved changes. Close the window again to quit; they are kept as drafts and offered \
                 next time you connect.",
                true,
            );
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_ui(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        if self.broadcast.is_some() {
            egui::Panel::top("broadcast")
                .frame(egui::Frame::side_top_panel(ui.style()).fill(egui::Color32::from_rgb(0x6e, 0x1f, 0x1f)))
                .show(ui, |ui| self.broadcast_ui(ui));
        }
        if self.show_sidebar {
            egui::Panel::left("connections").resizable(true).default_size(230.0).show(ui, |ui| self.sidebar_ui(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.session_ui(ui));
        self.send_broadcast();

        self.form_ui(&ctx);
        self.confirm_ui(&ctx);
        self.cross_copy_ui(&ctx);
        self.backup_ui(&ctx);
        self.settings_ui(&ctx);
        self.snippet_windows_ui(&ctx);
        self.multi.ui(&ctx, &self.profiles, &self.snippets, &self.shared);
        self.multi_edit.ui(&ctx, &self.profiles, &self.shared, &self.settings.validation_rules);
        if let Some((a, b)) = self.journal_win.ui(&ctx, &self.journal, &self.sessions) {
            self.compares.open(a, b, &self.sessions, &self.shared);
        }
        self.compares.ui(&ctx, &self.sessions, &self.shared);
        let actions = self.teleport_win.ui(&ctx, &self.teleport, &self.shared);
        self.teleport_actions(actions);
        if let Some((id, key)) = self.keys_win.ui(&ctx, &self.sessions, &self.shared)
            && let Some(mut p) = self.profiles.get(id).cloned()
        {
            p.auth = AuthMethod::Key { path: key.display().to_string() };
            let name = p.display_name();
            if self.profiles.upsert(p).is_ok() {
                self.save_profiles();
                self.notify(format!("{name} now logs in with {}", key.display()), false);
            }
        }
        self.question_ui(&ctx);

        if self.last_persist.elapsed() > Duration::from_secs(5) {
            self.persist_session();
        }
        if self.last_draft_save.elapsed() > Duration::from_secs(3) {
            self.last_draft_save = Instant::now();
            for s in &mut self.sessions {
                s.autosave_drafts(&self.shared);
            }
        }

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
