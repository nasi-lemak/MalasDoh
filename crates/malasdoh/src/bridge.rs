//! How background tasks talk to the UI thread.
//!
//! Tasks run on a tokio runtime and report back through one channel of
//! [`AppEvent`]s, which the UI drains every frame. Questions for the user
//! (host keys, passwords) travel the same way with a oneshot for the answer.

use std::sync::mpsc;

use malasdoh_core::ssh::{HostKeyPrompt, KbdPrompt, Prompter, SecretAnswer, SecretPrompt};
use tokio::sync::oneshot;

pub type SessionId = u64;

/// Events for the "Run on many hosts" window, which is not a session tab.
/// Session ids start at 1.
pub const MULTI_RUN: SessionId = 0;

pub enum Question {
    HostKey(HostKeyPrompt, oneshot::Sender<bool>),
    Secret(SecretPrompt, oneshot::Sender<Option<SecretAnswer>>),
    Keyboard(KbdPrompt, oneshot::Sender<Option<Vec<String>>>),
    /// The sudo password for `host`; `retry` after a wrong one. `note` says
    /// why a saved password was not used.
    Sudo {
        host: String,
        retry: bool,
        note: Option<String>,
        reply: oneshot::Sender<Option<SudoAnswer>>,
    },
}

pub struct SudoAnswer {
    pub password: String,
    /// Save it in the system keychain once sudo accepts it.
    pub remember: bool,
}

pub struct AppEvent {
    pub session: SessionId,
    pub kind: EventKind,
}

pub enum EventKind {
    Status(String),
    Question(Question),
    Session(crate::session::SessionMsg),
    Multi(crate::multirun::MultiMsg),
    MultiEdit(crate::multiedit::EditMsg),
    /// For the compare windows; the session id is not used.
    Compare(crate::compare::CompareMsg),
    /// For the SSH keys window; the session id is not used.
    Keys(crate::keys_ui::KeysMsg),
}

#[derive(Clone)]
pub struct Events {
    tx: mpsc::Sender<AppEvent>,
    ctx: egui::Context,
}

impl Events {
    pub fn new(ctx: egui::Context) -> (Self, mpsc::Receiver<AppEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx, ctx }, rx)
    }

    pub fn send(&self, session: SessionId, kind: EventKind) {
        let _ = self.tx.send(AppEvent { session, kind });
        self.ctx.request_repaint();
    }

    pub fn session(&self, session: SessionId, msg: crate::session::SessionMsg) {
        self.send(session, EventKind::Session(msg));
    }

    pub fn repaint(&self) {
        self.ctx.request_repaint();
    }
}

/// The [`Prompter`] the core sees: every question becomes a dialog.
pub struct UiPrompter {
    pub events: Events,
    pub session: SessionId,
}

impl Prompter for UiPrompter {
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> bool {
        let (tx, rx) = oneshot::channel();
        self.events.send(self.session, EventKind::Question(Question::HostKey(prompt, tx)));
        rx.await.unwrap_or(false)
    }

    async fn ask_secret(&self, prompt: SecretPrompt) -> Option<SecretAnswer> {
        let (tx, rx) = oneshot::channel();
        self.events.send(self.session, EventKind::Question(Question::Secret(prompt, tx)));
        rx.await.ok().flatten()
    }

    async fn keyboard_interactive(&self, prompt: KbdPrompt) -> Option<Vec<String>> {
        let (tx, rx) = oneshot::channel();
        self.events.send(self.session, EventKind::Question(Question::Keyboard(prompt, tx)));
        rx.await.ok().flatten()
    }

    fn status(&self, message: &str) {
        self.events.send(self.session, EventKind::Status(message.to_string()));
    }
}
