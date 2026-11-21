//! Tools → SSH keys: keys pasted into MalasDoh (from a password manager,
//! say), making a key, and putting one on a server.

use std::path::PathBuf;

use egui::RichText;
use malasdoh_core::keys::{self, Installed};
use malasdoh_core::profile::ProfileId;
use malasdoh_core::stored_keys::{self, KeyStore, PasteError, StoredKey};

use crate::bridge::{EventKind, SessionId};
use crate::session::{ConnState, Session, Shared};

/// A key a connection can log in with.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyChoice {
    File(PathBuf),
    Stored(uuid::Uuid, String),
}

pub enum KeysMsg {
    Installed { session: SessionId, key: Option<KeyChoice>, result: Result<Installed, String> },
}

/// What the window asks of the app (it owns the key store and the keychain).
pub enum KeysAction {
    /// Log in to this connection with this key from now on.
    UseKey(ProfileId, KeyChoice),
    /// Keep a pasted key: its details, and the private key for the keychain.
    AddStored(StoredKey, String),
    RenameStored(uuid::Uuid, String),
    RemoveStored(uuid::Uuid),
    SetCertificate(uuid::Uuid, Option<String>),
}

/// The "Paste a key" form.
#[derive(Default)]
struct PasteForm {
    name: String,
    text: String,
    passphrase: String,
    /// The key turned out to need its passphrase.
    asks_passphrase: bool,
    certificate: String,
    show: bool,
    error: Option<String>,
}

/// Editing one stored key: renaming, or its certificate.
enum Editing {
    Rename(uuid::Uuid, String),
    Certificate(uuid::Uuid, String, Option<String>),
    Remove(uuid::Uuid),
}

pub struct KeysWindow {
    pub open: bool,
    path: String,
    comment: String,
    pass: String,
    pass2: String,
    generated: Option<String>,
    error: Option<String>,
    /// The key to install, a private key or `.pub` path.
    install_key: String,
    target: Option<SessionId>,
    busy: bool,
    result: Option<(String, bool)>,
    /// After installing: offer to use the key for this connection.
    offer: Option<(ProfileId, String, KeyChoice)>,
    /// The stored key whose public key is in `install_key`.
    install_stored: Option<(uuid::Uuid, String)>,
    paste: Option<PasteForm>,
    editing: Option<Editing>,
    /// What happened to the last stored-key change, from the app.
    pub stored_message: Option<(String, bool)>,
}

impl Default for KeysWindow {
    fn default() -> Self {
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default();
        // The comment only labels the key; say where it was made.
        let comment = match (user.is_empty(), host.is_empty()) {
            (false, false) => format!("{user}@{host}"),
            (false, true) => user,
            (true, false) => format!("malasdoh@{host}"),
            (true, true) => "malasdoh".into(),
        };
        Self {
            open: false,
            path: String::new(),
            comment,
            pass: String::new(),
            pass2: String::new(),
            generated: None,
            error: None,
            install_key: String::new(),
            target: None,
            busy: false,
            result: None,
            offer: None,
            install_stored: None,
            paste: None,
            editing: None,
            stored_message: None,
        }
    }
}

/// `.pub` files in `~/.ssh`, as quick choices.
fn known_keys() -> Vec<PathBuf> {
    let Some(dir) = malasdoh_core::paths::user_ssh_dir() else { return Vec::new() };
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "pub") && !p.to_string_lossy().ends_with("-cert.pub"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl KeysWindow {
    pub fn show(&mut self, session: Option<SessionId>) {
        self.open = true;
        if self.path.is_empty() {
            self.path = keys::default_path().display().to_string();
        }
        if self.install_key.is_empty()
            && let Some(k) = known_keys().into_iter().next()
        {
            self.install_key = k.display().to_string();
        }
        if session.is_some() {
            self.target = session;
        }
    }

    pub fn handle(&mut self, msg: KeysMsg, sessions: &[Session]) {
        match msg {
            KeysMsg::Installed { session, key, result } => {
                self.busy = false;
                let s = sessions.iter().find(|s| s.id == session);
                let name = s.map(|s| s.profile.display_name()).unwrap_or_default();
                self.result = Some(match result {
                    Ok(Installed::Added) => (format!("Added to {name}'s authorized_keys."), false),
                    Ok(Installed::AlreadyThere) => (format!("{name} already has this key."), false),
                    Err(e) => (format!("Could not add the key: {e}"), true),
                });
                // Offer to log in with it from now on (the private half).
                use malasdoh_core::profile::AuthMethod;
                let choice = match key {
                    Some(KeyChoice::File(key)) => {
                        let private =
                            if key.extension().is_some_and(|e| e == "pub") { key.with_extension("") } else { key };
                        private.is_file().then_some(KeyChoice::File(private))
                    }
                    other => other,
                };
                if let (Some(s), Some(choice)) = (s, choice)
                    && self.result.as_ref().is_some_and(|r| !r.1)
                {
                    let already = match (&choice, &s.profile.auth) {
                        (KeyChoice::File(p), AuthMethod::Key { path }) => p.as_os_str() == path.as_str(),
                        (KeyChoice::Stored(id, _), AuthMethod::Stored { id: have, .. }) => id == have,
                        _ => false,
                    };
                    if !already {
                        self.offer = Some((s.profile.id, name, choice));
                    }
                }
            }
        }
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        sessions: &[Session],
        shared: &Shared,
        store: &KeyStore,
    ) -> Vec<KeysAction> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        let mut open = self.open;
        let shown =
            egui::Window::new("SSH keys").open(&mut open).resizable(false).default_width(560.0).show(ctx, |ui| {
                // As tall as the screen allows before it scrolls, so "Put on a
                // server" can be seen filling in the section below.
                let tall = ui.ctx().content_rect().height() - 120.0;
                egui::ScrollArea::vertical().max_height(tall).min_scrolled_height(tall.min(700.0)).show(ui, |ui| {
                    self.stored_ui(ui, store, &mut out);
                    ui.separator();
                    ui.heading("New key");
                    egui::Grid::new("keygen").num_columns(2).show(ui, |ui| {
                        ui.label("Save as");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(360.0));
                            if ui.button("Browse…").clicked()
                                && let Some(p) = crate::dialogs::FileDialog::new()
                                    .id("key-save")
                                    .set_file_name("id_ed25519")
                                    .set_title("Save the new key as")
                                    .save_file()
                            {
                                self.path = p.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Comment");
                        ui.add(egui::TextEdit::singleline(&mut self.comment).desired_width(360.0));
                        ui.end_row();
                        ui.label("Passphrase");
                        ui.add(egui::TextEdit::singleline(&mut self.pass).password(true).desired_width(240.0))
                            .on_hover_text("Recommended. Leave empty for a key that needs none.");
                        ui.end_row();
                        ui.label("Again");
                        ui.add(egui::TextEdit::singleline(&mut self.pass2).password(true).desired_width(240.0));
                        ui.end_row();
                    });
                    let mismatch = self.pass != self.pass2;
                    if mismatch {
                        ui.colored_label(ui.visuals().warn_fg_color, "The passphrases do not match.");
                    }
                    if ui
                        .add_enabled(
                            !mismatch && !self.path.trim().is_empty(),
                            egui::Button::new("Generate Ed25519 key"),
                        )
                        .clicked()
                    {
                        let path = malasdoh_core::paths::expand_tilde(self.path.trim());
                        match keys::generate(&path, self.comment.trim(), &self.pass) {
                            Ok(line) => {
                                self.generated = Some(line);
                                self.error = None;
                                self.install_key = keys::pub_path(&path).display().to_string();
                                self.pass.clear();
                                self.pass2.clear();
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                    if let Some(e) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    if let Some(line) = &self.generated {
                        ui.label("Public key (safe to share):");
                        ui.horizontal(|ui| {
                            // Button first: the line is cut to fit, the button never is.
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(line.clone());
                            }
                            ui.add(egui::Label::new(RichText::new(line).monospace()).truncate()).on_hover_text(line);
                        });
                    }

                    ui.separator();
                    ui.heading("Put a key on a server");
                    ui.label(
                        RichText::new(
                            "Like ssh-copy-id: adds the public key to ~/.ssh/authorized_keys on the server, once.",
                        )
                        .weak(),
                    );
                    let connected: Vec<(SessionId, String)> = sessions
                        .iter()
                        .filter(|s| s.state == ConnState::Connected)
                        .map(|s| (s.id, s.profile.display_name()))
                        .collect();
                    if self.target.is_none_or(|t| !connected.iter().any(|c| c.0 == t)) {
                        self.target = connected.first().map(|c| c.0);
                    }
                    egui::Grid::new("keyinstall").num_columns(2).show(ui, |ui| {
                        ui.label("Key");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.install_key).desired_width(300.0));
                            egui::ComboBox::from_id_salt("known-keys").selected_text("…").width(30.0).show_ui(
                                ui,
                                |ui| {
                                    for k in known_keys() {
                                        let s = k.display().to_string();
                                        if ui.selectable_label(self.install_key == s, &s).clicked() {
                                            self.install_key = s;
                                        }
                                    }
                                },
                            );
                            if (ui.button("Browse…").clicked() || crate::dialogs::ready("key-install"))
                                && let Some(p) = crate::dialogs::FileDialog::new()
                                    .id("key-install")
                                    .set_title("Key to install")
                                    .pick_file()
                            {
                                self.install_key = p.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Server");
                        let name = self
                            .target
                            .and_then(|t| connected.iter().find(|c| c.0 == t))
                            .map(|c| c.1.clone())
                            .unwrap_or_else(|| "Connect to the server first".into());
                        egui::ComboBox::from_id_salt("key-target").selected_text(name).show_ui(ui, |ui| {
                            for (id, n) in &connected {
                                ui.selectable_value(&mut self.target, Some(*id), n);
                            }
                        });
                        ui.end_row();
                    });
                    if self.install_stored.as_ref().is_some_and(|(id, _)| {
                        store.get(*id).is_none_or(|k| k.public.trim() != self.install_key.trim())
                    }) {
                        self.install_stored = None;
                    }
                    let ready = !self.busy && self.target.is_some() && !self.install_key.trim().is_empty();
                    ui.horizontal(|ui| {
                        if ui.add_enabled(ready, egui::Button::new("Add key to server")).clicked() {
                            self.result = None;
                            self.offer = None;
                            let typed = self.install_key.trim();
                            // A public key line itself, or a key file.
                            let is_line = ["ssh-", "ecdsa-", "sk-"].iter().any(|p| typed.starts_with(p));
                            let (line, key) = if is_line {
                                let key = self.install_stored.as_ref().map(|(id, n)| KeyChoice::Stored(*id, n.clone()));
                                (Ok(typed.to_string()), key)
                            } else {
                                let path = malasdoh_core::paths::expand_tilde(typed);
                                (keys::public_line(&path), Some(KeyChoice::File(path)))
                            };
                            match (line, self.target.and_then(|t| sessions.iter().find(|s| s.id == t))) {
                                (Err(e), _) => self.result = Some((format!("{e:#}"), true)),
                                (Ok(line), Some(s)) => match s.connection() {
                                    Some(conn) => {
                                        self.busy = true;
                                        let (events, session) = (shared.events.clone(), s.id);
                                        shared.rt.spawn(async move {
                                            let result =
                                                keys::install(&conn, &line, None).await.map_err(|e| format!("{e:#}"));
                                            events
                                                .send(0, EventKind::Keys(KeysMsg::Installed { session, key, result }));
                                        });
                                    }
                                    None => self.result = Some(("Not connected".into(), true)),
                                },
                                (Ok(_), None) => {}
                            }
                        }
                        if self.busy {
                            ui.spinner();
                        }
                    });
                    if let Some((msg, err)) = &self.result {
                        let c = if *err { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                        ui.colored_label(c, msg);
                    }
                    let mut used = false;
                    if let Some((id, name, key)) = &self.offer {
                        ui.horizontal(|ui| {
                            if ui.button(format!("Log in to {name} with this key from now on")).clicked() {
                                out.push(KeysAction::UseKey(*id, key.clone()));
                                used = true;
                            }
                        });
                    }
                    if used {
                        self.offer = None;
                    }
                });
            });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        if !open {
            self.paste = None;
            self.editing = None;
        }
        out
    }

    /// Keys pasted into MalasDoh, and the form to paste one.
    fn stored_ui(&mut self, ui: &mut egui::Ui, store: &KeyStore, out: &mut Vec<KeysAction>) {
        ui.heading("Stored keys");
        ui.label(
            RichText::new(
                "Private keys pasted here (from a password manager, say) are kept in the system keychain, not in \
                 files. Choose one as a connection's Authentication: Stored key.",
            )
            .weak(),
        );
        if store.keys.is_empty() {
            ui.label(RichText::new("None yet.").weak());
        }
        for k in &store.keys {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&k.name).strong());
                ui.label(RichText::new(k.describe()).weak().small());
            });
            ui.horizontal(|ui| {
                if ui.small_button("Copy public key").clicked() {
                    ui.ctx().copy_text(k.public.clone());
                }
                if ui.small_button("Put on a server").on_hover_text("Fills in the section below").clicked() {
                    self.install_key = k.public.clone();
                    self.install_stored = Some((k.id, k.name.clone()));
                    self.result = None;
                    self.offer = None;
                }
                let cert = if k.certificate.is_some() { "Certificate ✔" } else { "Certificate…" };
                if ui.small_button(cert).on_hover_text("An OpenSSH certificate for this key (optional)").clicked() {
                    self.editing = Some(Editing::Certificate(k.id, k.certificate.clone().unwrap_or_default(), None));
                }
                if ui.small_button("Rename").clicked() {
                    self.editing = Some(Editing::Rename(k.id, k.name.clone()));
                }
                if ui.small_button("Remove…").clicked() {
                    self.editing = Some(Editing::Remove(k.id));
                }
            });
            if k.key_in_file {
                ui.label(
                    RichText::new("Kept in MalasDoh's settings file (this computer has no keychain).").small().weak(),
                );
            }
            let mut done = false;
            match &mut self.editing {
                Some(Editing::Rename(id, name)) if *id == k.id => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(name).desired_width(200.0));
                        if (ui.button("Save").clicked() || crate::dlg::window_enter(ui, false))
                            && !name.trim().is_empty()
                        {
                            out.push(KeysAction::RenameStored(k.id, name.trim().to_string()));
                            done = true;
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::window_escape(ui) {
                            done = true;
                        }
                    });
                }
                Some(Editing::Certificate(id, text, error)) if *id == k.id => {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .font(egui::TextStyle::Monospace)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text("ssh-ed25519-cert-v01@openssh.com AAAA… (the -cert.pub file's contents)"),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Save").on_hover_text("Ctrl+Enter").clicked() || crate::dlg::window_enter(ui, true)
                        {
                            if text.trim().is_empty() {
                                out.push(KeysAction::SetCertificate(k.id, None));
                                done = true;
                            } else {
                                match stored_keys::check_certificate(text, &k.public) {
                                    Ok(c) => {
                                        out.push(KeysAction::SetCertificate(k.id, Some(c)));
                                        done = true;
                                    }
                                    Err(e) => *error = Some(format!("{e:#}")),
                                }
                            }
                        }
                        if ui.button("Cancel").clicked() || crate::dlg::window_escape(ui) {
                            done = true;
                        }
                        ui.label(RichText::new("Empty: no certificate.").weak().small());
                    });
                    if let Some(e) = error {
                        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                    }
                }
                Some(Editing::Remove(id)) if *id == k.id => {
                    ui.horizontal(|ui| {
                        ui.colored_label(ui.visuals().warn_fg_color, "Remove it from this computer?");
                        if ui.button("Remove").clicked() {
                            out.push(KeysAction::RemoveStored(k.id));
                            done = true;
                        }
                        if ui.button("Keep").clicked() || crate::dlg::window_escape(ui) {
                            done = true;
                        }
                    });
                }
                _ => {}
            }
            if done {
                self.editing = None;
            }
            ui.add_space(4.0);
        }
        if let Some((m, err)) = &self.stored_message {
            let c = if *err { ui.visuals().warn_fg_color } else { ui.visuals().text_color() };
            ui.colored_label(c, m);
        }
        let Some(f) = &mut self.paste else {
            if ui.button("Paste a key…").clicked() {
                self.paste = Some(PasteForm::default());
                self.stored_message = None;
            }
            return;
        };
        ui.add_space(4.0);
        ui.label(RichText::new("Paste a key").strong());
        egui::Grid::new("paste-key").num_columns(2).show(ui, |ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("e.g. Proton Pass").desired_width(240.0));
            ui.end_row();
            ui.label("Private key");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut f.text)
                        .font(egui::TextStyle::Monospace)
                        .password(!f.show)
                        .desired_rows(5)
                        .desired_width(400.0)
                        .hint_text("-----BEGIN OPENSSH PRIVATE KEY-----\n…"),
                );
                ui.checkbox(&mut f.show, "Show");
            });
            ui.end_row();
            if f.asks_passphrase {
                ui.label("Passphrase");
                ui.add(egui::TextEdit::singleline(&mut f.passphrase).password(true).desired_width(240.0));
                ui.end_row();
            }
            ui.label("Certificate");
            ui.add(
                egui::TextEdit::singleline(&mut f.certificate)
                    .hint_text("optional: ssh-ed25519-cert-v01@openssh.com AAAA…")
                    .desired_width(400.0),
            );
            ui.end_row();
        });
        let mut close = false;
        ui.horizontal(|ui| {
            let can = !f.name.trim().is_empty() && !f.text.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Keep it")).on_hover_text("Ctrl+Enter").clicked()
                || (can && crate::dlg::window_enter(ui, true))
            {
                let pass = f.asks_passphrase.then_some(f.passphrase.as_str());
                match stored_keys::check_pasted(&f.text, pass) {
                    Ok(p) => {
                        let certificate = if f.certificate.trim().is_empty() {
                            Ok(None)
                        } else {
                            stored_keys::check_certificate(&f.certificate, &p.public).map(Some)
                        };
                        match certificate {
                            Ok(certificate) => {
                                let entry = StoredKey {
                                    id: uuid::Uuid::new_v4(),
                                    name: f.name.trim().to_string(),
                                    public: p.public,
                                    certificate,
                                    key_in_file: false,
                                    private: None,
                                };
                                out.push(KeysAction::AddStored(entry, p.private));
                                close = true;
                            }
                            Err(e) => f.error = Some(format!("Certificate: {e:#}")),
                        }
                    }
                    Err(PasteError::NeedsPassphrase) => {
                        f.asks_passphrase = true;
                        f.error = Some(PasteError::NeedsPassphrase.to_string());
                    }
                    Err(e) => f.error = Some(e.to_string()),
                }
            }
            if ui.button("Cancel").clicked() || crate::dlg::window_escape(ui) {
                close = true;
            }
        });
        if let Some(e) = &f.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.label(
            RichText::new(
                "OpenSSH, PEM or PuTTY (.ppk) keys. A passphrase is asked for once and taken off: the keychain \
                 protects the key instead. The key is not shown again.",
            )
            .weak()
            .small(),
        );
        if close {
            self.paste = None;
        }
    }
}
