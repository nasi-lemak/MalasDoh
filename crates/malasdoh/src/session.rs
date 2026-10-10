//! One connection tab: its SSH connection, terminals, file browser and open
//! files. UI methods never block; every network operation runs as a task and
//! reports back as a [`SessionMsg`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use egui::RichText;
use malasdoh_core::edit::{self, Access, EditSession, RemoteIo, SaveError, SaveOptions, SaveReport, SudoError};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::profile::Profile;
use malasdoh_core::secrets::SecretStore;
use malasdoh_core::sftp::{self, Progress, RemoteEntry, RemoteFs};
use malasdoh_core::ssh::{self, ConnectContext, Connection, HostKeyError, ShellEvent};
use malasdoh_core::validate::{CheckOutcome, find_rule};
use malasdoh_core::{AppPaths, text};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{Events, Question, SessionId, UiPrompter};
use crate::browser::{self, Browser, Transfer};
use crate::editor::{Editor, EditorAction, State as EditorState};
use crate::forwards::{ForwardMsg, Forwards};
use crate::settings::Settings;
use crate::sync_ui::{SyncMsg, SyncWindow};
use crate::terminal::Terminal;

/// What every session needs from the app.
#[derive(Clone)]
pub struct Shared {
    pub rt: tokio::runtime::Handle,
    pub events: Events,
    pub secrets: Arc<dyn SecretStore>,
    pub known_hosts: KnownHosts,
    pub paths: AppPaths,
}

pub enum SessionMsg {
    Forward(ForwardMsg),
    Sync(SyncMsg),
    Connected {
        conn: Arc<Connection>,
        fs: Result<(RemoteFs, String), String>,
    },
    Failed {
        error: String,
        forgettable_key: bool,
    },
    ShellOutput {
        term: u64,
        data: Vec<u8>,
    },
    ShellExited {
        term: u64,
        status: Option<u32>,
    },
    ShellFailed {
        term: u64,
        error: String,
    },
    Listing {
        path: String,
        result: Result<Vec<RemoteEntry>, String>,
    },
    OpDone {
        result: Result<String, String>,
    },
    /// `restore`: replace the editor text with this (a backup) and review the save.
    Opened {
        path: String,
        result: Result<(EditSession, Access), String>,
        reload: Option<u64>,
        restore: Option<String>,
    },
    Saved {
        editor: u64,
        result: Result<(EditSession, SaveReport), SaveFailure>,
    },
    TransferDone {
        index: usize,
        result: Result<String, String>,
    },
    ExternalReady {
        session: EditSession,
        local: PathBuf,
        access: Access,
    },
    SudoReady {
        password: Result<Option<String>, String>,
        then: AfterSudo,
    },
}

pub enum SaveFailure {
    Conflict(Vec<u8>),
    /// `session` is the editor's base afterwards (after a rollback, the
    /// server holds the original again).
    Check {
        rule: String,
        output: String,
        rolled_back: bool,
        session: Box<EditSession>,
    },
    PermissionDenied(String),
    Other(String),
}

#[derive(Clone)]
pub enum AfterSudo {
    Open(String),
    Save(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting(String),
    Connected,
    Failed { error: String, forgettable_key: bool },
    Disconnected,
}

enum ShellCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
}

pub struct TermPane {
    pub id: u64,
    pub term: Terminal,
    tx: Option<mpsc::UnboundedSender<ShellCmd>>,
}

impl TermPane {
    /// Connected to a running shell.
    pub fn is_live(&self) -> bool {
        self.tx.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Term(u64),
    Editor(u64),
}

/// A file opened in the user's own editor, watched for saves.
struct ExternalEdit {
    local: PathBuf,
    session: EditSession,
    access: Access,
    mtime: Option<SystemTime>,
}

pub struct Session {
    pub id: SessionId,
    pub profile: Profile,
    chain: Vec<Profile>,
    pub state: ConnState,
    conn: Option<Arc<Connection>>,
    fs: Option<RemoteFs>,
    pub terms: Vec<TermPane>,
    pub editors: Vec<Editor>,
    pub active: Option<Pane>,
    pub browser: Browser,
    pub questions: VecDeque<Question>,
    /// sudo access once it has been checked; kept in memory only.
    sudo: Option<Access>,
    externals: Vec<ExternalEdit>,
    last_external_check: Instant,
    next_id: u64,
    saves_done: u64,
    pub toast: Option<(String, bool, Instant)>,
    backups_view: Option<(String, Vec<PathBuf>)>,
    pub forwards: Forwards,
    pub sync: SyncWindow,
}

impl Session {
    pub fn new(id: SessionId, chain: Vec<Profile>) -> Self {
        let profile = chain.last().cloned().expect("chain has a target");
        let forwards = Forwards::new(&profile.forwards);
        Self {
            id,
            profile,
            chain,
            state: ConnState::Connecting("Starting…".into()),
            conn: None,
            fs: None,
            terms: Vec::new(),
            editors: Vec::new(),
            active: None,
            browser: Browser::default(),
            questions: VecDeque::new(),
            sudo: None,
            externals: Vec::new(),
            last_external_check: Instant::now(),
            next_id: 1,
            saves_done: 0,
            toast: None,
            backups_view: None,
            forwards,
            sync: SyncWindow::default(),
        }
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn status_color(&self) -> egui::Color32 {
        match self.state {
            ConnState::Connected => egui::Color32::from_rgb(0x5c, 0xb8, 0x5c),
            ConnState::Connecting(_) => egui::Color32::from_rgb(0xe0, 0xb0, 0x40),
            _ => egui::Color32::from_rgb(0xd0, 0x50, 0x50),
        }
    }

    pub fn has_unsaved(&self) -> bool {
        self.editors.iter().any(|e| e.is_modified())
    }

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), error, Instant::now()));
    }

    pub fn connect(&mut self, shared: &Shared) {
        self.state = ConnState::Connecting("Connecting…".into());
        let chain = self.chain.clone();
        let shared2 = shared.clone();
        let id = self.id;
        let start_dir = self.profile.start_dir.clone();
        let current_dir = self.browser.path.clone();
        shared.rt.spawn(async move {
            let prompter = Arc::new(UiPrompter { events: shared2.events.clone(), session: id });
            let ctx = ConnectContext::new(shared2.known_hosts.clone(), shared2.secrets.clone(), prompter);
            let msg = match ssh::connect(chain, &ctx).await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    let fs = async {
                        let fs = conn.open_sftp().await?;
                        // After a reconnect, stay where the user was.
                        let dir = if !current_dir.is_empty() {
                            current_dir
                        } else if start_dir.trim().is_empty() {
                            fs.home().await?
                        } else {
                            fs.canonicalize(start_dir.trim()).await?
                        };
                        anyhow::Ok((fs, dir))
                    }
                    .await
                    .map_err(|e| format!("{e:#}"));
                    SessionMsg::Connected { conn, fs }
                }
                Err(e) => {
                    let forgettable_key = matches!(
                        e.downcast_ref::<HostKeyError>(),
                        Some(HostKeyError::Changed { file, .. }) if file == shared2.known_hosts.app_file()
                    );
                    SessionMsg::Failed { error: format!("{e:#}"), forgettable_key }
                }
            };
            shared2.events.session(id, msg);
        });
    }

    pub fn forget_host_key(&mut self, shared: &Shared) {
        match shared.known_hosts.forget(&self.profile.host, self.profile.port) {
            Ok(n) => self.notify(format!("Removed {n} old key(s) for {}", self.profile.host), false),
            Err(e) => self.notify(format!("Could not update known_hosts: {e:#}"), true),
        }
    }

    pub fn disconnect(&mut self, shared: &Shared) {
        if let Some(conn) = self.conn.take() {
            shared.rt.spawn(async move { conn.disconnect().await });
        }
        self.fs = None;
        for t in &mut self.terms {
            t.tx = None;
        }
        self.forwards.on_disconnected();
        self.state = ConnState::Disconnected;
    }

    pub fn open_terminal(&mut self, shared: &Shared, settings: &Settings) {
        let Some(conn) = self.conn.clone() else { return };
        let id = self.next();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.terms.push(TermPane { id, term: Terminal::new(settings.terminal_font_size), tx: Some(tx) });
        self.active = Some(Pane::Term(id));
        let events = shared.events.clone();
        let session = self.id;
        shared.rt.spawn(async move {
            let repaint = events.clone();
            let mut shell = match conn.open_shell(80, 24, Arc::new(move || repaint.repaint())).await {
                Ok(s) => s,
                Err(e) => {
                    events.session(session, SessionMsg::ShellFailed { term: id, error: format!("{e:#}") });
                    return;
                }
            };
            loop {
                tokio::select! {
                    ev = shell.events.recv() => match ev {
                        Some(ShellEvent::Output(data)) => events.session(session, SessionMsg::ShellOutput { term: id, data }),
                        Some(ShellEvent::Exited(status)) => {
                            events.session(session, SessionMsg::ShellExited { term: id, status });
                            return;
                        }
                        None => {
                            events.session(session, SessionMsg::ShellExited { term: id, status: None });
                            return;
                        }
                    },
                    cmd = rx.recv() => match cmd {
                        Some(ShellCmd::Input(b)) => { let _ = shell.send(&b).await; }
                        Some(ShellCmd::Resize(c, r)) => { let _ = shell.resize(c as u32, r as u32).await; }
                        None => { let _ = shell.writer().close().await; return; }
                    },
                }
            }
        });
    }

    pub fn send_to_terminal(&mut self, bytes: Vec<u8>) {
        let target = match self.active {
            Some(Pane::Term(id)) => Some(id),
            _ => self.terms.first().map(|t| t.id),
        };
        if let Some(t) = self.terms.iter().find(|t| Some(t.id) == target)
            && let Some(tx) = &t.tx
        {
            let _ = tx.send(ShellCmd::Input(bytes));
            self.active = Some(Pane::Term(t.id));
        }
    }

    pub fn handle(&mut self, msg: SessionMsg, shared: &Shared, settings: &Settings) {
        match msg {
            SessionMsg::Forward(m) => self.forwards.handle(m),
            SessionMsg::Sync(m) => {
                self.sync.handle(m);
                self.refresh(shared);
            }
            SessionMsg::Connected { conn, fs } => {
                self.forwards.on_connected(&conn, shared, self.id);
                self.conn = Some(conn);
                self.state = ConnState::Connected;
                match fs {
                    Ok((fs, dir)) => {
                        self.fs = Some(fs);
                        self.navigate(dir, shared);
                    }
                    Err(e) => self.browser.error = Some(format!("SFTP is not available: {e}")),
                }
                // Reuse terminals from before a reconnect; open one if there are none.
                let old: Vec<u64> = self.terms.iter().filter(|t| t.tx.is_none()).map(|t| t.id).collect();
                self.terms.retain(|t| !old.contains(&t.id));
                if self.terms.is_empty() {
                    self.open_terminal(shared, settings);
                }
                if self.active.is_none() || matches!(self.active, Some(Pane::Term(id)) if old.contains(&id)) {
                    self.active = self.terms.first().map(|t| Pane::Term(t.id));
                }
            }
            SessionMsg::Failed { error, forgettable_key } => {
                self.state = ConnState::Failed { error, forgettable_key };
            }
            SessionMsg::ShellOutput { term, data } => {
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == term) {
                    t.term.feed(&data);
                }
            }
            SessionMsg::ShellExited { term, status } => {
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == term) {
                    t.tx = None;
                    t.term.exited = Some(match status {
                        Some(s) => format!("Shell exited with status {s}"),
                        None => "Shell closed".into(),
                    });
                }
                if self.conn.as_ref().is_some_and(|c| c.is_closed()) {
                    self.state = ConnState::Disconnected;
                }
            }
            SessionMsg::ShellFailed { term, error } => {
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == term) {
                    t.tx = None;
                    t.term.exited = Some(format!("Could not open a shell: {error}"));
                }
            }
            SessionMsg::Listing { path, result } => self.browser.set_listing(path, result),
            SessionMsg::OpDone { result } => {
                match result {
                    Ok(msg) => self.notify(msg, false),
                    Err(e) => self.notify(e, true),
                }
                self.refresh(shared);
            }
            SessionMsg::Opened { path, result, reload, restore } => match result {
                Ok((session, access)) if restore.is_some() => {
                    let id = self.next();
                    let check = find_rule(&settings.validation_rules, &session.path).cloned();
                    let mut ed = Editor::new(id, session, access, check);
                    ed.text = restore.unwrap();
                    ed.message = Some(("Backup loaded. Saving it restores the file.".into(), false));
                    // Review is forced here: restoring is exactly when you want to see what changes.
                    ed.request_save(true);
                    self.editors.push(ed);
                    self.active = Some(Pane::Editor(id));
                }
                Ok((session, access)) => {
                    if let Some(editor) = reload.and_then(|id| self.editors.iter_mut().find(|e| e.id == id)) {
                        editor.text = session.base_text.clone();
                        editor.session = session;
                        editor.access = access;
                        editor.state = EditorState::Ready;
                        editor.message = Some(("Reloaded from the server.".into(), false));
                    } else {
                        let id = self.next();
                        let check = find_rule(&settings.validation_rules, &session.path).cloned();
                        self.editors.push(Editor::new(id, session, access, check));
                        self.active = Some(Pane::Editor(id));
                    }
                }
                Err(e) => {
                    if let Some(editor) = reload.and_then(|id| self.editors.iter_mut().find(|e| e.id == id)) {
                        editor.state = EditorState::Ready;
                        editor.message = Some((e, true));
                    } else {
                        self.notify(format!("Could not open {path}: {e}"), true);
                    }
                }
            },
            SessionMsg::Saved { editor, result } => {
                let Some(ed) = self.editors.iter_mut().find(|e| e.id == editor) else { return };
                match result {
                    Ok((session, report)) => {
                        ed.session = session;
                        ed.state = EditorState::Ready;
                        self.saves_done += 1;
                        let mut msg =
                            format!("Saved {} (save #{}).", sftp::human_size(report.bytes as u64), self.saves_done);
                        if let Some(b) = &report.remote_backup {
                            msg.push_str(&format!(" Server backup: {b}."));
                        }
                        if report.local_backup.is_some() {
                            msg.push_str(" Original kept in local backups.");
                        }
                        let mut warn = false;
                        if let Some(c) = &report.check {
                            match &c.outcome {
                                CheckOutcome::Passed { .. } => msg.push_str(&format!(" {} check passed.", c.rule)),
                                CheckOutcome::Unavailable { .. } => {
                                    warn = true;
                                    msg.push_str(&format!(" Not checked: the {} check tool is not installed.", c.rule));
                                }
                            }
                            match &c.reload {
                                Some((true, _)) => msg.push_str(" Reloaded."),
                                Some((false, out)) => {
                                    warn = true;
                                    msg.push_str(&format!(" Reload FAILED: {out}"));
                                }
                                None => {}
                            }
                        }
                        ed.message = Some((msg, warn));
                        let refresh = sftp::parent(&ed.session.path) == self.browser.path;
                        // Keep a watched external file in step with what was saved.
                        if let Some(x) = self.externals.iter_mut().find(|x| Some(&x.local) == ed.external.as_ref()) {
                            x.session = ed.session.clone();
                        }
                        if refresh {
                            self.refresh(shared);
                        }
                    }
                    Err(SaveFailure::Conflict(current)) => {
                        let theirs = text::decode(&current)
                            .map(|(t, _)| t)
                            .unwrap_or_else(|_| String::from_utf8_lossy(&current).into_owned());
                        let diff = malasdoh_core::diff::diff(&theirs, &ed.text, 3);
                        let merged = malasdoh_core::diff::merge3(&ed.session.base_text, &ed.text, &theirs);
                        ed.state = EditorState::Conflict { diff, theirs: current, merged };
                    }
                    Err(SaveFailure::Check { rule, output, rolled_back, session }) => {
                        ed.session = *session;
                        ed.state = EditorState::CheckFailed { rule, output, rolled_back };
                    }
                    Err(SaveFailure::PermissionDenied(e)) if !ed.access.is_sudo() => {
                        ed.state = EditorState::NeedsSudo(e)
                    }
                    Err(SaveFailure::PermissionDenied(e)) | Err(SaveFailure::Other(e)) => {
                        ed.state = EditorState::Ready;
                        ed.message = Some((format!("Not saved: {e}"), true));
                    }
                }
            }
            SessionMsg::TransferDone { index, result } => {
                if let Some(t) = self.browser.transfers.get_mut(index) {
                    t.result = Some(result);
                }
                self.refresh(shared);
            }
            SessionMsg::ExternalReady { session, local, access } => {
                let mtime = std::fs::metadata(&local).and_then(|m| m.modified()).ok();
                if let Err(e) = launch_editor(&settings.external_editor, &local) {
                    self.notify(format!("Could not start the external editor: {e}"), true);
                } else {
                    self.notify(
                        format!("Editing {} externally; saves there are uploaded after review.", session.path),
                        false,
                    );
                }
                self.externals.retain(|x| x.session.path != session.path);
                self.externals.push(ExternalEdit { local, session, access, mtime });
            }
            SessionMsg::SudoReady { password, then } => match password {
                Ok(password) => {
                    self.sudo = Some(Access::Sudo { password });
                    match then {
                        AfterSudo::Open(path) => self.open_file(path, true, shared),
                        AfterSudo::Save(id) => {
                            let access = self.sudo.clone().unwrap();
                            if let Some(ed) = self.editors.iter_mut().find(|e| e.id == id) {
                                ed.access = access;
                            }
                            self.save_editor(id, false, true, shared, settings);
                        }
                    }
                }
                Err(e) => {
                    if let AfterSudo::Save(id) = then
                        && let Some(ed) = self.editors.iter_mut().find(|e| e.id == id)
                    {
                        ed.state = EditorState::Ready;
                        ed.message = Some((format!("sudo: {e}"), true));
                    } else {
                        self.notify(format!("sudo: {e}"), true);
                    }
                }
            },
        }
    }

    fn io(&self, access: Access) -> Option<RemoteIo> {
        Some(RemoteIo { fs: self.fs.clone()?, conn: self.conn.clone()?, access })
    }

    pub fn navigate(&mut self, path: String, shared: &Shared) {
        let Some(fs) = self.fs.clone() else { return };
        self.browser.loading = true;
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = async {
                let path = fs.canonicalize(&path).await?;
                let entries = fs.list(&path).await?;
                anyhow::Ok((path, entries))
            }
            .await;
            let msg = match result {
                Ok((path, entries)) => SessionMsg::Listing { path, result: Ok(entries) },
                Err(e) => SessionMsg::Listing { path, result: Err(format!("{e:#}")) },
            };
            events.session(id, msg);
        });
    }

    fn refresh(&mut self, shared: &Shared) {
        if !self.browser.path.is_empty() {
            self.navigate(self.browser.path.clone(), shared);
        }
    }

    /// Run a quick filesystem operation and report the result as a toast.
    fn fs_op<F, Fut>(&self, shared: &Shared, done: String, f: F)
    where
        F: FnOnce(RemoteFs) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send,
    {
        let Some(fs) = self.fs.clone() else { return };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = f(fs).await.map(|_| done).map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::OpDone { result });
        });
    }

    fn open_file(&mut self, path: String, sudo: bool, shared: &Shared) {
        if let Some(ed) = self.editors.iter().find(|e| e.session.path == path && e.access.is_sudo() == sudo) {
            self.active = Some(Pane::Editor(ed.id));
            return;
        }
        if sudo && self.sudo.is_none() {
            self.ensure_sudo(AfterSudo::Open(path), shared);
            return;
        }
        let access = if sudo { self.sudo.clone().unwrap() } else { Access::Sftp };
        let Some(io) = self.io(access.clone()) else { return };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = io.open(&path).await.map(|s| (s, access)).map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::Opened { path, result, reload: None, restore: None });
        });
    }

    /// Open `path` with a backup's contents in the editor, ready to review and save.
    fn restore_backup(&mut self, path: String, backup: PathBuf, shared: &Shared) {
        let text = match std::fs::read(&backup)
            .map_err(|e| e.to_string())
            .and_then(|b| text::decode(&b).map(|(t, _)| t).map_err(|e| e.to_string()))
        {
            Ok(t) => t,
            Err(e) => return self.notify(format!("Cannot read {}: {e}", backup.display()), true),
        };
        let Some(io) = self.io(Access::Sftp) else { return self.notify("Not connected", true) };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = io.open(&path).await.map(|s| (s, Access::Sftp)).map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::Opened { path, result, reload: None, restore: Some(text) });
        });
    }

    fn reload_editor(&mut self, editor: u64, shared: &Shared) {
        let Some(ed) = self.editors.iter_mut().find(|e| e.id == editor) else { return };
        ed.state = EditorState::Busy("Reloading…".into());
        let (path, access) = (ed.session.path.clone(), ed.access.clone());
        let Some(io) = self.io(access.clone()) else { return };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = io.open(&path).await.map(|s| (s, access)).map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::Opened { path, result, reload: Some(editor), restore: None });
        });
    }

    fn save_editor(&mut self, editor: u64, force: bool, check: bool, shared: &Shared, _settings: &Settings) {
        let backup_root = edit::host_backup_dir(&shared.paths.backups_dir(), &self.profile.host, self.profile.port);
        let remote_backup = self.profile.remote_backup;
        let Some(ed) = self.editors.iter_mut().find(|e| e.id == editor) else { return };
        let Some(io) = (match (&self.fs, &self.conn) {
            (Some(fs), Some(conn)) => Some(RemoteIo { fs: fs.clone(), conn: conn.clone(), access: ed.access.clone() }),
            _ => None,
        }) else {
            ed.message = Some(("Not connected — reconnect, then save again. Your changes are kept.".into(), true));
            ed.state = EditorState::Ready;
            return;
        };
        ed.state = EditorState::Busy("Saving…".into());
        let mut session = ed.session.clone();
        let text = ed.text.clone();
        let validation = if check && ed.check_enabled { ed.check.clone() } else { None };
        let opts = SaveOptions { local_backup_dir: Some(backup_root), remote_backup, force, validation };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = match io.save(&mut session, &text, &opts).await {
                Ok(report) => Ok((session, report)),
                Err(e) => Err(match e.downcast_ref::<SaveError>() {
                    Some(SaveError::Conflict { current }) => SaveFailure::Conflict(current.clone()),
                    Some(SaveError::ValidationFailed { rule, output, rolled_back }) => SaveFailure::Check {
                        rule: rule.clone(),
                        output: output.clone(),
                        rolled_back: *rolled_back,
                        session: Box::new(session.clone()),
                    },
                    _ => {
                        let msg = format!("{e:#}");
                        if msg.to_lowercase().contains("permission denied") {
                            SaveFailure::PermissionDenied(msg)
                        } else {
                            SaveFailure::Other(msg)
                        }
                    }
                }),
            };
            events.session(id, SessionMsg::Saved { editor, result });
        });
    }

    /// Check sudo works, asking for the password if it needs one.
    fn ensure_sudo(&mut self, then: AfterSudo, shared: &Shared) {
        let Some(conn) = self.conn.clone() else { return };
        let (events, id, host) = (shared.events.clone(), self.id, self.profile.host.clone());
        shared.rt.spawn(async move {
            let mut password: Option<String> = None;
            let mut retry = false;
            let result = loop {
                match RemoteIo::check_sudo(&conn, password.as_deref()).await {
                    Ok(()) => break Ok(password),
                    Err(e) if matches!(e.downcast_ref::<SudoError>(), Some(SudoError::PasswordRequired)) => {
                        let (reply, rx) = oneshot::channel();
                        events.send(
                            id,
                            crate::bridge::EventKind::Question(Question::Sudo { host: host.clone(), retry, reply }),
                        );
                        match rx.await.ok().flatten() {
                            Some(p) => password = Some(p),
                            None => break Err("cancelled".to_string()),
                        }
                        retry = true;
                    }
                    Err(e) => break Err(format!("{e:#}")),
                }
            };
            events.session(id, SessionMsg::SudoReady { password: result, then });
        });
    }

    fn transfer(
        &mut self,
        label: String,
        shared: &Shared,
        f: impl FnOnce(RemoteFs, Arc<Progress>) -> futures::Transfer,
    ) {
        let Some(fs) = self.fs.clone() else { return };
        let progress = Arc::new(Progress::default());
        let index = self.browser.transfers.len();
        self.browser.transfers.push(Transfer { label, progress: progress.clone(), result: None });
        let fut = f(fs, progress);
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = fut.await.map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::TransferDone { index, result });
        });
    }

    pub fn browser_action(&mut self, action: browser::Action, shared: &Shared) {
        use browser::Action as A;
        match action {
            A::Navigate(p) => self.navigate(p, shared),
            A::Refresh => self.refresh(shared),
            A::OpenSync => self.sync.show_for(&self.browser.path.clone()),
            A::Edit { path, sudo } => self.open_file(path, sudo, shared),
            A::ExternalEdit(path) => {
                let Some(io) = self.io(Access::Sftp) else { return };
                let dir = shared.paths.edit_cache_dir().join(uuid::Uuid::new_v4().simple().to_string());
                let (events, id) = (shared.events.clone(), self.id);
                shared.rt.spawn(async move {
                    let result = async {
                        let session = io.open(&path).await?;
                        malasdoh_core::paths::create_private_dir(&dir)?;
                        let local = dir.join(sftp::local_name(sftp::file_name(&path))?);
                        malasdoh_core::paths::write_private(
                            &local,
                            &text::encode(&session.base_text, &session.format),
                        )?;
                        anyhow::Ok((session, local))
                    }
                    .await;
                    match result {
                        Ok((session, local)) => {
                            events.session(id, SessionMsg::ExternalReady { session, local, access: Access::Sftp })
                        }
                        Err(e) => events
                            .session(id, SessionMsg::OpDone { result: Err(format!("Could not open {path}: {e:#}")) }),
                    }
                });
            }
            A::Download(paths) => {
                let Some(dir) = rfd::FileDialog::new().set_title("Download to folder").pick_folder() else { return };
                for p in paths {
                    let dir = dir.clone();
                    let label = format!("⬇ {}", sftp::file_name(&p));
                    self.transfer(label, shared, move |fs, progress| {
                        Box::pin(async move {
                            let dest = fs.download(&p, &dir, &progress).await?;
                            Ok(format!("Downloaded {}", dest.display()))
                        })
                    });
                }
            }
            A::UploadPick => {
                if let Some(files) = rfd::FileDialog::new().set_title("Upload files").pick_files() {
                    self.browser_action(A::Upload(files), shared);
                }
            }
            A::Upload(files) => {
                let dir = self.browser.path.clone();
                for f in files {
                    let dir = dir.clone();
                    let label =
                        format!("⬆ {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                    self.transfer(label, shared, move |fs, progress| {
                        Box::pin(async move {
                            let dest = fs.upload(&f, &dir, &progress).await?;
                            Ok(format!("Uploaded {dest}"))
                        })
                    });
                }
            }
            A::Mkdir(p) => self.fs_op(shared, format!("Created {p}"), move |fs| async move { fs.mkdir(&p).await }),
            A::NewFile(p) => {
                self.fs_op(shared, format!("Created {p}"), move |fs| async move { fs.create_new(&p, b"", None).await })
            }
            A::Rename { from, to } => {
                self.fs_op(shared, format!("Renamed to {to}"), move |fs| async move { fs.rename(&from, &to).await })
            }
            A::Delete(paths) => {
                let n = paths.len();
                self.fs_op(shared, format!("Deleted {n} item(s)"), move |fs| async move {
                    for p in paths {
                        fs.remove(&p).await?;
                    }
                    Ok(())
                })
            }
            A::Chmod { path, mode } => {
                self.fs_op(shared, format!("Permissions of {path} set to {mode:o}"), move |fs| async move {
                    fs.chmod(&path, mode).await
                })
            }
            A::CdInTerminal(dir) => self.send_to_terminal(format!("cd {}\r", ssh::shell_quote(&dir)).into_bytes()),
            A::ShowBackups(path) => {
                let root = edit::host_backup_dir(&shared.paths.backups_dir(), &self.profile.host, self.profile.port);
                let list = edit::list_local_backups(&root, &path);
                self.backups_view = Some((path, list));
            }
        }
    }

    pub fn editor_action(&mut self, editor: u64, action: EditorAction, shared: &Shared, settings: &Settings) {
        match action {
            EditorAction::Save { force, check } => self.save_editor(editor, force, check, shared, settings),
            EditorAction::Reload => self.reload_editor(editor, shared),
            EditorAction::RetryWithSudo => {
                if let Some(ed) = self.editors.iter_mut().find(|e| e.id == editor) {
                    ed.state = EditorState::Busy("Checking sudo…".into());
                }
                match self.sudo.clone() {
                    Some(access) => {
                        if let Some(ed) = self.editors.iter_mut().find(|e| e.id == editor) {
                            ed.access = access;
                        }
                        self.save_editor(editor, false, true, shared, settings);
                    }
                    None => self.ensure_sudo(AfterSudo::Save(editor), shared),
                }
            }
        }
    }

    /// Notice saves made in an external editor (checked about once a second).
    pub fn poll_external(&mut self, shared: &Shared, settings: &Settings) {
        if self.last_external_check.elapsed().as_millis() < 800 || self.externals.is_empty() {
            return;
        }
        self.last_external_check = Instant::now();
        let mut changed = Vec::new();
        for x in &mut self.externals {
            let mtime = std::fs::metadata(&x.local).and_then(|m| m.modified()).ok();
            if mtime.is_some() && mtime != x.mtime {
                x.mtime = mtime;
                changed.push((x.local.clone(), x.session.clone(), x.access.clone()));
            }
        }
        for (local, session, access) in changed {
            let Ok(bytes) = std::fs::read(&local) else { continue };
            let text = match text::decode(&bytes) {
                Ok((t, _)) => t,
                Err(e) => {
                    self.notify(format!("{}: {e}", local.display()), true);
                    continue;
                }
            };
            let id = match self.editors.iter().position(|e| e.external.as_ref() == Some(&local)) {
                Some(i) => self.editors[i].id,
                None => {
                    let id = self.next();
                    let check = find_rule(&settings.validation_rules, &session.path).cloned();
                    let mut ed = Editor::new(id, session, access, check);
                    ed.external = Some(local.clone());
                    self.editors.push(ed);
                    id
                }
            };
            let ed = self.editors.iter_mut().find(|e| e.id == id).unwrap();
            ed.text = text;
            ed.message = Some(("Changed in the external editor.".into(), false));
            self.active = Some(Pane::Editor(id));
            // With review on this shows the diff first, as for any save.
            if ed.request_save(settings.review_before_save).is_some() {
                self.save_editor(id, false, true, shared, settings);
            }
        }
    }

    pub fn close_editor(&mut self, id: u64) {
        self.editors.retain(|e| e.id != id);
        if self.active == Some(Pane::Editor(id)) {
            self.active =
                self.terms.first().map(|t| Pane::Term(t.id)).or(self.editors.first().map(|e| Pane::Editor(e.id)));
        }
    }

    pub fn close_terminal(&mut self, id: u64) {
        self.terms.retain(|t| t.id != id);
        if self.active == Some(Pane::Term(id)) {
            self.active =
                self.terms.first().map(|t| Pane::Term(t.id)).or(self.editors.first().map(|e| Pane::Editor(e.id)));
        }
    }

    /// Draw the active pane (terminal or editor).
    pub fn pane_ui(&mut self, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        match self.active {
            Some(Pane::Term(id)) => {
                let Some(t) = self.terms.iter_mut().find(|t| t.id == id) else { return };
                t.term.set_font_size(settings.terminal_font_size);
                let out = t.term.ui(ui);
                if let Some(tx) = &t.tx {
                    if let Some((c, r)) = out.resize {
                        let _ = tx.send(ShellCmd::Resize(c, r));
                    }
                    if !out.send.is_empty() {
                        let _ = tx.send(ShellCmd::Input(out.send));
                    }
                }
            }
            Some(Pane::Editor(id)) => {
                let Some(ed) = self.editors.iter_mut().find(|e| e.id == id) else { return };
                if let Some(action) = ed.ui(ui, settings.editor_font_size, settings.review_before_save) {
                    self.editor_action(id, action, shared, settings);
                }
            }
            None => {
                ui.centered_and_justified(|ui| ui.label("Open a terminal or double-click a file to edit it."));
            }
        }
        self.sync.ui(ui.ctx(), self.fs.as_ref(), shared, self.id);
        if let Some((path, backup)) = self.backups_ui(ui.ctx()) {
            self.restore_backup(path, backup, shared);
        }
    }

    fn backups_ui(&mut self, ctx: &egui::Context) -> Option<(String, PathBuf)> {
        let (path, list) = self.backups_view.as_ref()?;
        let mut close = false;
        let mut restore = None;
        egui::Modal::new(egui::Id::new(("backups", self.id))).show(ctx, |ui| {
            ui.heading(format!("Local backups of {path}"));
            ui.label(RichText::new("Newest first. Times are UTC.").weak());
            if list.is_empty() {
                ui.label("None yet. A backup is made every time you save this file from MalasDoh.");
            }
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for b in list {
                    ui.horizontal(|ui| {
                        ui.monospace(b.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                        if ui
                            .small_button("Restore…")
                            .on_hover_text("Open this version in the editor and review the change before saving")
                            .clicked()
                        {
                            restore = Some((path.clone(), b.clone()));
                        }
                        if ui.small_button("Copy path").clicked() {
                            ui.ctx().copy_text(b.display().to_string());
                        }
                    });
                }
            });
            if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
        if close || restore.is_some() {
            self.backups_view = None;
        }
        restore
    }

    /// Notice a connection that dropped (network, server reboot).
    pub fn check_alive(&mut self) {
        if self.state == ConnState::Connected && self.conn.as_ref().is_some_and(|c| c.is_closed()) {
            self.state = ConnState::Disconnected;
            for t in &mut self.terms {
                t.tx = None;
            }
            self.forwards.on_disconnected();
        }
    }

    /// The forwards window. Returns the list to save to the profile, if the
    /// user changed what is saved.
    pub fn forwards_ui(
        &mut self,
        ctx: &egui::Context,
        shared: &Shared,
    ) -> Option<Vec<malasdoh_core::forward::ForwardSpec>> {
        let title = self.profile.display_name();
        let conn = self.conn.clone().filter(|_| self.state == ConnState::Connected);
        self.forwards.ui(ctx, &title, conn.as_ref(), shared, self.id).then(|| self.forwards.saved_specs())
    }
}

mod futures {
    pub type Transfer = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>;
}

/// Start the user's editor on a local file without waiting for it.
fn launch_editor(command: &str, file: &std::path::Path) -> std::io::Result<()> {
    let command = command.trim();
    if !command.is_empty() {
        let file_s = file.display().to_string();
        let mut parts: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if parts.iter().any(|p| p.contains("{file}")) {
            for p in &mut parts {
                *p = p.replace("{file}", &file_s);
            }
        } else {
            parts.push(file_s);
        }
        std::process::Command::new(&parts[0]).args(&parts[1..]).spawn()?;
        return Ok(());
    }
    #[cfg(windows)]
    {
        std::process::Command::new("notepad.exe").arg(file).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg("-t").arg(file).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(file).spawn()?;
    }
    Ok(())
}
