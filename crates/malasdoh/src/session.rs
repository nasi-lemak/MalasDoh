//! One connection tab: its SSH connection, terminals, file browser and open
//! files. UI methods never block; every network operation runs as a task and
//! reports back as a [`SessionMsg`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime};

use egui::RichText;
use malasdoh_core::drafts::Draft;
use malasdoh_core::edit::{self, Access, EditSession, RemoteIo, SaveError, SaveOptions, SaveReport};
use malasdoh_core::journal::{self, Journal};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::profile::Profile;
use malasdoh_core::secrets::SecretStore;
use malasdoh_core::sftp::{self, Progress, RemoteEntry, RemoteFs};
use malasdoh_core::ssh::{self, ConnectContext, Connection, HostKeyError, ShellEvent};
use malasdoh_core::transfer::{self, Ended, Job};
use malasdoh_core::validate::{CheckOutcome, find_rule};
use malasdoh_core::{AppPaths, text};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{Events, Question, SessionId, UiPrompter};
use crate::browser::{self, Browser};
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
    pub drafts: malasdoh_core::drafts::Drafts,
    pub rt: tokio::runtime::Handle,
    pub events: Events,
    pub secrets: Arc<dyn SecretStore>,
    pub known_hosts: KnownHosts,
    pub paths: AppPaths,
    /// WireGuard configurations (no keys), kept current by the app.
    pub wireguard: malasdoh_core::wireguard::Configs,
    /// Keys pasted into Tools → SSH keys, kept current by the app.
    pub stored_keys: malasdoh_core::stored_keys::StoredKeys,
    /// Settings → Transfers, kept current by the app.
    pub transfer: Arc<std::sync::RwLock<malasdoh_core::transfer::TransferOptions>>,
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

/// Typed into a new shell to start or rejoin lasting terminal `name`. `exec`,
/// so leaving tmux ends the SSH channel too; the leading space keeps it out
/// of history where the shell ignores such lines.
fn lasting_command(name: &str) -> String {
    format!(
        " if command -v tmux >/dev/null 2>&1; then exec tmux new-session -A -s {name}; \
         elif command -v screen >/dev/null 2>&1; then exec screen -D -RR -S {name}; \
         else echo 'MalasDoh: a lasting terminal needs tmux or screen on the server (e.g. apt install tmux).'; fi\r"
    )
}

/// Names MalasDoh gives lasting terminals (and so trusts in a command line).
fn is_lasting_name(name: &str) -> bool {
    name.strip_prefix("malasdoh-")
        .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

pub enum SessionMsg {
    /// Connecting needs a Teleport login for this cluster first.
    TeleportLogin(malasdoh_core::teleport::ClusterId),
    /// Lasting terminals on the server: name, and whether something is attached.
    LastingList(Vec<(String, bool)>),
    RootFs(Result<RemoteFs, String>),
    PatchChecked(Result<Vec<crate::patch_ui::FileCheck>, String>),
    PatchApplied(Vec<(String, Result<String, String>)>),
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
    /// A transfer run elsewhere (a copy between servers) finished.
    TransferDone {
        id: u64,
        result: Result<String, String>,
    },
    /// The files of a queued transfer are known, and which of them are
    /// already at the destination (`place` names it).
    JobPlanned {
        id: u64,
        job: Result<Job, String>,
        replaced: Vec<malasdoh_core::replace::Replaced>,
        place: String,
    },
    /// A reviewed upload backed up what it replaces and can start.
    JobReady {
        id: u64,
        job: Job,
        saved: Vec<(String, PathBuf, PathBuf)>,
        too_big: usize,
        failed: usize,
    },
    /// A run of a queued transfer ended.
    JobRan {
        id: u64,
        job: Job,
        ended: Result<Ended, String>,
    },
    ExternalReady {
        session: EditSession,
        local: PathBuf,
        access: Access,
    },
    SudoReady {
        password: Result<Option<String>, String>,
        warning: Option<String>,
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
    Restore(Box<Draft>),
    RootBrowse,
    PatchCheck {
        text: String,
        base: String,
        strip: usize,
    },
    PatchApply,
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
    /// The tmux (or screen) session this terminal runs in, for a "lasting"
    /// terminal: it keeps running on the server when the connection drops,
    /// and the tab goes back to it on reconnect.
    pub lasting: Option<String>,
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

/// On a narrow screen (a phone), the parts shown side by side on a wide one
/// take turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactView {
    /// Terminals and open files.
    #[default]
    Panes,
    /// The server's files.
    Files,
    /// This device's files.
    Local,
}

/// The upload's file dialog, whose answer may come later (phones).
pub const UPLOAD_ID: &str = "upload";

pub struct Session {
    pub id: SessionId,
    pub profile: Profile,
    chain: Vec<Profile>,
    pub state: ConnState,
    conn: Option<Arc<Connection>>,
    fs: Option<RemoteFs>,
    pub terms: Vec<TermPane>,
    /// The server's lasting terminals, for "Reattach"; `None` while asking.
    pub lasting_list: Option<Vec<(String, bool)>>,
    pub editors: Vec<Editor>,
    pub active: Option<Pane>,
    pub browser: Browser,
    pub questions: VecDeque<Question>,
    /// sudo access once it has been checked; kept in memory only.
    sudo: Option<Access>,
    /// Files copied at once by this connection's transfers.
    transfer_limit: Option<(usize, Arc<tokio::sync::Semaphore>)>,
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
    /// Transfers waiting for "Replace files?", first shown first.
    reviews: Vec<crate::replace_ui::Review>,
    /// Backups of server files that queued uploads replace, by queue row:
    /// (remote path, local file, backup), for the change journal.
    upload_backups: std::collections::HashMap<u64, Vec<(String, PathBuf, PathBuf)>>,
    /// A comparison to open; the app picks it up.
    pub compare: Option<CompareRequest>,
    /// A path whose change history to show; the app picks it up.
    pub history: Option<String>,
    /// "Edit on many servers" asked for this file (as root?).
    pub edit_many: Option<(String, bool)>,
    /// What to reopen once connected (from the last run).
    pub restore: Option<crate::restore::Tab>,
    pub logs: Vec<LogView>,
    pub find: FindWindow,
    /// The bookmarks changed; the app saves them to the connection.
    pub bookmarks_changed: bool,
    /// The pane shown beside the active one.
    pub split: Option<Pane>,
    /// On a narrow screen, which one of the side-by-side parts is shown.
    pub compact_view: CompactView,
    /// The active pane last frame: a file that opens is shown.
    pub compact_seen: Option<Pane>,
    /// What was typed into terminals this frame (terminal id, bytes), for
    /// sending to other servers too.
    pub typed: Vec<(u64, Vec<u8>)>,
    /// Unsaved edits from an earlier run, offered once connected.
    pub found_drafts: Vec<Draft>,
    /// Your own file access, kept while browsing as root.
    fs_user: Option<RemoteFs>,
    pub patch: crate::patch_ui::PatchWindow,
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
            lasting_list: None,
            editors: Vec::new(),
            active: None,
            browser: {
                let mut b = Browser::new(id);
                b.bookmarks = bookmarks;
                b
            },
            questions: VecDeque::new(),
            sudo: None,
            transfer_limit: None,
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
            reviews: Vec::new(),
            upload_backups: Default::default(),
            compare: None,
            history: None,
            edit_many: None,
            restore: None,
            logs: Vec::new(),
            find: FindWindow::default(),
            bookmarks_changed: false,
            split: None,
            compact_view: CompactView::default(),
            compact_seen: None,
            typed: Vec::new(),
            found_drafts: Vec::new(),
            fs_user: None,
            patch: Default::default(),
        }
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn status_color(&self, ctx: &egui::Context) -> egui::Color32 {
        let sem = crate::look::sem(ctx);
        match self.state {
            ConnState::Connected => sem.good,
            ConnState::Connecting(_) => sem.warn,
            _ => sem.bad,
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
            let mut ctx = ConnectContext::new(shared2.known_hosts.clone(), shared2.secrets.clone(), prompter);
            ctx.wireguard = shared2.wireguard.clone();
            ctx.stored_keys = shared2.stored_keys.clone();
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
                    if let Some(l) = e.downcast_ref::<malasdoh_core::teleport::LoginNeeded>() {
                        shared2.events.session(id, SessionMsg::TeleportLogin(l.cluster));
                    }
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
        self.fs_user = None;
        self.browser.root = false;
        for t in &mut self.terms {
            t.tx = None;
        }
        self.forwards.on_disconnected();
        self.state = ConnState::Disconnected;
    }

    pub fn open_terminal(&mut self, shared: &Shared, settings: &Settings) {
        self.add_terminal(None, shared, settings);
    }

    /// A terminal whose shell runs in tmux (or screen) on the server, so
    /// what runs in it survives a dropped connection. `attach` names one
    /// that is already there; otherwise a new one is started.
    pub fn open_lasting_terminal(&mut self, attach: Option<String>, shared: &Shared, settings: &Settings) {
        let name = attach.unwrap_or_else(|| format!("malasdoh-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
        self.add_terminal(Some(name), shared, settings);
    }

    fn add_terminal(&mut self, lasting: Option<String>, shared: &Shared, settings: &Settings) {
        let id = self.next();
        let Some(tx) = self.start_shell(id, lasting.as_deref(), shared) else { return };
        let mut term = Terminal::new(settings.terminal_font_size);
        term.take_focus = true;
        self.terms.push(TermPane { id, term, tx: Some(tx), lasting });
        self.active = Some(Pane::Term(id));
    }

    /// Open a shell for terminal `id`, or go back into its lasting session.
    fn start_shell(&self, id: u64, lasting: Option<&str>, shared: &Shared) -> Option<mpsc::UnboundedSender<ShellCmd>> {
        let conn = self.conn.clone()?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Queued before the shell is open; the shell reads them as typed-ahead
        // input. The folder hook goes first, so a startup command like
        // `sudo -i` cannot swallow it as a password.
        if let Some(name) = lasting {
            // Inside tmux the startup command would run again on every
            // reattach, so neither it nor the folder hook is typed.
            let _ = tx.send(ShellCmd::Input(lasting_command(name).into_bytes()));
        } else {
            if self.profile.follow_terminal {
                let _ = tx.send(ShellCmd::Input(FOLLOW_HOOK.as_bytes().to_vec()));
            }
            let startup = self.profile.startup_command.trim();
            if !startup.is_empty() {
                let _ = tx.send(ShellCmd::Input(format!("{startup}\r").into_bytes()));
            }
        }
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
        Some(tx)
    }

    /// Ask the server which lasting terminals (tmux sessions) it has.
    pub fn list_lasting(&mut self, shared: &Shared) {
        let Some(conn) = self.conn.clone() else { return };
        self.lasting_list = None;
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let out = conn.exec("tmux ls -F '#{session_name} #{session_attached}' 2>/dev/null", None).await;
            let list = out
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| {
                            let (name, attached) = l.split_once(' ')?;
                            is_lasting_name(name).then(|| (name.to_string(), attached.trim() != "0"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            events.session(id, SessionMsg::LastingList(list));
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
            lasting: self.terms.iter().filter_map(|t| t.lasting.clone()).collect(),
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
            SessionMsg::PatchChecked(result) => {
                self.patch.busy = false;
                self.patch.checked = Some(result);
            }
            SessionMsg::PatchApplied(results) => {
                self.patch.busy = false;
                let n = results.iter().filter(|(_, r)| r.is_ok()).count();
                self.notify(format!("Patch applied to {n} of {} file(s)", results.len()), n < results.len());
                self.patch.applied = results;
                self.refresh(shared);
            }
            SessionMsg::RootFs(result) => {
                self.browser.root_pending = false;
                match result {
                    Ok(fs) => {
                        self.fs_user = self.fs.replace(fs);
                        self.browser.root = true;
                        self.notify("Browsing as root. Changes here are made as root.", false);
                        self.refresh(shared);
                    }
                    Err(e) => self.notify(format!("Could not browse as root: {e}"), true),
                }
            }
            SessionMsg::Forward(m) => self.forwards.handle(m),
            SessionMsg::Sync(m) => {
                if let SyncMsg::Done(Ok(report), local, remote) = &m {
                    self.journal_synced(report, local, remote, shared);
                }
                self.sync.handle(m);
                self.refresh(shared);
            }
            SessionMsg::Connected { conn, fs } => {
                // A new connection starts as your own user.
                self.fs_user = None;
                self.browser.root = false;
                self.forwards.on_connected(&conn, shared, self.id);
                self.conn = Some(conn);
                self.state = ConnState::Connected;
                match fs {
                    Ok((fs, dir)) => {
                        self.browser.note = fs
                            .is_shell()
                            .then(|| "No SFTP on this server: files go through shell commands.".to_string());
                        self.fs = Some(fs);
                        // Transfers the lost connection interrupted carry on.
                        for id in self.browser.transfers.interrupted() {
                            self.run_job(id, shared);
                        }
                        let dir = match &self.restore {
                            Some(r) if !r.dir.is_empty() => r.dir.clone(),
                            _ => dir,
                        };
                        self.navigate(dir, shared);
                    }
                    Err(e) => self.browser.error = Some(format!("SFTP is not available: {e}")),
                }
                // Lasting terminals go back into their session on the server;
                // other terminals from before a reconnect are gone. Open one if
                // there are none.
                let back: Vec<(u64, String)> = self
                    .terms
                    .iter()
                    .filter(|t| t.tx.is_none())
                    .filter_map(|t| t.lasting.clone().map(|n| (t.id, n)))
                    .collect();
                for (id, name) in back {
                    let tx = self.start_shell(id, Some(&name), shared);
                    if let Some(t) = self.terms.iter_mut().find(|t| t.id == id) {
                        t.tx = tx;
                        t.term.exited = None;
                    }
                }
                let old: Vec<u64> = self.terms.iter().filter(|t| t.tx.is_none()).map(|t| t.id).collect();
                self.terms.retain(|t| !old.contains(&t.id));
                // Reopening lasting terminals counts as having one.
                let lasting_to_restore = self.restore.as_ref().is_some_and(|r| !r.lasting.is_empty());
                if self.terms.is_empty() && !lasting_to_restore {
                    self.open_terminal(shared, settings);
                }
                if self.active.is_none() || matches!(self.active, Some(Pane::Term(id)) if old.contains(&id)) {
                    self.active = self.terms.first().map(|t| Pane::Term(t.id));
                }
                // Unsaved edits left from a crash or a quit, not already open.
                let open: Vec<(String, bool)> =
                    self.editors.iter().map(|e| (e.session.path.clone(), e.access.is_sudo())).collect();
                self.found_drafts = shared
                    .drafts
                    .for_profile(self.profile.id)
                    .into_iter()
                    .filter(|d| !open.contains(&(d.session.path.clone(), d.sudo)))
                    .collect();
                if let Some(r) = self.restore.take() {
                    for name in r.lasting.iter().filter(|n| is_lasting_name(n)).take(8) {
                        if !self.terms.iter().any(|t| t.lasting.as_deref() == Some(name.as_str())) {
                            self.open_lasting_terminal(Some(name.clone()), shared, settings);
                        }
                    }
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
            SessionMsg::LastingList(list) => self.lasting_list = Some(list),
            // The app takes these before they get here.
            SessionMsg::TeleportLogin(_) => {}
            SessionMsg::ShellExited { term, status } => {
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == term) {
                    t.tx = None;
                    // Ended or detached on purpose (not a dropped connection):
                    // nothing to go back to.
                    if status.is_some() {
                        t.lasting = None;
                    }
                    t.term.exited = Some(match (status, &t.lasting) {
                        (Some(s), _) => format!("Shell exited with status {s}"),
                        (None, Some(_)) => "Connection lost. This terminal is still running on the server; \
                                            Reconnect to go back to it."
                            .into(),
                        (None, None) => "Shell closed".into(),
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
                        if ed.draft_saved.take().is_some() {
                            shared.drafts.remove(self.profile.id, &ed.session.path, ed.access.is_sudo());
                        }
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
                            match (&c.outcome, &c.note) {
                                (CheckOutcome::Passed { .. }, None) => {
                                    msg.push_str(&format!(" {} check passed.", c.rule))
                                }
                                (CheckOutcome::Passed { .. }, Some(n)) => {
                                    msg.push_str(&format!(" {} check passed. {n}", c.rule))
                                }
                                (CheckOutcome::Unavailable { .. }, None) => {
                                    warn = true;
                                    msg.push_str(&format!(" Not checked: the {} check tool is not installed.", c.rule));
                                }
                                (CheckOutcome::Unavailable { .. }, Some(n)) => {
                                    warn = true;
                                    msg.push_str(&format!(" Not checked: {n}"));
                                }
                            }
                            match &c.reload {
                                Some((true, out)) if !out.trim().is_empty() => {
                                    msg.push_str(&format!(" Reloaded. {}", out.trim()))
                                }
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
            SessionMsg::TransferDone { id, result } => {
                self.browser.transfers.finish_external(id, result);
                if let Some(l) = &mut self.local {
                    l.refresh();
                }
                self.refresh(shared);
            }
            SessionMsg::JobPlanned { id, job, replaced, place } => match job {
                // Everything it would replace is identical: nothing to ask.
                Ok(mut job) if !replaced.is_empty() && replaced.iter().all(|r| r.same == Some(true)) => {
                    let same = crate::queue::SAME.trim_matches(['(', ')']);
                    let drop: Vec<(usize, String)> = replaced.iter().map(|r| (r.index, same.to_string())).collect();
                    malasdoh_core::replace::leave_out(&mut job, &drop);
                    self.browser.transfers.planned(id, Ok(job));
                    self.run_job(id, shared);
                }
                Ok(job) if !replaced.is_empty() => {
                    let backup = job.way == transfer::Way::Up && shared.transfer.read().unwrap().backup;
                    self.browser.transfers.reviewing(id);
                    self.reviews.push(crate::replace_ui::Review::new(id, job, place, replaced, backup));
                }
                job => {
                    let ok = job.is_ok();
                    self.browser.transfers.planned(id, job);
                    if ok {
                        self.run_job(id, shared);
                    }
                }
            },
            SessionMsg::JobReady { id, job, saved, too_big, failed } => {
                let n = saved.len();
                if !saved.is_empty() {
                    self.upload_backups.insert(id, saved);
                }
                self.browser.transfers.planned(id, Ok(job));
                if failed > 0 {
                    self.notify(
                        format!(
                            "{failed} file(s) were left as they are: their copy on the server could not be backed \
                             up first (Details in the transfer list)."
                        ),
                        true,
                    );
                } else if too_big > 0 {
                    self.notify(
                        format!(
                            "Backed up {n} file(s) before replacing them; {too_big} over {} replaced without a backup.",
                            sftp::human_size(malasdoh_core::replace::BACKUP_MAX)
                        ),
                        false,
                    );
                }
                self.run_job(id, shared);
            }
            SessionMsg::JobRan { id, job, ended } => {
                self.journal_uploads(id, &job, &ended, shared);
                let lost = self.state != ConnState::Connected || self.conn.as_ref().is_none_or(|c| c.is_closed());
                self.browser.transfers.ran(id, job, ended, lost);
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
            SessionMsg::SudoReady { password, warning, then } => match password {
                Ok(password) => {
                    if let Some(w) = warning {
                        self.notify(w, true);
                    }
                    self.sudo = Some(Access::Sudo { password });
                    match then {
                        AfterSudo::Open(path) => self.open_file(path, true, shared),
                        AfterSudo::Run { cmd, what, done } => self.run_op(cmd, what, done, true, shared),
                        AfterSudo::Follow(path) => self.follow(path, true, shared),
                        AfterSudo::Find(cmd) => self.run_find(cmd, true, shared),
                        AfterSudo::Restore(d) => self.restore_draft(*d, shared, settings),
                        AfterSudo::RootBrowse => self.toggle_root(shared),
                        AfterSudo::PatchCheck { text, base, strip } => {
                            self.patch_check(text, base, strip, true, shared)
                        }
                        AfterSudo::PatchApply => self.patch_apply(shared, settings),
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
                        if matches!(then, AfterSudo::RootBrowse) {
                            self.browser.root_pending = false;
                        }
                        if matches!(then, AfterSudo::PatchCheck { .. } | AfterSudo::PatchApply) {
                            self.patch.busy = false;
                        }
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

    /// A journal entry naming this connection and `path`, to be filled in.
    pub(crate) fn journal_entry(profile: &Profile, path: &str, sudo: bool) -> journal::Entry {
        journal::Entry {
            time: 0,
            connection: profile.display_name(),
            user: profile.user.clone(),
            host: profile.host.clone(),
            port: profile.port,
            path: path.to_string(),
            outcome: journal::Outcome::Saved,
            sudo,
            added: 0,
            removed: 0,
            check: None,
            local_backup: None,
            remote_backup: None,
            patch: String::new(),
            truncated: false,
        }
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
        let entry = Self::journal_entry(&self.profile, &session.path, ed.access.is_sudo());
        shared.rt.spawn(async move {
            let saved = io.save(&mut session, &text, &opts).await;
            // Record what happened to the server's file, then report.
            if let Some(r) = journal::entry_for(entry, &saved, &session.base_text, &text)
                && let Err(e) = journal.append(&r)
            {
                tracing::warn!("could not write the change journal: {e:#}");
                let msg = format!(
                    "The change journal could not be written ({e:#}); the save itself is reported in the editor."
                );
                events.session(id, SessionMsg::OpDone { result: Err(msg) });
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

    /// Check sudo works, with a saved password or by asking if it needs one.
    fn ensure_sudo(&mut self, then: AfterSudo, shared: &Shared) {
        let Some(conn) = self.conn.clone() else { return };
        let (events, id, profile, secrets) =
            (shared.events.clone(), self.id, self.profile.clone(), shared.secrets.clone());
        shared.rt.spawn(async move {
            let (password, warning) = match crate::sudo::obtain(&conn, &profile, &secrets, &events, id).await {
                Ok(o) => (Ok(o.password), o.warning),
                Err(e) => (Err(e), None),
            };
            events.session(id, SessionMsg::SudoReady { password, warning, then });
        });
    }

    /// Download into a local folder; files it would replace are reviewed
    /// first.
    pub fn download_to(&mut self, paths: Vec<String>, dir: PathBuf, shared: &Shared) {
        self.start_download(paths, dir, shared);
    }

    /// Queue an upload of `files` (files or folders) into `dir`. Once the
    /// files are known, those already on the server are reviewed first.
    fn start_upload(&mut self, files: Vec<PathBuf>, dir: String, shared: &Shared) {
        let Some(fs) = self.fs.clone() else { return };
        if files.is_empty() {
            return;
        }
        let what = match files.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            many => format!("{} items", many.len()),
        };
        let id = self.browser.transfers.add(format!("⬆ {what} to {dir}"), None);
        let opts = shared.transfer.read().unwrap().clone();
        let (events, sid) = (shared.events.clone(), self.id);
        let checker = self.conn.clone().map(|c| transfer::Checker::new(c, self.checker_sudo()));
        let place = format!("{dir} on {}", self.profile.display_name());
        shared.rt.spawn(async move {
            let planned = {
                let dir = dir.clone();
                tokio::task::spawn_blocking(move || transfer::plan_upload(&files, &dir, &opts)).await
            };
            let job = match planned {
                Ok(j) => j.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            let replaced = match &job {
                Ok(j) => malasdoh_core::replace::find(&fs, j, &dir, checker.as_ref()).await,
                Err(_) => Vec::new(),
            };
            events.session(sid, SessionMsg::JobPlanned { id, job, replaced, place });
        });
    }

    /// Queue a download of `paths` (files or folders) into `dir`.
    fn start_download(&mut self, paths: Vec<String>, dir: PathBuf, shared: &Shared) {
        let Some(fs) = self.fs.clone() else { return };
        if paths.is_empty() {
            return;
        }
        let what = match paths.as_slice() {
            [one] => sftp::file_name(one).to_string(),
            many => format!("{} items", many.len()),
        };
        let id = self.browser.transfers.add(format!("⬇ {what} to {}", dir.display()), None);
        let opts = shared.transfer.read().unwrap().clone();
        let (events, sid) = (shared.events.clone(), self.id);
        let checker = self.conn.clone().map(|c| transfer::Checker::new(c, self.checker_sudo()));
        let place = dir.display().to_string();
        shared.rt.spawn(async move {
            let job = transfer::plan_download(&fs, &paths, &dir, &opts).await.map_err(|e| format!("{e:#}"));
            let replaced = match &job {
                Ok(j) => malasdoh_core::replace::find(&fs, j, &dir.to_string_lossy(), checker.as_ref()).await,
                Err(_) => Vec::new(),
            };
            events.session(sid, SessionMsg::JobPlanned { id, job, replaced, place });
        });
    }

    /// The limit on files copied at once, shared by this connection's transfers.
    fn transfer_limit(&mut self, parallel: usize) -> Arc<tokio::sync::Semaphore> {
        let n = parallel.clamp(1, 16);
        match &self.transfer_limit {
            Some((have, sem)) if *have == n => sem.clone(),
            _ => {
                let sem = Arc::new(tokio::sync::Semaphore::new(n));
                self.transfer_limit = Some((n, sem.clone()));
                sem
            }
        }
    }

    /// Start (or continue) a queued transfer.
    fn run_job(&mut self, id: u64, shared: &Shared) {
        let (Some(fs), Some(conn)) = (self.fs.clone(), self.conn.clone()) else {
            if let Some(t) = self.browser.transfers.get_mut(id) {
                t.state = crate::queue::State::Interrupted;
            }
            return;
        };
        let opts = shared.transfer.read().unwrap().clone();
        let limit = self.transfer_limit(opts.parallel);
        let Some((progress, mut job)) = self.browser.transfers.started(id) else { return };
        let sudo = self.checker_sudo();
        let (events, sid) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let checker = transfer::Checker::new(conn, sudo);
            let ended = transfer::run(&fs, &mut job, &opts, &progress, limit, Some(&checker))
                .await
                .map_err(|e| format!("{e:#}"));
            events.session(sid, SessionMsg::JobRan { id, job, ended });
        });
    }

    /// Checksums are computed as root while browsing as root, so the files
    /// can be read.
    pub fn checker_sudo(&self) -> Option<Option<String>> {
        match (&self.sudo, self.browser.root) {
            (Some(Access::Sudo { password }), true) => Some(password.clone()),
            _ => None,
        }
    }

    fn queue_control(&mut self, c: crate::queue::Control, shared: &Shared) {
        use crate::queue::{Control, State};
        match c {
            Control::Resume(id) => self.run_job(id, shared),
            Control::Pause(id) => {
                if let Some(t) = self.browser.transfers.get_mut(id) {
                    t.progress.pause.store(true, Ordering::Relaxed);
                    t.progress.cancel.store(true, Ordering::Relaxed);
                    t.state = State::Stopping;
                }
            }
            Control::Cancel(id) => {
                let fs = self.fs.clone();
                let Some(t) = self.browser.transfers.get_mut(id) else { return };
                if t.is_active() {
                    t.progress.pause.store(false, Ordering::Relaxed);
                    t.progress.cancel.store(true, Ordering::Relaxed);
                    t.state = State::Stopping;
                    return;
                }
                // Paused or waiting: remove what it left behind.
                let Some(mut job) = t.job.take() else { return };
                t.state = State::Stopping;
                let (events, sid) = (shared.events.clone(), self.id);
                shared.rt.spawn(async move {
                    if let Some(fs) = fs {
                        transfer::discard_partials(&fs, &mut job).await;
                    } else if job.way == transfer::Way::Down {
                        // Local partials can go even while disconnected.
                        for f in &job.files {
                            let _ = std::fs::remove_file(format!("{}{}", f.local.display(), transfer::PART));
                        }
                    }
                    events.session(sid, SessionMsg::JobRan { id, job, ended: Ok(Ended::Cancelled) });
                });
            }
        }
    }

    pub fn connection(&self) -> Option<Arc<Connection>> {
        self.conn.clone()
    }

    pub fn fs(&self) -> Option<RemoteFs> {
        self.fs.clone()
    }

    /// A transfer run by someone else (a copy between servers), shown in this
    /// session's list; finish it with `SessionMsg::TransferDone { id, .. }`.
    pub fn add_transfer(&mut self, label: String) -> (u64, Arc<Progress>) {
        self.browser.transfers.add_external(label)
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

    pub fn browser_action(&mut self, action: browser::Action, shared: &Shared) {
        use browser::Action as A;
        match action {
            A::Navigate(p) => self.navigate(p, shared),
            A::Refresh => self.refresh(shared),
            A::OpenSync => self.sync.show_for(&self.browser.path.clone()),
            A::Edit { path, sudo } => self.open_file(path, sudo, shared),
            A::ExternalEdit(path) => {
                // As root while browsing as root, so checks also run as root.
                let access = if self.browser.root { self.sudo.clone().unwrap_or(Access::Sftp) } else { Access::Sftp };
                let ext_access = access.clone();
                let Some(io) = self.io(access) else { return };
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
                            events.session(id, SessionMsg::ExternalReady { session, local, access: ext_access })
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
            A::EditMany(path) => self.edit_many = Some((path, self.browser.root)),
            A::Chown { paths, owner, recursive, sudo } => match commands::chown(&paths, &owner, recursive) {
                Ok(cmd) => self.run_op(cmd, "change the owner", format!("Owner set to {owner}"), sudo, shared),
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            A::Symlink { target, link } => match commands::symlink(&target, &link) {
                Ok(cmd) => self.run_op(cmd, "create the link", format!("Created {link} -> {target}"), false, shared),
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
            A::Queue(c) => self.queue_control(c, shared),
            A::ToggleRoot => self.toggle_root(shared),
            A::OpenPatch => {
                let (dir, root) = (self.browser.path.clone(), self.browser.root);
                self.patch.show(&dir, root);
            }
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
                    None => match crate::dialogs::FileDialog::new()
                        .id("download-to")
                        .set_title("Download to folder")
                        .pick_folder()
                    {
                        Some(d) => d,
                        None => return,
                    },
                };
                self.download_to(paths, dir, shared);
            }
            A::UploadPick => {
                if let Some(files) =
                    crate::dialogs::FileDialog::new().id(UPLOAD_ID).set_title("Upload files").pick_files()
                {
                    self.browser_action(A::Upload(files), shared);
                }
            }
            A::Upload(files) => {
                let dir = self.browser.path.clone();
                self.start_upload(files, dir, shared);
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

    /// Read every file the patch touches and try it in memory.
    fn patch_check(&mut self, text: String, base: String, strip: usize, sudo: bool, shared: &Shared) {
        use crate::patch_ui::{FileCheck, Status, remote_path};
        use malasdoh_core::patch::{self, Applied};
        let then = AfterSudo::PatchCheck { text: text.clone(), base: base.clone(), strip };
        let Some(access) = self.access_for(sudo, then, shared) else { return };
        let Some(io) = self.io(access) else {
            self.patch.busy = false;
            return self.notify("Not connected", true);
        };
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = async {
                let files = patch::parse(&text)?;
                let mut out = Vec::new();
                for fp in files {
                    let remote = remote_path(&base, fp.path(), strip);
                    let status = if fp.is_deletion() {
                        Status::Skipped("deletes the file; not done here (delete it in the browser)".into())
                    } else {
                        match io.read_bytes(&remote).await {
                            Err(e) => Status::Failed(format!("{e:#}")),
                            Ok(None) if fp.is_new_file() => match patch::apply("", &fp) {
                                Ok(Applied::Changed(t)) => Status::New { text: t },
                                Ok(Applied::AlreadyApplied) => Status::Already,
                                Err(e) => Status::Failed(e.to_string()),
                            },
                            Ok(None) => Status::Failed("does not exist".into()),
                            // A new file that is already there with the same text: done.
                            Ok(Some(have)) if fp.is_new_file() => match patch::apply("", &fp) {
                                Ok(Applied::Changed(t)) if text::decode(&have).is_ok_and(|(h, _)| h == t) => {
                                    Status::Already
                                }
                                _ => Status::Failed("should be new, but already exists".into()),
                            },
                            Ok(Some(_)) => match io.open(&remote).await {
                                Err(e) => Status::Failed(format!("{e:#}")),
                                Ok(session) => match patch::apply(&session.base_text, &fp) {
                                    Ok(Applied::Changed(new_text)) => {
                                        let (added, removed) = fp.counts();
                                        Status::Ready { session: Box::new(session), new_text, added, removed }
                                    }
                                    Ok(Applied::AlreadyApplied) => Status::Already,
                                    Err(e) => Status::Failed(e.to_string()),
                                },
                            },
                        }
                    };
                    out.push(FileCheck { patch_path: fp.path().to_string(), remote, status });
                }
                anyhow::Ok(out)
            }
            .await
            .map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::PatchChecked(result));
        });
    }

    /// Save the checked files one by one, stopping at the first failure.
    fn patch_apply(&mut self, shared: &Shared, settings: &Settings) {
        use crate::patch_ui::Status;
        let Some(Ok(files)) = &self.patch.checked else { return };
        let sudo = self.patch.sudo;
        let jobs: Vec<(String, Option<EditSession>, String)> = files
            .iter()
            .filter_map(|f| match &f.status {
                Status::Ready { session, new_text, .. } => {
                    Some((f.remote.clone(), Some((**session).clone()), new_text.clone()))
                }
                Status::New { text } => Some((f.remote.clone(), None, text.clone())),
                _ => None,
            })
            .collect();
        let Some(access) = self.access_for(sudo, AfterSudo::PatchApply, shared) else { return };
        let Some(io) = self.io(access) else {
            self.patch.busy = false;
            return self.notify("Not connected", true);
        };
        let rules = settings.validation_rules.clone();
        let backup_root = edit::host_backup_dir(&shared.paths.backups_dir(), &self.profile.host, self.profile.port);
        let remote_backup = self.profile.remote_backup;
        let journal = Journal::new(shared.paths.journal_file());
        let entries: Vec<journal::Entry> =
            jobs.iter().map(|(p, _, _)| Self::journal_entry(&self.profile, p, sudo)).collect();
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let mut results = Vec::new();
            for ((path, session, text), entry) in jobs.into_iter().zip(entries) {
                let outcome = match session {
                    Some(mut session) => {
                        let opts = SaveOptions {
                            local_backup_dir: Some(backup_root.clone()),
                            remote_backup,
                            force: false,
                            validation: find_rule(&rules, &path).cloned(),
                        };
                        let saved = io.save(&mut session, &text, &opts).await;
                        if let Some(r) = journal::entry_for(entry, &saved, &session.base_text, &text) {
                            let _ = journal.append(&r);
                        }
                        saved.map(|rep| match rep.check {
                            Some(c) => format!("saved, {} check done", c.rule),
                            None => "saved".to_string(),
                        })
                    }
                    None => {
                        let bytes = text.as_bytes();
                        let made = io.create_new(&path, bytes, 0o644).await;
                        if made.is_ok() {
                            let e = journal::Entry { time: journal::now(), ..entry }.with_change("", &text);
                            let _ = journal.append(&e);
                        }
                        made.map(|_| "created".to_string())
                    }
                };
                let failed = outcome.is_err();
                results.push((path, outcome.map_err(|e| format!("{e:#}"))));
                if failed {
                    break;
                }
            }
            events.session(id, SessionMsg::PatchApplied(results));
        });
    }

    /// Switch the browser to root (SFTP through sudo, set up first if
    /// needed) or back to your own user.
    pub fn toggle_root(&mut self, shared: &Shared) {
        if self.browser.root {
            if let Some(fs) = self.fs_user.take() {
                self.fs = Some(fs);
            }
            self.browser.root = false;
            self.notify("Back to your own user.", false);
            return self.refresh(shared);
        }
        let Some(access) = self.access_for(true, AfterSudo::RootBrowse, shared) else {
            self.browser.root_pending = true;
            return;
        };
        let (Some(conn), Access::Sudo { password }) = (self.conn.clone(), access) else { return };
        self.browser.root_pending = true;
        let (events, id) = (shared.events.clone(), self.id);
        shared.rt.spawn(async move {
            let result = conn.open_sftp_sudo(password.as_deref()).await.map_err(|e| format!("{e:#}"));
            events.session(id, SessionMsg::RootFs(result));
        });
    }

    /// Write a draft of every modified file whose text changed since the
    /// last one, and remove drafts of files that are no longer modified.
    pub fn autosave_drafts(&mut self, shared: &Shared) {
        use std::hash::{Hash, Hasher};
        let profile = self.profile.id;
        for ed in &mut self.editors {
            if ed.is_modified() {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                ed.text.hash(&mut h);
                let h = h.finish();
                if ed.draft_saved != Some(h) {
                    let d = Draft {
                        profile,
                        sudo: ed.access.is_sudo(),
                        session: ed.session.clone(),
                        text: ed.text.clone(),
                        saved: journal::now(),
                    };
                    match shared.drafts.save(&d) {
                        Ok(()) => ed.draft_saved = Some(h),
                        Err(e) => tracing::warn!("{e:#}"),
                    }
                }
            } else if ed.draft_saved.take().is_some() {
                shared.drafts.remove(profile, &ed.session.path, ed.access.is_sudo());
            }
        }
    }

    /// Throw away the unsaved edits of every open file (closing on purpose).
    pub fn discard_drafts(&mut self, shared: &Shared) {
        for ed in &mut self.editors {
            if ed.draft_saved.take().is_some() || ed.is_modified() {
                shared.drafts.remove(self.profile.id, &ed.session.path, ed.access.is_sudo());
            }
        }
    }

    /// Reopen a draft: the editor gets the version it was based on, so a
    /// save notices if the server's copy changed and offers the merge.
    pub fn restore_draft(&mut self, d: Draft, shared: &Shared, settings: &Settings) {
        let access = if d.sudo {
            match &self.sudo {
                Some(a) => a.clone(),
                None => return self.ensure_sudo(AfterSudo::Restore(Box::new(d)), shared),
            }
        } else {
            Access::Sftp
        };
        let id = self.next();
        let check = find_rule(&settings.validation_rules, &d.session.path).cloned();
        let mut ed = Editor::new(id, d.session, access, check);
        ed.text = d.text;
        ed.message = Some((
            format!(
                "Restored unsaved edits from {}. Saving checks the server's copy first.",
                journal::format_time(d.saved)
            ),
            false,
        ));
        self.editors.push(ed);
        self.active = Some(Pane::Editor(id));
    }

    pub fn close_editor(&mut self, id: u64) {
        self.editors.retain(|e| e.id != id);
        if self.active == Some(Pane::Editor(id)) {
            self.active =
                self.terms.first().map(|t| Pane::Term(t.id)).or(self.editors.first().map(|e| Pane::Editor(e.id)));
        }
    }

    pub fn close_terminal(&mut self, id: u64) {
        if let Some(name) = self.terms.iter().find(|t| t.id == id && t.is_live()).and_then(|t| t.lasting.clone()) {
            self.notify(
                format!("{name} keeps running on the server. Reattach it from the ⏷ next to + Terminal, or type exit in it to end it."),
                false,
            );
        }
        self.terms.retain(|t| t.id != id);
        if self.active == Some(Pane::Term(id)) {
            self.active =
                self.terms.first().map(|t| Pane::Term(t.id)).or(self.editors.first().map(|e| Pane::Editor(e.id)));
        }
    }

    /// Give the keyboard to the pane shown (after switching to it).
    pub fn focus_active(&mut self) {
        match self.active {
            Some(Pane::Term(id)) => {
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == id) {
                    t.term.take_focus = true;
                }
            }
            Some(Pane::Editor(id)) => {
                if let Some(e) = self.editors.iter_mut().find(|e| e.id == id) {
                    e.focus();
                }
            }
            _ => {}
        }
    }

    /// The next (or previous, with `step` -1) pane in tab order.
    pub fn cycle_pane(&mut self, step: isize) {
        let panes: Vec<Pane> = self.panes().into_iter().map(|(p, _)| p).collect();
        if panes.is_empty() {
            return;
        }
        let here = self.active.and_then(|a| panes.iter().position(|p| *p == a)).unwrap_or(0) as isize;
        let to = panes[(here + step).rem_euclid(panes.len() as isize) as usize];
        if self.split == Some(to) {
            self.split = self.active;
        }
        self.active = Some(to);
        self.focus_active();
    }

    /// Close the pane shown: a terminal, a followed log, or a file without
    /// unsaved changes.
    pub fn close_active(&mut self) {
        match self.active {
            Some(Pane::Term(id)) => self.close_terminal(id),
            Some(Pane::Log(id)) => self.close_log(id),
            Some(Pane::Editor(id)) => {
                if self.editors.iter().any(|e| e.id == id && e.is_modified()) {
                    self.notify("This file has unsaved changes. Save or reload it before closing.".to_string(), true);
                } else {
                    self.close_editor(id);
                }
            }
            None => {}
        }
        self.focus_active();
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

    fn drafts_ui(&mut self, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        if self.found_drafts.is_empty() {
            return;
        }
        let mut restore = None;
        let mut discard = None;
        egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
            ui.label(RichText::new("Unsaved edits from last time").strong());
            for (i, d) in self.found_drafts.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(&d.session.path);
                    if d.sudo {
                        ui.label(RichText::new("as root").weak());
                    }
                    ui.label(RichText::new(journal::format_time(d.saved)).weak());
                    if ui.button("Restore").clicked() {
                        restore = Some(i);
                    }
                    if ui.button("Discard").on_hover_text("Delete these edits for good").clicked() {
                        discard = Some(i);
                    }
                });
            }
        });
        if let Some(i) = restore {
            let d = self.found_drafts.remove(i);
            self.restore_draft(d, shared, settings);
        } else if let Some(i) = discard {
            let d = self.found_drafts.remove(i);
            shared.drafts.remove(d.profile, &d.session.path, d.sudo);
        }
    }

    pub fn pane_ui(&mut self, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        self.drafts_ui(ui, shared, settings);
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
        self.dialogs_ui(ui, shared, settings);
    }

    fn render_pane(&mut self, pane: Option<Pane>, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        match pane {
            Some(Pane::Term(id)) => {
                let Some(t) = self.terms.iter_mut().find(|t| t.id == id) else { return };
                t.term.set_font_size(settings.terminal_font_size);
                let opts = crate::terminal::TermOptions {
                    theme: crate::look::terminal_theme(ui.ctx()),
                    copy_on_select: settings.copy_on_select,
                    confirm_paste: settings.confirm_paste,
                    touch: crate::look::is_touch(ui.ctx()),
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

    fn dialogs_ui(&mut self, ui: &mut egui::Ui, shared: &Shared, settings: &Settings) {
        let checker = self.conn.clone().map(|c| (c, self.checker_sudo()));
        let backups = shared
            .transfer
            .read()
            .unwrap()
            .backup
            .then(|| edit::host_backup_dir(&shared.paths.backups_dir(), &self.profile.host, self.profile.port));
        self.sync.ui(ui.ctx(), self.fs.as_ref(), checker, backups, shared, self.id);
        if let Some((path, backup)) = self.backups_ui(ui.ctx()) {
            self.restore_backup(path, backup, shared);
        }
        self.reviews_ui(ui.ctx(), shared);
        match self.patch.ui(ui.ctx(), self.id) {
            Some(crate::patch_ui::PatchAction::Check { text, base, strip, sudo }) => {
                self.patch_check(text, base, strip, sudo, shared)
            }
            Some(crate::patch_ui::PatchAction::Apply) => self.patch_apply(shared, settings),
            None => {}
        }
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

    /// "Replace files?" for the first transfer waiting for it.
    fn reviews_ui(&mut self, ctx: &egui::Context, shared: &Shared) {
        use crate::replace_ui::ReviewAction;
        let Some(r) = self.reviews.first_mut() else { return };
        let Some(action) = r.ui(ctx, egui::Id::new(("replace-files", self.id, r.id))) else { return };
        match action {
            ReviewAction::Load(i, slot) => {
                let Some(fs) = self.fs.clone() else {
                    *slot.lock().unwrap() = Some(Err("not connected".into()));
                    return;
                };
                let (way, file) = (r.job.way, r.job.files[r.replaced[i].index].clone());
                let ctx = ctx.clone();
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
            ReviewAction::Cancel => {
                let r = self.reviews.remove(0);
                self.browser.transfers.dropped(r.id);
            }
            ReviewAction::Go => {
                let r = self.reviews.remove(0);
                let (keep, drop) = r.choice();
                let (id, mut job) = (r.id, r.job);
                let fs = self.fs.clone();
                match fs {
                    Some(fs) if r.backup && !keep.is_empty() => {
                        let host_dir =
                            edit::host_backup_dir(&shared.paths.backups_dir(), &self.profile.host, self.profile.port);
                        let (events, sid) = (shared.events.clone(), self.id);
                        shared.rt.spawn(async move {
                            let b = malasdoh_core::replace::back_up(&fs, &job, &keep, &host_dir).await;
                            let mut drop = drop;
                            for (i, e) in &b.failed {
                                drop.push((
                                    *i,
                                    format!(
                                        "kept as it was: its copy on the server could not be backed up first ({e})"
                                    ),
                                ));
                            }
                            let saved = b
                                .saved
                                .iter()
                                .map(|(i, p)| (job.files[*i].remote.clone(), job.files[*i].local.clone(), p.clone()))
                                .collect();
                            malasdoh_core::replace::leave_out(&mut job, &drop);
                            let (too_big, failed) = (b.too_big.len(), b.failed.len());
                            events.session(sid, SessionMsg::JobReady { id, job, saved, too_big, failed });
                        });
                    }
                    _ => {
                        malasdoh_core::replace::leave_out(&mut job, &drop);
                        self.browser.transfers.planned(id, Ok(job));
                        self.run_job(id, shared);
                    }
                }
            }
        }
    }

    /// Server files a folder sync replaced (and backed up first) go into
    /// the change journal, like uploads.
    fn journal_synced(
        &self,
        report: &malasdoh_core::sync::SyncReport,
        local: &std::path::Path,
        remote: &str,
        shared: &Shared,
    ) {
        let failed: std::collections::HashSet<&str> = report.failed.iter().map(|(r, _)| r.as_str()).collect();
        let bases: Vec<_> = report
            .backups
            .iter()
            .filter(|(rel, _, upload)| *upload && !failed.contains(rel.as_str()))
            .map(|(rel, backup, _)| {
                let path = sftp::join(remote, rel);
                let local = rel.split('/').fold(local.to_path_buf(), |p, c| p.join(c));
                (Self::journal_entry(&self.profile, &path, self.browser.root), local, backup.clone())
            })
            .collect();
        if bases.is_empty() {
            return;
        }
        let journal = Journal::new(shared.paths.journal_file());
        shared.rt.spawn_blocking(move || {
            for (base, local, backup) in bases {
                if let Err(e) = journal.append(&malasdoh_core::replace::journal_entry(base, &backup, &local)) {
                    tracing::warn!("could not write the change journal: {e:#}");
                }
            }
        });
    }

    /// Replaced server files that were backed up go into the change
    /// journal once their upload is done, with what changed for text.
    fn journal_uploads(&mut self, id: u64, job: &Job, ended: &Result<Ended, String>, shared: &Shared) {
        let Some(list) = self.upload_backups.remove(&id) else { return };
        if matches!(ended, Ok(Ended::Cancelled)) {
            return;
        }
        let done: std::collections::HashSet<&str> =
            job.files.iter().filter(|f| f.status == transfer::Status::Done).map(|f| f.remote.as_str()).collect();
        let (now, later): (Vec<_>, Vec<_>) = list.into_iter().partition(|(remote, ..)| done.contains(remote.as_str()));
        if !later.is_empty() {
            self.upload_backups.insert(id, later);
        }
        if now.is_empty() {
            return;
        }
        let journal = Journal::new(shared.paths.journal_file());
        let bases: Vec<_> = now
            .into_iter()
            .map(|(remote, local, backup)| {
                (Self::journal_entry(&self.profile, &remote, self.browser.root), local, backup)
            })
            .collect();
        shared.rt.spawn_blocking(move || {
            for (base, local, backup) in bases {
                let entry = malasdoh_core::replace::journal_entry(base, &backup, &local);
                if let Err(e) = journal.append(&entry) {
                    tracing::warn!("could not write the change journal: {e:#}");
                }
            }
        });
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
            if ui.button("Close").clicked() || crate::dlg::escape(ui) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_tmux_names_reach_a_command_line() {
        assert!(is_lasting_name("malasdoh-2d0feb"));
        assert!(!is_lasting_name("malasdoh-"));
        assert!(!is_lasting_name("work"));
        assert!(!is_lasting_name("malasdoh-x;rm -rf ~"));
        assert!(lasting_command("malasdoh-2d0feb").contains("tmux new-session -A -s malasdoh-2d0feb;"));
    }
}
