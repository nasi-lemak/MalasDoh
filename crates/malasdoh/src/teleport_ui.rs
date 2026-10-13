//! The Teleport window: add a cluster, log in with a password and an
//! authenticator code, find its servers and add them as connections.

use std::collections::BTreeSet;

use egui::RichText;
use malasdoh_core::secrets::SecretKey;
use malasdoh_core::teleport::{self, CertState, Cluster, ClusterId, ClusterInfo, Node, TeleportError, TeleportStore};

use crate::bridge::{EventKind, MULTI_RUN};
use crate::session::Shared;

pub enum TeleportMsg {
    Checked { address: String, pin: Option<String>, result: Result<ClusterInfo, String>, untrusted: Option<String> },
    LoggedIn { cluster: Box<Cluster>, result: Result<CertState, String> },
    Found { cluster: ClusterId, result: Result<Vec<Node>, String> },
    Password { cluster: ClusterId, password: Option<String> },
}

/// What the app has to do (it owns the connections and sessions).
pub enum TeleportAction {
    /// Store this cluster (new, or with a new login) and refresh its trust.
    Save(Box<Cluster>),
    /// Add or update connections for these servers, as `login`.
    AddServers {
        cluster: ClusterId,
        nodes: Vec<Node>,
        login: String,
    },
    LogOut(ClusterId),
    Remove(ClusterId),
}

#[derive(Default)]
struct AddForm {
    address: String,
    user: String,
    pin: Option<String>,
    checked: Option<Result<ClusterInfo, String>>,
    /// The proxy's certificate fingerprint, when this computer does not trust it.
    untrusted: Option<String>,
}

#[derive(Default)]
pub struct TeleportWindow {
    pub open: bool,
    add: Option<AddForm>,
    selected: Option<ClusterId>,
    password: String,
    remember: bool,
    code: String,
    find_code: String,
    busy: bool,
    message: Option<(String, bool)>,
    found: Option<(ClusterId, Vec<(Node, bool)>)>,
    login_choice: String,
    /// Sessions waiting for this login, to connect once it is done.
    pub waiting: Vec<(ClusterId, u64)>,
    loaded_password_for: Option<ClusterId>,
}

fn remaining(state: &CertState) -> String {
    let secs = (state.valid_before - malasdoh_core::journal::now()).max(0);
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 { format!("{h} h {m} min") } else { format!("{m} min") }
}

fn describe(info: &ClusterInfo) -> String {
    let factor = match info.second_factor.as_str() {
        "otp" => "password and authenticator code",
        "on" | "optional" => "password and a second factor",
        "off" => "password only",
        other => other,
    };
    let routing = if info.tls_routing { "everything on one port" } else { "separate SSH port" };
    format!("Cluster {} · Teleport {} · {factor} · {routing}", info.name, info.version)
}

impl TeleportWindow {
    /// Open on `cluster`'s login, to connect `session` once logged in.
    pub fn ask_login(&mut self, cluster: ClusterId, session: u64) {
        self.open = true;
        self.selected = Some(cluster);
        self.add = None;
        if !self.waiting.contains(&(cluster, session)) {
            self.waiting.push((cluster, session));
        }
        self.message = Some(("Your Teleport login has expired or is missing. Log in to continue.".into(), false));
    }

    pub fn handle(&mut self, msg: TeleportMsg, store: &TeleportStore) -> Vec<TeleportAction> {
        let mut actions = Vec::new();
        match msg {
            TeleportMsg::Checked { address, pin, result, untrusted } => {
                self.busy = false;
                if let Some(f) = self.add.as_mut().filter(|f| f.address.trim() == address) {
                    f.pin = pin;
                    f.untrusted = untrusted;
                    f.checked = Some(result);
                }
            }
            TeleportMsg::LoggedIn { cluster, result } => {
                self.busy = false;
                match result {
                    Ok(state) => {
                        self.code.clear();
                        self.message = Some((
                            format!("Logged in for {}. Server logins: {}.", remaining(&state), state.logins.join(", ")),
                            false,
                        ));
                        actions.push(TeleportAction::Save(cluster));
                    }
                    Err(e) => {
                        self.code.clear();
                        self.message = Some((e, true));
                    }
                }
            }
            TeleportMsg::Found { cluster, result } => {
                self.busy = false;
                self.find_code.clear();
                match result {
                    Ok(nodes) => {
                        let known: BTreeSet<&str> = store
                            .get(cluster)
                            .map(|c| c.hosts.iter().map(String::as_str).collect())
                            .unwrap_or_default();
                        let rows = nodes
                            .into_iter()
                            .map(|n| {
                                let new = !known.contains(teleport::host_pattern(&n.id, n.port()).as_str());
                                (n, new)
                            })
                            .collect::<Vec<_>>();
                        self.message = Some((format!("{} server(s) found.", rows.len()), false));
                        self.found = Some((cluster, rows));
                    }
                    Err(e) => self.message = Some((e, true)),
                }
            }
            TeleportMsg::Password { cluster, password } => {
                if self.selected == Some(cluster)
                    && self.password.is_empty()
                    && let Some(p) = password
                {
                    self.password = p;
                    self.remember = true;
                }
            }
        }
        actions
    }

    pub fn ui(&mut self, ctx: &egui::Context, store: &TeleportStore, shared: &Shared) -> Vec<TeleportAction> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        let mut open = true;
        egui::Window::new("Teleport").open(&mut open).default_width(640.0).default_height(520.0).show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "Log in to a Teleport cluster with your password and authenticator code, then use its servers \
                     like any other connection. Teleport's own client (tsh) is not needed.",
                )
                .weak(),
            );
            ui.horizontal_wrapped(|ui| {
                for c in &store.clusters {
                    let state = teleport::cert_state(&c.key_path(&shared.paths.teleport_dir()));
                    let status = match &state {
                        Some(s) if s.is_valid() => format!("✔ {}", remaining(s)),
                        _ => "not logged in".into(),
                    };
                    let label = format!("{} ({}) · {status}", c.label(), c.user);
                    if ui.selectable_label(self.selected == Some(c.id) && self.add.is_none(), label).clicked() {
                        self.selected = Some(c.id);
                        self.add = None;
                        self.message = None;
                        self.found = None;
                    }
                }
                if ui.button("+ Add a cluster").clicked() {
                    self.add = Some(AddForm::default());
                    self.message = None;
                }
            });
            ui.separator();
            if self.add.is_some() {
                if let Some(a) = self.add_ui(ui, shared) {
                    actions.push(a);
                }
            } else if let Some(c) = self.selected.and_then(|id| store.get(id)).cloned() {
                self.cluster_ui(ui, &c, shared, &mut actions);
            } else if store.clusters.is_empty() {
                ui.label("No clusters yet. Add yours with its web address (what you open in the browser).");
            }
            if let Some((m, err)) = &self.message {
                ui.add_space(6.0);
                let color = if *err { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                ui.colored_label(color, m);
            }
            if self.busy {
                ui.spinner();
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
        });
        if !open {
            self.open = false;
            self.password.clear();
            self.code.clear();
            self.find_code.clear();
        }
        actions
    }

    fn add_ui(&mut self, ui: &mut egui::Ui, shared: &Shared) -> Option<TeleportAction> {
        let f = self.add.as_mut()?;
        let mut result = None;
        egui::Grid::new("tp-add").num_columns(2).show(ui, |ui| {
            ui.label("Address");
            if ui
                .add(egui::TextEdit::singleline(&mut f.address).hint_text("teleport.example.com").desired_width(300.0))
                .changed()
            {
                f.checked = None;
                f.pin = None;
                f.untrusted = None;
            }
            ui.end_row();
            ui.label("User");
            ui.add(egui::TextEdit::singleline(&mut f.user).hint_text("your Teleport user name").desired_width(200.0));
            ui.end_row();
        });
        let mut check = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy && !f.address.trim().is_empty(), egui::Button::new("Check")).clicked() {
                check = Some(f.pin.clone());
            }
            if ui.button("Cancel").clicked() {
                result = Some(None);
            }
        });
        if let Some(fp) = &f.untrusted {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "This computer does not trust the proxy's certificate (often self-signed). Its fingerprint is\n{}\n\
                     Trust it only if it matches the proxy's real certificate; on the proxy, see it with\n\
                     openssl x509 -noout -fingerprint -sha256 -in <its certificate file>",
                    teleport::format_fingerprint(fp)
                ),
            );
            if ui.button("Trust this certificate").clicked() {
                check = Some(Some(fp.clone()));
            }
        }
        match &f.checked {
            Some(Ok(info)) => {
                ui.label(describe(info));
                match info.unsupported() {
                    Some(why) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("Not possible here: {why}."));
                    }
                    None => {
                        if ui.add_enabled(!f.user.trim().is_empty(), egui::Button::new("Add cluster")).clicked() {
                            let c = Cluster {
                                address: f.address.trim().to_string(),
                                user: f.user.trim().to_string(),
                                name: info.name.clone(),
                                tls_routing: info.tls_routing,
                                ssh_addr: info.ssh_addr.clone(),
                                pinned_cert: f.pin.clone(),
                                ..Default::default()
                            };
                            self.selected = Some(c.id);
                            result = Some(Some(TeleportAction::Save(Box::new(c))));
                        }
                    }
                }
            }
            Some(Err(e)) if f.untrusted.is_none() => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            _ => {}
        }
        if let Some(pin) = check {
            self.busy = true;
            let address = f.address.trim().to_string();
            let events = shared.events.clone();
            shared.rt.spawn(async move {
                let (result, untrusted) = match teleport::ping(&address, pin.as_deref()).await {
                    Ok(i) => (Ok(i), None),
                    Err(e) => {
                        let fp = match e.downcast_ref::<TeleportError>() {
                            Some(TeleportError::UntrustedCert { fingerprint, .. }) => Some(fingerprint.clone()),
                            _ => None,
                        };
                        (Err(format!("{e:#}")), fp)
                    }
                };
                events.send(MULTI_RUN, EventKind::Teleport(TeleportMsg::Checked { address, pin, result, untrusted }));
            });
        }
        match result {
            Some(a) => {
                self.add = None;
                a
            }
            None => None,
        }
    }

    fn cluster_ui(&mut self, ui: &mut egui::Ui, c: &Cluster, shared: &Shared, actions: &mut Vec<TeleportAction>) {
        let tdir = shared.paths.teleport_dir();
        let state = teleport::cert_state(&c.key_path(&tdir));
        let logged_in = state.as_ref().is_some_and(|s| s.is_valid());
        // A remembered password, filled in once per cluster.
        if self.loaded_password_for != Some(c.id) {
            self.loaded_password_for = Some(c.id);
            self.password.clear();
            self.remember = false;
            let (secrets, events, id) = (shared.secrets.clone(), shared.events.clone(), c.id);
            shared.rt.spawn(async move {
                let password = tokio::task::spawn_blocking(move || secrets.get(&SecretKey::TeleportPassword(id)))
                    .await
                    .ok()
                    .flatten();
                events.send(MULTI_RUN, EventKind::Teleport(TeleportMsg::Password { cluster: id, password }));
            });
        }
        ui.label(RichText::new(format!("{} — {} as {}", c.label(), c.address, c.user)).strong());
        match &state {
            Some(s) if s.is_valid() => {
                ui.label(format!("Logged in for {} more. Server logins: {}.", remaining(s), s.logins.join(", ")));
            }
            Some(_) => {
                ui.colored_label(ui.visuals().warn_fg_color, "The login has expired.");
            }
            None => {
                ui.label("Not logged in.");
            }
        }
        ui.add_space(4.0);
        egui::Grid::new("tp-login").num_columns(2).show(ui, |ui| {
            ui.label("Password");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0));
                ui.checkbox(&mut self.remember, "Remember in the system keychain");
            });
            ui.end_row();
            ui.label("Code");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.code)
                    .hint_text("6 digits from your authenticator")
                    .desired_width(200.0),
            );
            ui.end_row();
            let can = !self.busy && !self.password.is_empty();
            let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.label("");
            let label = if logged_in { "Log in again" } else { "Log in" };
            if ui.add_enabled(can, egui::Button::new(label)).clicked() || (go && can) {
                self.busy = true;
                self.message = None;
                let mut cluster = c.clone();
                let (password, code, remember) = (self.password.clone(), self.code.clone(), self.remember);
                let (events, secrets, tdir) = (shared.events.clone(), shared.secrets.clone(), tdir.clone());
                shared.rt.spawn(async move {
                    let result =
                        teleport::log_in(&mut cluster, &tdir, &password, &code).await.map_err(|e| format!("{e:#}"));
                    if result.is_ok() {
                        let id = cluster.id;
                        let _ = tokio::task::spawn_blocking(move || {
                            if remember {
                                let _ = secrets.set(&SecretKey::TeleportPassword(id), &password);
                            } else {
                                secrets.delete(&SecretKey::TeleportPassword(id));
                            }
                        })
                        .await;
                    }
                    events.send(
                        MULTI_RUN,
                        EventKind::Teleport(TeleportMsg::LoggedIn { cluster: Box::new(cluster), result }),
                    );
                });
            }
            ui.end_row();
        });
        ui.separator();
        ui.label(RichText::new("Servers").strong());
        ui.label(
            RichText::new(
                "Finding servers signs in to the cluster's web API, which takes its own code (each code works once).",
            )
            .weak(),
        );
        ui.horizontal(|ui| {
            ui.label("Code");
            ui.add(egui::TextEdit::singleline(&mut self.find_code).hint_text("a new code").desired_width(120.0));
            if ui.add_enabled(!self.busy && !self.password.is_empty(), egui::Button::new("Find servers")).clicked() {
                self.busy = true;
                self.message = None;
                let cluster = c.clone();
                let (password, code, events) = (self.password.clone(), self.find_code.clone(), shared.events.clone());
                shared.rt.spawn(async move {
                    let result = teleport::list_nodes(&cluster, &password, &code).await.map_err(|e| format!("{e:#}"));
                    events.send(MULTI_RUN, EventKind::Teleport(TeleportMsg::Found { cluster: cluster.id, result }));
                });
            }
        });
        let mut added = false;
        if let Some((cid, rows)) = self.found.as_mut().filter(|(cid, _)| *cid == c.id) {
            let cid = *cid;
            let logins: BTreeSet<String> = rows.iter().flat_map(|(n, _)| n.logins.iter().cloned()).collect();
            if !logins.contains(&self.login_choice) {
                self.login_choice = if logins.contains("root") {
                    "root".into()
                } else {
                    logins.iter().next().cloned().unwrap_or_default()
                };
            }
            egui::ScrollArea::vertical().id_salt("tp-nodes").max_height(220.0).show(ui, |ui| {
                for (n, on) in rows.iter_mut() {
                    let labels: Vec<String> = n.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    let text = if labels.is_empty() {
                        n.hostname.clone()
                    } else {
                        format!("{}   {}", n.hostname, labels.join(" "))
                    };
                    ui.checkbox(on, text).on_hover_text(format!("logins: {}", n.logins.join(", ")));
                }
            });
            let picked: Vec<Node> = rows.iter().filter(|(_, on)| *on).map(|(n, _)| n.clone()).collect();
            ui.horizontal(|ui| {
                ui.label("Log in as");
                egui::ComboBox::from_id_salt("tp-login-as").selected_text(&self.login_choice).show_ui(ui, |ui| {
                    for l in &logins {
                        ui.selectable_value(&mut self.login_choice, l.clone(), l);
                    }
                });
                let label = format!("Add {} connection(s)", picked.len());
                if ui.add_enabled(!picked.is_empty() && logged_in, egui::Button::new(label)).clicked() {
                    actions.push(TeleportAction::AddServers {
                        cluster: cid,
                        nodes: picked.clone(),
                        login: self.login_choice.clone(),
                    });
                    added = true;
                }
                if !logged_in {
                    ui.label(RichText::new("(log in first)").weak());
                }
            });
        }
        if added {
            self.message =
                Some((format!("Added to the connection list, in the group \"Teleport · {}\".", c.label()), false));
            self.found = None;
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(state.is_some(), egui::Button::new("Log out"))
                .on_hover_text("Delete this computer's key and certificate for the cluster")
                .clicked()
            {
                actions.push(TeleportAction::LogOut(c.id));
                self.message = Some(("Logged out.".into(), false));
            }
            if ui.button("Remove cluster").on_hover_text("Its connections stay, but cannot log in").clicked() {
                actions.push(TeleportAction::Remove(c.id));
                self.selected = None;
            }
        });
    }
}
