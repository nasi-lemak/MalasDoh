//! Tools → SSH keys: make a key, and put it on a server.

use std::path::PathBuf;

use egui::RichText;
use malasdoh_core::keys::{self, Installed};
use malasdoh_core::profile::ProfileId;

use crate::bridge::{EventKind, SessionId};
use crate::session::{ConnState, Session, Shared};

pub enum KeysMsg {
    Installed { session: SessionId, key: PathBuf, result: Result<Installed, String> },
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
    offer: Option<(ProfileId, String, PathBuf)>,
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
                let private = if key.extension().is_some_and(|e| e == "pub") { key.with_extension("") } else { key };
                if let Some(s) = s
                    && private.is_file()
                    && !matches!(&s.profile.auth, malasdoh_core::profile::AuthMethod::Key { path } if *path == private)
                    && self.result.as_ref().is_some_and(|r| !r.1)
                {
                    self.offer = Some((s.profile.id, name, private));
                }
            }
        }
    }

    /// Returns a connection to switch to key login, if the user asked.
    pub fn ui(&mut self, ctx: &egui::Context, sessions: &[Session], shared: &Shared) -> Option<(ProfileId, PathBuf)> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut use_key = None;
        egui::Window::new("SSH keys").open(&mut open).resizable(false).default_width(560.0).show(ctx, |ui| {
            ui.heading("New key");
            egui::Grid::new("keygen").num_columns(2).show(ui, |ui| {
                ui.label("Save as");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(360.0));
                    if ui.button("Browse…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_title("Save the new key as").save_file()
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
                .add_enabled(!mismatch && !self.path.trim().is_empty(), egui::Button::new("Generate Ed25519 key"))
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
                RichText::new("Like ssh-copy-id: adds the public key to ~/.ssh/authorized_keys on the server, once.")
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
                    egui::ComboBox::from_id_salt("known-keys").selected_text("…").width(30.0).show_ui(ui, |ui| {
                        for k in known_keys() {
                            let s = k.display().to_string();
                            if ui.selectable_label(self.install_key == s, &s).clicked() {
                                self.install_key = s;
                            }
                        }
                    });
                    if ui.button("Browse…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_title("Key to install").pick_file()
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
            let ready = !self.busy && self.target.is_some() && !self.install_key.trim().is_empty();
            ui.horizontal(|ui| {
                if ui.add_enabled(ready, egui::Button::new("Add key to server")).clicked() {
                    self.result = None;
                    self.offer = None;
                    let key = malasdoh_core::paths::expand_tilde(self.install_key.trim());
                    match (keys::public_line(&key), self.target.and_then(|t| sessions.iter().find(|s| s.id == t))) {
                        (Err(e), _) => self.result = Some((format!("{e:#}"), true)),
                        (Ok(line), Some(s)) => match s.connection() {
                            Some(conn) => {
                                self.busy = true;
                                let (events, session) = (shared.events.clone(), s.id);
                                shared.rt.spawn(async move {
                                    let result = keys::install(&conn, &line, None).await.map_err(|e| format!("{e:#}"));
                                    events.send(0, EventKind::Keys(KeysMsg::Installed { session, key, result }));
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
            if let Some((id, name, key)) = &self.offer {
                ui.horizontal(|ui| {
                    if ui.button(format!("Log in to {name} with this key from now on")).clicked() {
                        use_key = Some((*id, key.clone()));
                    }
                });
            }
        });
        self.open = open;
        if use_key.is_some() {
            self.offer = None;
        }
        use_key
    }
}
