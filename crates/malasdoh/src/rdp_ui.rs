//! Remote Desktop connections: the route each one found, the local port
//! forwarded to the desktop, and the Remote Desktop program opened on it.

use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::Instant;

use egui::RichText;
use malasdoh_core::forward::ForwardHandle;
use malasdoh_core::profile::ProfileId;
use malasdoh_core::secrets::{SecretKey, SecretStore};
use malasdoh_core::ssh::Connection;

/// What opening the Remote Desktop program needs from the app.
pub struct Env {
    /// Where the `.rdp` files go.
    pub dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
}

impl Env {
    pub fn new(shared: &crate::session::Shared) -> Self {
        Self { dir: shared.paths.data_dir.join("rdp"), secrets: shared.secrets.clone() }
    }
}

/// The password asked for a program without a login dialog (FreeRDP).
#[derive(Default)]
struct Ask {
    password: String,
    remember: bool,
}

/// What carries a desktop's connections while it is open.
pub enum Carrier {
    /// A forwarded port on an SSH connection.
    Ssh { conn: Arc<Connection>, forward: ForwardHandle },
    /// A local port passed on through the connection's proxy.
    Proxy(tokio::task::JoinHandle<()>),
    /// Nothing: the program connects to the desktop itself.
    Direct,
}

impl Carrier {
    fn close(self, rt: &tokio::runtime::Handle) {
        match self {
            Carrier::Ssh { conn, forward } => {
                drop(forward);
                rt.spawn(async move { conn.disconnect().await });
            }
            Carrier::Proxy(task) => task.abort(),
            Carrier::Direct => {}
        }
    }
}

pub enum RdpMsg {
    Status {
        id: u64,
        text: String,
    },
    /// A route works: `addr` is where the Remote Desktop program connects.
    Ready {
        id: u64,
        addr: String,
        via: Option<ProfileId>,
        via_name: String,
        carrier: Carrier,
    },
    Failed {
        id: u64,
        error: String,
    },
}

enum State {
    Starting(String),
    Open { addr: String, via_name: String, client: Option<&'static str> },
    Closed(String),
}

struct Desktop {
    id: u64,
    profile: ProfileId,
    name: String,
    user: String,
    state: State,
    carrier: Option<Carrier>,
    child: Option<(Child, Instant)>,
    /// Something to know about an open desktop (its program did not start).
    note: Option<String>,
    /// Waiting for the Windows password, before opening FreeRDP.
    asking: Option<Ask>,
    /// The password just typed, for the next launch.
    typed: Option<String>,
}

/// What the window asks of the app.
pub enum RdpAction {
    /// Connect this desktop (again).
    Open(ProfileId),
}

#[derive(Default)]
pub struct DesktopsWindow {
    pub open: bool,
    items: Vec<Desktop>,
    next_id: u64,
}

impl DesktopsWindow {
    /// A new attempt for `profile`; its id names the messages about it.
    pub fn start(&mut self, profile: ProfileId, name: String, user: String) -> u64 {
        self.open = true;
        self.next_id += 1;
        let id = self.next_id;
        self.items.retain(|d| d.profile != profile || matches!(d.state, State::Open { .. }));
        self.items.push(Desktop {
            id,
            profile,
            name,
            user,
            state: State::Starting("Starting…".into()),
            carrier: None,
            child: None,
            note: None,
            asking: None,
            typed: None,
        });
        id
    }

    /// Is this desktop open already? Then its program is opened again.
    pub fn reopen(&mut self, profile: ProfileId, env: &Env) -> bool {
        let Some(d) = self.items.iter_mut().find(|d| d.profile == profile && matches!(d.state, State::Open { .. }))
        else {
            return false;
        };
        self.open = true;
        launch(d, env);
        true
    }

    /// Handles a message; returns the route that worked, for remembering.
    pub fn handle(&mut self, msg: RdpMsg, env: &Env) -> Option<(ProfileId, Option<ProfileId>)> {
        match msg {
            RdpMsg::Status { id, text } => {
                if let Some(d) = self.items.iter_mut().find(|d| d.id == id)
                    && let State::Starting(_) = d.state
                {
                    d.state = State::Starting(text);
                }
                None
            }
            RdpMsg::Failed { id, error } => {
                if let Some(d) = self.items.iter_mut().find(|d| d.id == id) {
                    d.state = State::Closed(error);
                }
                None
            }
            RdpMsg::Ready { id, addr, via, via_name, carrier } => {
                let Some(d) = self.items.iter_mut().find(|d| d.id == id) else {
                    // Closed while it was connecting.
                    return None;
                };
                d.carrier = Some(carrier);
                d.state = State::Open { addr, via_name, client: None };
                launch(d, env);
                Some((d.profile, via))
            }
        }
    }

    /// Notices programs that ended; their port closes with them.
    fn poll(&mut self, rt: &tokio::runtime::Handle) {
        for d in &mut self.items {
            let Some((child, since)) = &mut d.child else { continue };
            let Ok(Some(status)) = child.try_wait() else { continue };
            let quick = since.elapsed().as_secs() < 10;
            d.child = None;
            if quick {
                // It stopped before a session could have started (or handed
                // it to another process): the port stays for another try.
                d.note = Some(if status.success() {
                    "The Remote Desktop program exited at once; the port stays open until you close it.".into()
                } else {
                    format!("The Remote Desktop program stopped ({status}); the port stays open for another try.")
                });
                continue;
            }
            if let Some(c) = d.carrier.take() {
                c.close(rt);
            }
            d.state = State::Closed("Remote Desktop closed.".into());
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, rt: &tokio::runtime::Handle, env: &Env) -> Vec<RdpAction> {
        let mut out = Vec::new();
        self.poll(rt);
        if !self.open || self.items.is_empty() {
            return out;
        }
        let mut open = true;
        let mut close = None;
        let mut forget = None;
        let shown = egui::Window::new("Remote Desktop").open(&mut open).default_width(520.0).show(ctx, |ui| {
            for d in &mut self.items {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&d.name).strong());
                    match &d.state {
                        State::Starting(t) => {
                            ui.spinner();
                            ui.label(t);
                        }
                        State::Open { via_name, .. } => {
                            let via = if via_name.is_empty() { String::new() } else { format!(" through {via_name}") };
                            ui.label(format!("open{via}"));
                            if ui
                                .small_button("Open again")
                                .on_hover_text("Open the Remote Desktop window again")
                                .clicked()
                            {
                                launch(d, env);
                            }
                            if ui
                                .small_button("Close")
                                .on_hover_text("Close the forwarded port (ends the session)")
                                .clicked()
                            {
                                close = Some(d.id);
                            }
                        }
                        State::Closed(why) => {
                            ui.label(RichText::new(why.lines().next().unwrap_or("")).weak());
                            if ui.small_button("Connect again").clicked() {
                                out.push(RdpAction::Open(d.profile));
                            }
                            if ui.small_button("Dismiss").clicked() {
                                forget = Some(d.id);
                            }
                        }
                    }
                });
                match &d.state {
                    State::Open { addr, client, .. } => {
                        let who = if d.user.is_empty() { String::new() } else { format!(" as {}", d.user) };
                        let prog = client.map(|c| format!("{c} opened on ")).unwrap_or_default();
                        ui.label(RichText::new(format!("{prog}{addr}{who}")).weak().small());
                        if let Some(n) = &d.note {
                            ui.label(RichText::new(n).small().color(ui.visuals().warn_fg_color));
                        }
                        let mut go = false;
                        if let Some(a) = &mut d.asking {
                            ui.horizontal(|ui| {
                                ui.label(format!("Windows password for {}", d.user));
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut a.password).password(true).desired_width(160.0),
                                );
                                ui.checkbox(&mut a.remember, "Remember in the system keychain");
                                go = ui.button("Open").clicked()
                                    || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                            });
                            ui.label(
                                RichText::new(
                                    "FreeRDP has no login window of its own, so MalasDoh passes the password to it \
                                     (not on its command line).",
                                )
                                .weak()
                                .small(),
                            );
                        } else if client.is_some_and(malasdoh_core::rdp::wants_password)
                            && ui.small_button("Forget the saved password").clicked()
                        {
                            env.secrets.delete(&SecretKey::RdpPassword(d.profile));
                        }
                        if go && let Some(a) = d.asking.take() {
                            if a.remember
                                && let Err(e) = env.secrets.set(&SecretKey::RdpPassword(d.profile), &a.password)
                            {
                                d.note = Some(format!("The password was not saved: {e}"));
                            }
                            d.typed = Some(a.password);
                            launch(d, env);
                        }
                    }
                    State::Closed(why) if why.lines().count() > 1 || why.len() > 60 => {
                        ui.label(RichText::new(why).small().color(ui.visuals().warn_fg_color));
                    }
                    _ => {}
                }
                ui.separator();
            }
            ui.label(
                RichText::new(
                    "Your own Remote Desktop program does the drawing (that is what keeps it smooth); MalasDoh \
                     only carries its connection. Closing it here ends the session.",
                )
                .weak()
                .small(),
            );
            if self.items.iter().any(|d| matches!(d.state, State::Starting(_))) {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(2));
            }
        });
        if let Some(id) = close
            && let Some(d) = self.items.iter_mut().find(|d| d.id == id)
        {
            if let Some(c) = d.carrier.take() {
                c.close(rt);
            }
            d.state = State::Closed("Closed.".into());
        }
        if let Some(id) = forget {
            self.items.retain(|d| d.id != id);
        }
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        if !open {
            // Hidden, not closed: open desktops stay open.
            self.open = false;
            self.items.retain(|d| !matches!(d.state, State::Closed(_)));
        }
        out
    }
}

/// Opens the Remote Desktop program on an open desktop (FreeRDP gets the
/// Windows password: typed just now, saved, or asked for first).
fn launch(d: &mut Desktop, env: &Env) {
    let State::Open { addr, client, .. } = &mut d.state else { return };
    let mut password = None;
    if malasdoh_core::rdp::client().is_some_and(malasdoh_core::rdp::wants_password) {
        if d.user.trim().is_empty() {
            d.note = Some("Set the Windows user first (Edit… on the connection).".into());
            return;
        }
        password = d.typed.take().or_else(|| env.secrets.get(&SecretKey::RdpPassword(d.profile)));
        if password.is_none() {
            d.asking = Some(Ask::default());
            d.note = None;
            return;
        }
    }
    let file = match malasdoh_core::rdp::write_rdp_file(&env.dir, &d.name, addr, &d.user) {
        Ok(f) => f,
        Err(e) => {
            d.note = Some(format!("{e:#}"));
            return;
        }
    };
    match malasdoh_core::rdp::launch(&file, addr, &d.user, password.as_deref()) {
        Ok((child, name)) => {
            *client = Some(name);
            d.child = Some((child, Instant::now()));
            d.note = None;
        }
        Err(e) => {
            // The port stays open for a program started by hand.
            d.note = Some(format!("{e:#}. Open {addr} in your Remote Desktop program instead."));
        }
    }
}
