//! The port-forwarding window of a session, and the spec editor shared with
//! the profile form.

use crate::dlg::FitScreen as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use egui::RichText;
use malasdoh_core::forward::{ForwardHandle, ForwardKind, ForwardSpec};
use malasdoh_core::sftp::human_size;
use malasdoh_core::ssh::Connection;

use crate::bridge::SessionId;
use crate::session::{SessionMsg, Shared};

pub enum ForwardMsg {
    Started { index: usize, result: Result<ForwardHandle, String> },
}

struct Item {
    spec: ForwardSpec,
    handle: Option<ForwardHandle>,
    starting: bool,
    error: Option<String>,
    /// Should be running: restarted after a reconnect.
    wanted: bool,
    /// Added in this session only, not saved to the profile.
    temporary: bool,
}

#[derive(Default)]
pub struct Forwards {
    items: Vec<Item>,
    pub open: bool,
    draft: Option<ForwardSpec>,
}

impl Forwards {
    pub fn new(specs: &[ForwardSpec]) -> Self {
        let items = specs
            .iter()
            .map(|s| Item {
                spec: s.clone(),
                handle: None,
                starting: false,
                error: None,
                wanted: s.auto_start,
                temporary: false,
            })
            .collect();
        Self { items, open: false, draft: None }
    }

    pub fn running(&self) -> usize {
        self.items.iter().filter(|i| i.handle.is_some()).count()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    fn start(&mut self, index: usize, conn: &Arc<Connection>, shared: &Shared, session: SessionId) {
        let Some(item) = self.items.get_mut(index) else { return };
        if item.handle.is_some() || item.starting {
            return;
        }
        item.starting = true;
        item.wanted = true;
        item.error = None;
        let (conn, spec, events) = (conn.clone(), item.spec.clone(), shared.events.clone());
        shared.rt.spawn(async move {
            let result = conn.start_forward(&spec).await.map_err(|e| format!("{e:#}"));
            events.session(session, SessionMsg::Forward(ForwardMsg::Started { index, result }));
        });
    }

    fn stop(&mut self, index: usize, conn: Option<&Arc<Connection>>, shared: &Shared) {
        let Some(item) = self.items.get_mut(index) else { return };
        item.wanted = false;
        if let Some(h) = item.handle.take() {
            match conn {
                Some(c) => {
                    let c = c.clone();
                    shared.rt.spawn(async move { c.stop_forward(h).await });
                }
                None => drop(h),
            }
        }
    }

    /// Start everything that should be running.
    pub fn on_connected(&mut self, conn: &Arc<Connection>, shared: &Shared, session: SessionId) {
        for i in 0..self.items.len() {
            if self.items[i].wanted {
                self.start(i, conn, shared, session);
            }
        }
    }

    /// The connection is gone; listeners die with it but are restarted on reconnect.
    pub fn on_disconnected(&mut self) {
        for item in &mut self.items {
            item.handle = None;
            item.starting = false;
        }
    }

    pub fn handle(&mut self, msg: ForwardMsg) {
        match msg {
            ForwardMsg::Started { index, result } => {
                let Some(item) = self.items.get_mut(index) else { return };
                item.starting = false;
                match result {
                    Ok(h) => item.handle = Some(h),
                    Err(e) => {
                        item.wanted = false;
                        item.error = Some(e);
                    }
                }
            }
        }
    }

    /// The forwards that belong in the profile (not this-session-only ones).
    pub fn saved_specs(&self) -> Vec<ForwardSpec> {
        self.items.iter().filter(|i| !i.temporary).map(|i| i.spec.clone()).collect()
    }

    /// Returns `true` when the user asked to save the list to the profile.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        title: &str,
        conn: Option<&Arc<Connection>>,
        shared: &Shared,
        session: SessionId,
    ) -> bool {
        if !self.open {
            return false;
        }
        let mut open = true;
        let mut save = false;
        let mut start = None;
        let mut stop = None;
        let mut remove = None;
        let shown = egui::Window::new(format!("Port forwards — {title}"))
            .id(egui::Id::new(("forwards", session)))
            .open(&mut open)
            .default_width(640.0)
            .fit_to_screen(ctx).show(ctx, |ui| {
                if self.items.is_empty() {
                    ui.label("No forwards yet. Add one below, or in the connection's settings.");
                }
                for (i, item) in self.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let name = if item.spec.name.is_empty() { item.spec.kind.label().to_string() } else { item.spec.name.clone() };
                        ui.label(RichText::new(name).strong());
                        ui.label(item.spec.describe());
                        if item.temporary {
                            ui.label(RichText::new("(this session)").weak());
                        }
                    });
                    ui.horizontal(|ui| {
                        match (&item.handle, item.starting) {
                            (Some(h), _) => {
                                let s = &h.stats;
                                ui.colored_label(
                                    crate::look::sem(ui.ctx()).good,
                                    format!(
                                        "• listening on port {} · {} open · {} total · {}",
                                        h.port,
                                        s.active.load(Ordering::Relaxed),
                                        s.total.load(Ordering::Relaxed),
                                        human_size(s.bytes.load(Ordering::Relaxed))
                                    ),
                                );
                                if ui.small_button("Stop").clicked() {
                                    stop = Some(i);
                                }
                                if h.spec.kind != ForwardKind::Remote && ui.small_button("Copy address").clicked() {
                                    ui.ctx().copy_text(format!("{}:{}", h.spec.bind_host, h.port));
                                }
                                if let Some(e) = s.last_error.lock().unwrap().as_ref() {
                                    ui.colored_label(ui.visuals().warn_fg_color, format!("last error: {e}"));
                                }
                            }
                            (None, true) => {
                                ui.spinner();
                                ui.label("starting…");
                            }
                            (None, false) => {
                                ui.label(RichText::new("stopped").weak());
                                if ui.add_enabled(conn.is_some(), egui::Button::new("Start").small()).clicked() {
                                    start = Some(i);
                                }
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(i);
                                }
                                if let Some(e) = &item.error {
                                    ui.colored_label(ui.visuals().error_fg_color, e);
                                }
                            }
                        }
                    });
                    ui.separator();
                }
                match &mut self.draft {
                    None => {
                        ui.horizontal(|ui| {
                            if ui.button("+ Add forward").clicked() {
                                self.draft = Some(ForwardSpec::default());
                            }
                            if self.items.iter().any(|i| i.temporary) && ui.button("Save list to this connection").clicked() {
                                save = true;
                            }
                        });
                    }
                    Some(spec) => {
                        egui::Grid::new(("draft", session)).num_columns(2).show(ui, |ui| spec_editor(ui, spec));
                        let valid = spec.validate();
                        if let Err(e) = &valid {
                            ui.colored_label(ui.visuals().warn_fg_color, e.to_string());
                        }
                        ui.horizontal(|ui| {
                            if ui.add_enabled(valid.is_ok(), egui::Button::new("Add and start")).clicked()
                                || (valid.is_ok() && crate::dlg::window_enter(ui, false))
                            {
                                let spec = self.draft.take().unwrap();
                                self.items.push(Item { spec, handle: None, starting: false, error: None, wanted: true, temporary: true });
                                start = Some(self.items.len() - 1);
                            }
                            if ui.button("Cancel").clicked() || crate::dlg::window_escape(ui) {
                                self.draft = None;
                            }
                        });
                    }
                }
                ui.label(
                    RichText::new("Listening on 127.0.0.1 keeps a forward private to this computer; 0.0.0.0 shares it with your network.")
                        .weak()
                        .small(),
                );
            });
        if crate::dlg::closes(ctx, &shown) {
            open = false;
        }
        self.open = open;
        if let Some(i) = stop {
            self.stop(i, conn, shared);
        }
        if let (Some(i), Some(c)) = (start, conn) {
            self.start(i, c, shared, session);
        }
        if let Some(i) = remove {
            // Removing a saved forward changes the profile.
            save |= !self.items.remove(i).temporary;
        } else if save {
            for it in &mut self.items {
                it.temporary = false;
            }
        }
        save
    }
}

/// Grid rows for editing one forward (used here and in the profile form).
pub fn spec_editor(ui: &mut egui::Ui, spec: &mut ForwardSpec) {
    ui.label("Type");
    ui.horizontal(|ui| {
        for k in [ForwardKind::Local, ForwardKind::Remote, ForwardKind::Dynamic] {
            ui.selectable_value(&mut spec.kind, k, k.label());
        }
    });
    ui.end_row();
    ui.label("Name");
    ui.add(egui::TextEdit::singleline(&mut spec.name).hint_text("postgres, admin UI…").desired_width(200.0));
    ui.end_row();
    ui.label(if spec.kind == ForwardKind::Remote { "Listen on server" } else { "Listen here" });
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut spec.bind_host).desired_width(130.0));
        ui.label(":");
        port_field(ui, &mut spec.bind_port, "0 = any");
    });
    ui.end_row();
    if spec.kind != ForwardKind::Dynamic {
        ui.label(if spec.kind == ForwardKind::Remote { "Connect to (from here)" } else { "Connect to (from server)" });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut spec.target_host).desired_width(130.0));
            ui.label(":");
            port_field(ui, &mut spec.target_port, "port");
        });
        ui.end_row();
    }
    ui.label("");
    ui.checkbox(&mut spec.auto_start, "Start when connected");
    ui.end_row();
}

fn port_field(ui: &mut egui::Ui, port: &mut u16, hint: &str) {
    let mut text = if *port == 0 { String::new() } else { port.to_string() };
    if ui.add(egui::TextEdit::singleline(&mut text).desired_width(60.0).hint_text(hint)).changed() {
        *port = text.trim().parse().unwrap_or(0);
    }
}
