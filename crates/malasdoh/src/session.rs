//! One connection tab: its SSH connection, terminals, file browser and open
//! files. UI methods never block; every network operation runs as a task and
//! reports back as a [`SessionMsg`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use egui::RichText;
use malasdoh_core::edit::{self, Access, EditSession, RemoteIo, SaveError, SaveOptions, SaveReport, SudoError};
use malasdoh_core::journal::{self, Journal};
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
use crate::compare::{CompareRequest, Side};
use crate::editor::{Editor, EditorAction, State as EditorState};
use crate::find_ui::{FindAction, FindWindow};
use crate::forwards::{ForwardMsg, Forwards};
use crate::local::{LocalAction, LocalBrowser};
use crate::logview::{LogAction, LogView};
use crate::settings::Settings;
use crate::sync_ui::{SyncMsg, SyncWindow};
use crate::terminal::Terminal;
use malasdoh_core::commands;

/// What every session needs from the app.
#[derive(Clone)]
pub struct Shared {
    pub rt: tokio::runtime::Handle,
    pub events: Events,
    pub secrets: Arc<dyn SecretStore>,
    pub known_hosts: KnownHosts,
    pub paths: AppPaths,
}

/// Typed into a new terminal when "follow the terminal" is on: bash and zsh
/// then report their folder (OSC 7) at each prompt. The screen is cleared so
/// the typed line does not stay; the leading space keeps it out of history
/// where the shell ignores such lines.
const FOLLOW_HOOK: &str = concat!(
    r#" if [ -n "$ZSH_VERSION" ]; then _malasdoh_cwd() { printf '\033]7;file://%s%s\007' "$HOST" "$PWD"; };"#,
    // `eval`: POSIX sh cannot even parse zsh's array syntax.
    r#" eval 'precmd_functions+=(_malasdoh_cwd)'; elif [ -n "$BASH_VERSION" ]; then"#,
    r#" PROMPT_COMMAND='printf "\033]7;file://%s%s\007" "$HOSTNAME" "$PWD"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi; clear"#,
    "\r"
);

pub enum SessionMsg {
    LogData {
        id: u64,
        data: Vec<u8>,
    },
    LogEnded {
        id: u64,
        why: String,
    },
    FindDone {
        result: Result<Vec<String>, String>,
    },
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
    /// A file operation command, and what to say when it worked.
    Run {
        cmd: String,
        what: &'static str,
        done: String,
    },
    Follow(String),
    Find(String),
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
    Log(u64),
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
    /// The local file pane, when shown.
    pub local: Option<LocalBrowser>,
    /// Paths the user asked to copy to another server; the app picks it up.
    pub cross_copy: Option<Vec<String>>,
    /// A transfer waiting for "replace existing?".
    overwrite: Option<Overwrite>,
    /// A comparison to open; the app picks it up.
    pub compare: Option<CompareRequest>,
    /// A path whose change history to show; the app picks it up.
    pub history: Option<String>,
    /// What to reopen once connected (from the last run).
    pub restore: Option<crate::restore::Tab>,
    pub logs: Vec<LogView>,
    pub find: FindWindow,
    /// The bookmarks changed; the app saves them to the connection.
    pub bookmarks_changed: bool,
    /// The pane shown beside the active one.
    pub split: Option<Pane>,
    /// What was typed into terminals this frame (terminal id, bytes), for
    /// sending to other servers too.
    pub typed: Vec<(u64, Vec<u8>)>,
}

/// A transfer whose destination already has some of the same names.
enum Overwrite {
    Upload { files: Vec<PathBuf>, dir: String, existing: Vec<String> },
    Download { paths: Vec<String>, dir: PathBuf, existing: Vec<String> },
}

impl Session {
    pub fn new(id: SessionId, chain: Vec<Profile>) -> Self {
        let profile = chain.last().cloned().expect("chain has a target");
        let forwards = Forwards::new(&profile.forwards);
        let bookmarks = profile.bookmarks.clone();
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
            browser: {
                let mut b = Browser::new(id);
                b.bookmarks = bookmarks;
                b
            },
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
            local: None,
            cross_copy: None,
            overwrite: None,
            compare: None,
            history: None,
            restore: None,
            logs: Vec::new(),
            find: FindWindow::default(),
            bookmarks_changed: false,
            split: None,
            typed: Vec::new(),
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
                        let fs = conn.open_fs().await?;
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
        // Queued before the shell is open; the shell reads them as typed-ahead
        // input. The folder hook goes first, so a startup command like
        // `sudo -i` cannot swallow it as a password.
        if self.profile.follow_terminal {
            let _ = tx.send(ShellCmd::Input(FOLLOW_HOOK.as_bytes().to_vec()));
        }
        let startup = self.profile.startup_command.trim();
        if !startup.is_empty() {
            let _ = tx.send(ShellCmd::Input(format!("{startup}\r").into_bytes()));
        }
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

    /// What to reopen next time.
    pub fn snapshot(&self) -> crate::restore::Tab {
        crate::restore::Tab {
            profile: self.profile.id,
            dir: self.browser.path.clone(),
            files: self
                .editors
                .iter()
                .map(|e| crate::restore::OpenFile { path: e.session.path.clone(), sudo: e.access.is_sudo() })
                .collect(),
            terminals: self.terms.len().max(1),
            local_dir: self.local.as_ref().map(|l| l.path.clone()),
        }
    }

    /// Typing from another server's terminal ("type into several"): goes to
    /// this session's current terminal, without switching panes.
    pub fn echo_input(&self, bytes: &[u8]) {
        let target = match self.active {
            Some(Pane::Term(id)) => Some(id),
            _ => self.terms.iter().find(|t| t.is_live()).map(|t| t.id),
        };
        if let Some(tx) = self.terms.iter().find(|t| Some(t.id) == target).and_then(|t| t.tx.as_ref()) {
            let _ = tx.send(ShellCmd::Input(bytes.to_vec()));
        }
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
            SessionMsg::LogData { id, data } => {
                if let Some(l) = self.logs.iter_mut().find(|l| l.id == id) {
                    l.push(&data);
                }
            }
            SessionMsg::LogEnded { id, why } => {
                if let Some(l) = self.logs.iter_mut().find(|l| l.id == id) {
                    l.end(why);
                }
            }
            SessionMsg::FindDone { result } => self.find.done(result),
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
                        self.browser.note = fs
                            .is_shell()
                            .then(|| "No SFTP on this server: files go through shell commands.".to_string());
                        self.fs = Some(fs);
                        let dir = match &self.restore {
                            Some(r) if !r.dir.is_empty() => r.dir.clone(),
                            _ => dir,
                        };
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
                if let Some(r) = self.restore.take() {
                    while self.terms.len() < r.terminals.min(8) {
                        self.open_terminal(shared, settings);
                    }
                    if let Some(d) = r.local_dir {
                        self.local = Some(LocalBrowser::new(Some(d)));
                    }
                    for f in r.files {
                        self.open_file(f.path, f.sudo, shared);
                    }
                    self.active = self.terms.first().map(|t| Pane::Term(t.id));
                }
            }
            SessionMsg::Failed { error, forgettable_key } => {
                self.state = ConnState::Failed { error, forgettable_key };
            }
            SessionMsg::ShellOutput { term, data } => {
                let mut follow = None;
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == term) {
                    let before = t.term.cwd.clone();
                    let reply = t.term.feed(&data);
                    // The first report is where the shell started; follow
                    // changes after that, so a start folder is kept.
                    if self.profile.follow_terminal && t.term.cwd_reports > 1 && t.term.cwd != before {
                        follow = t.term.cwd.clone();
                    }
                    if !reply.is_empty()
                        && let Some(tx) = &t.tx
                    {
                        let _ = tx.send(ShellCmd::Input(reply));
                    }
                }
                if let Some(dir) = follow
                    && dir != self.browser.path
                {
                    self.navigate(dir, shared);
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
                if let Some(l) = &mut self.local {
                    l.refresh();
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
                        AfterSudo::Run { cmd, what, done } => self.run_op(cmd, what, done, true, shared),
                        AfterSudo::Follow(path) => self.follow(path, true, shared),
                        AfterSudo::Find(cmd) => self.run_find(cmd, true, shared),
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
                        if matches!(then, AfterSudo::Find(_)) {
                            self.find.done(Err(format!("sudo: {e}")));
                        }
                        self.notify(format!("sudo: {e}"), true);
                    }
                }
            },
        }
    }

    fn io(&self, access: Access) -> Option<RemoteIo> {
        Some(RemoteIo { fs: self.fs.clone()?, conn: self.conn.clone()?, access })
    }

    /// For reading files: plain access, and sudo if it was set up already.
    pub fn readers(&self) -> Option<(RemoteIo, Option<RemoteIo>)> {
        Some((self.io(Access::Sftp)?, self.sudo.clone().and_then(|a| self.io(a))))
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
    /// The access to use: plain, or sudo once it is set up. When sudo is
    /// asked for but not ready, it is set up first and `then` runs after.
    fn access_for(&mut self, sudo: bool, then: AfterSudo, shared: &Shared) -> Option<Access> {
        if !sudo {
            return Some(Access::Sftp);
        }
        match &self.sudo {
            Some(a) => Some(a.clone()),
            None => {
                self.ensure_sudo(then, shared);
                None
            }
        }
    }

    /// Run a shell command for a file operation, report how it went, and
    /// refresh the listing.
    /// `what` completes "Could not …" when it fails.
    fn run_op(&mut self, cmd: String, what: &'static str, done: String, sudo: bool, shared: &Shared) {
        let Some(access) = self.access_for(sudo, AfterSudo::Run { cmd: cmd.clone(), what, done: done.clone() }, shared)
        else {
            return;
        };
        let Some(io) = self.io(access) else { return self.notify("Not connected", true) };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = match io.run(&cmd).await {
                Ok(out) if out.success() => Ok(done),
                Ok(out) => {
                    let why = out.stderr_text();
                    let why = if why.is_empty() { format!("exit status {:?}", out.status) } else { why };
                    Err(format!("Could not {what}: {why}"))
                }
                Err(e) => Err(format!("Could not {what}: {e:#}")),
            };
            events.session(id, SessionMsg::OpDone { result });
        });
    }

    fn run_find(&mut self, cmd: String, sudo: bool, shared: &Shared) {
        let Some(access) = self.access_for(sudo, AfterSudo::Find(cmd.clone()), shared) else { return };
        let Some(io) = self.io(access) else {
            return self.find.done(Err("Not connected".into()));
        };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = match io.run(&cmd).await {
                Ok(out) => Ok(String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect()),
                Err(e) => Err(format!("{e:#}")),
            };
            events.session(id, SessionMsg::FindDone { result });
        });
    }

    /// Open a pane following `path` as it grows.
    pub fn follow(&mut self, path: String, sudo: bool, shared: &Shared) {
        let Some(access) = self.access_for(sudo, AfterSudo::Follow(path.clone()), shared) else { return };
        let Some(conn) = self.conn.clone() else { return self.notify("Not connected", true) };
        let tail = commands::follow(&path, 500);
        let (cmd, stdin) = match &access {
            Access::Sftp => (tail, None),
            Access::Sudo { password } => (
                format!("sudo -S -p '' -- sh -c {}", ssh::shell_quote(&tail)),
                password.as_ref().map(|p| format!("{p}\n").into_bytes()),
            ),
        };
        let id = self.next();
        let (stop, mut stopped) = oneshot::channel::<()>();
        self.logs.push(LogView::new(id, path, access.is_sudo(), stop));
        self.active = Some(Pane::Log(id));
        let (events, session) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let mut stream = match conn.exec_stream(&cmd, stdin).await {
                Ok(s) => s,
                Err(e) => return events.session(session, SessionMsg::LogEnded { id, why: format!("{e:#}") }),
            };
            let mut stderr = Vec::new();
            loop {
                tokio::select! {
                    ev = stream.events.recv() => match ev {
                        Some(ssh::StreamEvent::Stdout(data)) => events.session(session, SessionMsg::LogData { id, data }),
                        Some(ssh::StreamEvent::Stderr(e)) => stderr.extend_from_slice(&e),
                        ev @ (Some(ssh::StreamEvent::Exited(_)) | None) => {
                            let status = match ev { Some(ssh::StreamEvent::Exited(s)) => s, _ => None };
                            let err = String::from_utf8_lossy(&stderr).trim().to_string();
                            let why = match status {
                                _ if !err.is_empty() => err,
                                Some(s) => format!("Stopped (exit status {s})"),
                                None => "Stopped".into(),
                            };
                            events.session(session, SessionMsg::LogEnded { id, why });
                            break;
                        }
                    },
                    // The pane was closed: dropping the stream stops tail.
                    _ = &mut stopped => break,
                }
            }
        });
    }

    pub fn close_log(&mut self, id: u64) {
        self.logs.retain(|l| l.id != id);
        if self.active == Some(Pane::Log(id)) {
            self.active = self.terms.first().map(|t| Pane::Term(t.id));
        }
    }

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
        let journal = Journal::new(shared.paths.journal_file());
        let entry = journal::Entry {
            time: 0,
            connection: self.profile.display_name(),
            user: self.profile.user.clone(),
            host: self.profile.host.clone(),
            port: self.profile.port,
            path: session.path.clone(),
            outcome: journal::Outcome::Saved,
            sudo: ed.access.is_sudo(),
            added: 0,
            removed: 0,
            check: None,
            local_backup: None,
            remote_backup: None,
            patch: String::new(),
            truncated: false,
        };
        shared.rt.spawn(async move {
            let saved = io.save(&mut session, &text, &opts).await;
            // Record what happened to the server's file, then report.
            let record = match &saved {
                Ok(report) => Some(
                    journal::Entry {
                        check: report.check.as_ref().map(|c| {
                            let how = match c.outcome {
                                CheckOutcome::Passed { .. } => "passed",
                                CheckOutcome::Unavailable { .. } => "not installed",
                            };
                            format!("{}: {how}", c.rule)
                        }),
                        local_backup: report.local_backup.clone(),
                        remote_backup: report.remote_backup.clone(),
                        ..entry
                    }
                    .with_change(&report.previous, &text),
                ),
                // After a rollback the session is based on what is back on
                // the server.
                Err(e) => match e.downcast_ref::<SaveError>() {
                    Some(SaveError::ValidationFailed { rule, rolled_back: true, .. }) => Some(
                        journal::Entry {
                            outcome: journal::Outcome::RolledBack,
                            check: Some(format!("{rule}: failed")),
                            ..entry
                        }
                        .with_change(&session.base_text, &text),
                    ),
                    Some(SaveError::RollbackFailed { rule, backup, .. }) => Some(
                        journal::Entry {
                            outcome: journal::Outcome::RollbackFailed,
                            check: Some(format!("{rule}: failed")),
                            local_backup: backup.clone(),
                            ..entry
                        }
                        .with_change(&session.base_text, &text),
                    ),
                    Some(SaveError::VerifyFailed { backup }) => Some(
                        journal::Entry {
                            outcome: journal::Outcome::VerifyFailed,
                            local_backup: backup.clone(),
                            ..entry
                        }
                        .with_change(&session.base_text, &text),
                    ),
                    _ => None,
                },
            };
            if let Some(mut r) = record {
                r.time = journal::now();
                if let Err(e) = journal.append(&r) {
                    tracing::warn!("could not write the change journal: {e:#}");
                    let msg = format!(
                        "The change journal could not be written ({e:#}); the save itself is reported in the editor."
                    );
                    events.session(id, SessionMsg::OpDone { result: Err(msg) });
                }
            }
            let result = match saved {
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

    /// Download into a local folder, asking first if that would replace
    /// anything there.
    pub fn download_to(&mut self, paths: Vec<String>, dir: PathBuf, shared: &Shared) {
        let existing: Vec<String> = paths
            .iter()
            .map(|p| sftp::file_name(p).to_string())
            .filter(|n| sftp::local_name(n).is_ok_and(|n| dir.join(n).symlink_metadata().is_ok()))
            .collect();
        if existing.is_empty() {
            self.start_download(paths, dir, shared);
        } else {
            self.overwrite = Some(Overwrite::Download { paths, dir, existing });
        }
    }

    fn start_upload(&mut self, files: Vec<PathBuf>, dir: String, shared: &Shared) {
        for f in files {
            let dir = dir.clone();
            let label = format!("⬆ {}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            self.transfer(label, shared, move |fs, progress| {
                Box::pin(async move {
                    let dest = fs.upload(&f, &dir, &progress).await?;
                    Ok(format!("Uploaded {dest}"))
                })
            });
        }
    }

    fn start_download(&mut self, paths: Vec<String>, dir: PathBuf, shared: &Shared) {
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

    pub fn connection(&self) -> Option<Arc<Connection>> {
        self.conn.clone()
    }

    pub fn fs(&self) -> Option<RemoteFs> {
        self.fs.clone()
    }

    /// A transfer run by someone else (a copy between servers), shown in this
    /// session's list; finish it with `SessionMsg::TransferDone { index, .. }`.
    pub fn add_transfer(&mut self, label: String) -> (usize, Arc<Progress>) {
        let progress = Arc::new(Progress::default());
        self.browser.transfers.push(Transfer { label, progress: progress.clone(), result: None });
        (self.browser.transfers.len() - 1, progress)
    }

    /// Show the local pane's actions in this session.
    pub fn local_ui(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let Some(local) = &mut self.local else { return };
        for a in local.ui(ui, self.id) {
            match a {
                LocalAction::Upload(paths) => self.browser_action(browser::Action::Upload(paths), shared),
                LocalAction::Download { paths, dir } => self.download_to(paths, dir, shared),
            }
        }
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
            A::CopyToServer(paths) => self.cross_copy = Some(paths),
            A::Compare(paths) => {
                self.compare = Some(match paths.as_slice() {
                    [a, b] => CompareRequest::Pair(Side::remote(self, a), Side::remote(self, b)),
                    _ => CompareRequest::Pick { path: paths.into_iter().next().unwrap_or_default() },
                })
            }
            A::History(path) => self.history = Some(path),
            A::Chown { paths, owner, recursive, sudo } => match commands::chown(&paths, &owner, recursive) {
                Ok(cmd) => self.run_op(cmd, "change the owner", format!("Owner set to {owner}"), sudo, shared),
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            A::Symlink { target, link } => match commands::symlink(&target, &link) {
                Ok(cmd) => self.run_op(cmd, "create the link", format!("Created {link} → {target}"), false, shared),
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            A::Extract(path) => match commands::extract(&path) {
                Ok((cmd, dest)) => self.run_op(cmd, "unpack", format!("Unpacked to {dest}"), false, shared),
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            A::Compress { names, out, kind } => match commands::create(&self.browser.path, &names, &out, kind) {
                Ok(cmd) => self.run_op(cmd, "compress", format!("Created {out}"), false, shared),
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            A::Follow { path, sudo } => self.follow(path, sudo, shared),
            A::OpenFind => self.find.show(&self.browser.path.clone()),
            A::ToggleBookmark(dir) => {
                let b = &mut self.profile.bookmarks;
                if let Some(i) = b.iter().position(|x| *x == dir) {
                    b.remove(i);
                } else {
                    b.push(dir);
                    b.sort();
                }
                self.browser.bookmarks = b.clone();
                self.bookmarks_changed = true;
            }
            A::Download(paths) => {
                // With the local pane open, download into its folder.
                let dir = match &self.local {
                    Some(l) => l.path.clone(),
                    None => match rfd::FileDialog::new().set_title("Download to folder").pick_folder() {
                        Some(d) => d,
                        None => return,
                    },
                };
                self.download_to(paths, dir, shared);
            }
            A::UploadPick => {
                if let Some(files) = rfd::FileDialog::new().set_title("Upload files").pick_files() {
                    self.browser_action(A::Upload(files), shared);
                }
            }
            A::Upload(files) => {
                let dir = self.browser.path.clone();
                let existing: Vec<String> = files
                    .iter()
                    .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .filter(|n| self.browser.entries.iter().any(|e| &e.name == n))
                    .collect();
                if existing.is_empty() {
                    self.start_upload(files, dir, shared);
                } else {
                    self.overwrite = Some(Overwrite::Upload { files, dir, existing });
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
    /// Every pane, with a short label, in tab order.
    pub fn panes(&self) -> Vec<(Pane, String)> {
        let mut v: Vec<(Pane, String)> =
            self.terms.iter().enumerate().map(|(n, t)| (Pane::Term(t.id), format!("Terminal {}", n + 1))).collect();
        v.extend(self.logs.iter().map(|l| (Pane::Log(l.id), l.title())));
        v.extend(self.editors.iter().map(|e| (Pane::Editor(e.id), e.title())));
        v
    }

    /// Show a second pane beside the active one, or stop. With only one pane,
    /// a new terminal is opened for the other side.
    pub fn toggle_split(&mut self, shared: &Shared, settings: &Settings) {
        if self.split.take().is_some() {
            return;
        }
        let active = self.active;
        let other = self.panes().into_iter().map(|(p, _)| p).find(|p| Some(*p) != active);
        match other {
            Some(p) => self.split = Some(p),
            None if self.state == ConnState::Connected => {
                self.open_terminal(shared, settings);
                self.split = self.active;
                self.active = active;
            }
            None => {}
        }
    }

    pub fn pane_ui(&mut self, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        let panes = self.panes();
        // A split side that closed, or became the active pane, goes away.
        if self.split.is_some_and(|s| Some(s) == self.active || !panes.iter().any(|(p, _)| *p == s)) {
            self.split = None;
        }
        match (self.active, self.split) {
            (Some(left), Some(right)) => {
                ui.columns(2, |cols| {
                    // A header on both sides, so the two panes line up.
                    let name = panes.iter().find(|(p, _)| *p == left).map(|(_, n)| n.clone()).unwrap_or_default();
                    cols[0].horizontal(|ui| {
                        ui.add_enabled(false, egui::Button::new(name));
                    });
                    self.render_pane(Some(left), &mut cols[0], shared, settings);
                    let mut pick = None;
                    let mut close = false;
                    cols[1].horizontal(|ui| {
                        let name = panes.iter().find(|(p, _)| *p == right).map(|(_, n)| n.clone()).unwrap_or_default();
                        egui::ComboBox::from_id_salt(("split-pick", self.id)).selected_text(name).show_ui(ui, |ui| {
                            for (p, n) in panes.iter().filter(|(p, _)| *p != left) {
                                if ui.selectable_label(*p == right, n).clicked() {
                                    pick = Some(*p);
                                }
                            }
                        });
                        if ui.small_button("✖").on_hover_text("Back to one pane").clicked() {
                            close = true;
                        }
                    });
                    if let Some(p) = pick {
                        self.split = Some(p);
                    }
                    if close {
                        self.split = None;
                    } else {
                        self.render_pane(Some(right), &mut cols[1], shared, settings);
                    }
                });
            }
            (active, _) => self.render_pane(active, ui, shared, settings),
        }
        self.dialogs_ui(ui, shared);
    }

    fn render_pane(&mut self, pane: Option<Pane>, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        match pane {
            Some(Pane::Term(id)) => {
                let Some(t) = self.terms.iter_mut().find(|t| t.id == id) else { return };
                t.term.set_font_size(settings.terminal_font_size);
                let opts = crate::terminal::TermOptions {
                    theme: crate::themes::theme(&settings.terminal_theme),
                    copy_on_select: settings.copy_on_select,
                    confirm_paste: settings.confirm_paste,
                };
                let out = t.term.ui(ui, &opts);
                if out.start_log {
                    let host = edit::host_backup_dir(
                        &shared.paths.data_dir.join("logs"),
                        &self.profile.host,
                        self.profile.port,
                    );
                    let stamp = time::OffsetDateTime::now_utc()
                        .format(&time::macros::format_description!("[year][month][day]-[hour][minute][second]Z"))
                        .unwrap_or_default();
                    let path = host.join(format!("{stamp}-terminal{id}.log"));
                    match t.term.start_log(path.clone()) {
                        Ok(()) => self.toast = Some((format!("Logging to {}", path.display()), false, Instant::now())),
                        Err(e) => self.toast = Some((format!("Could not start the log: {e}"), true, Instant::now())),
                    }
                }
                if let Some(tx) = &t.tx {
                    if let Some((c, r)) = out.resize {
                        let _ = tx.send(ShellCmd::Resize(c, r));
                    }
                    if !out.send.is_empty() {
                        // Kept for "type into several terminals".
                        self.typed.push((id, out.send.clone()));
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
            Some(Pane::Log(id)) => {
                let Some(l) = self.logs.iter_mut().find(|l| l.id == id) else { return };
                if let Some(LogAction::Restart) = l.ui(ui) {
                    let (path, sudo) = (l.path.clone(), l.sudo);
                    self.close_log(id);
                    self.follow(path, sudo, shared);
                }
            }
            None => {
                ui.centered_and_justified(|ui| ui.label("Open a terminal or double-click a file to edit it."));
            }
        }
    }

    fn dialogs_ui(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        self.sync.ui(ui.ctx(), self.fs.as_ref(), shared, self.id);
        if let Some((path, backup)) = self.backups_ui(ui.ctx()) {
            self.restore_backup(path, backup, shared);
        }
        self.overwrite_ui(ui.ctx(), shared);
        match self.find.ui(ui.ctx(), self.id) {
            Some(FindAction::Search { dir, name, contains, depth, sudo }) => {
                match commands::find(&dir, &name, &contains, depth, crate::find_ui::LIMIT) {
                    Ok(cmd) => self.run_find(cmd, sudo, shared),
                    Err(e) => self.find.done(Err(format!("{e:#}"))),
                }
            }
            Some(FindAction::Reveal(p)) => {
                self.browser.reveal = Some(p.clone());
                self.navigate(sftp::parent(&p), shared);
            }
            Some(FindAction::Open(p)) => self.open_file(p, false, shared),
            None => {}
        }
    }

    fn overwrite_ui(&mut self, ctx: &egui::Context, shared: &Shared) {
        let Some(o) = &self.overwrite else { return };
        let (existing, place) = match o {
            Overwrite::Upload { existing, dir, .. } => (existing, format!("{dir} on {}", self.profile.name)),
            Overwrite::Download { existing, dir, .. } => (existing, dir.display().to_string()),
        };
        #[derive(PartialEq)]
        enum Choice {
            Replace,
            Skip,
            Cancel,
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::new(("overwrite", self.id))).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Replace existing items?");
            ui.label(format!("Already in {place}:"));
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                for n in existing {
                    ui.monospace(n);
                }
            });
            ui.label(
                RichText::new("Files with the same name are overwritten; folders are merged, replacing files inside.")
                    .weak(),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Replace").clicked() {
                    choice = Some(Choice::Replace);
                }
                if ui.button("Skip these").on_hover_text("Transfer only the items that are not there yet").clicked() {
                    choice = Some(Choice::Skip);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        let Some(choice) = choice else { return };
        let Some(o) = self.overwrite.take() else { return };
        if choice == Choice::Cancel {
            return;
        }
        let skip = choice == Choice::Skip;
        match o {
            Overwrite::Upload { mut files, dir, existing } => {
                if skip {
                    files
                        .retain(|f| !f.file_name().is_some_and(|n| existing.iter().any(|e| *e == n.to_string_lossy())));
                }
                self.start_upload(files, dir, shared);
            }
            Overwrite::Download { mut paths, dir, existing } => {
                if skip {
                    paths.retain(|p| !existing.iter().any(|e| e == sftp::file_name(p)));
                }
                self.start_download(paths, dir, shared);
            }
        }
    }

    fn backups_ui(&mut self, ctx: &egui::Context) -> Option<(String, PathBuf)> {
        let (path, list) = self.backups_view.as_ref()?;
        let mut close = false;
        let mut restore = None;
        let mut compare = None;
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
                        if ui
                            .small_button("Compare")
                            .on_hover_text("Compare this backup with the file on the server now")
                            .clicked()
                        {
                            compare = Some(b.clone());
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
        if let Some(b) = compare {
            let current = Side::remote(self, path);
            self.compare = Some(CompareRequest::Pair(Side::Local(b), current));
            close = true;
        }
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
