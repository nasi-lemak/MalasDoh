//! Connection → WireGuard…: import a WireGuard configuration so connections
//! can go through MalasDoh's own tunnel (no WireGuard install, no admin).

use egui::RichText;
use malasdoh_core::sftp::human_size;
use malasdoh_core::wireguard::{self, WgConfig, WgEntry, WgStore};

/// What the app does for the window (it owns the store and the keychain).
pub enum WgAction {
    /// Store this entry, and its private key in the keychain; the full
    /// text is kept for computers without a keychain.
    Add(WgEntry, String, String),
    Rename(uuid::Uuid, String),
    Remove(uuid::Uuid),
}

#[derive(Default)]
pub struct WireGuardWindow {
    pub open: bool,
    name: String,
    text: String,
    error: Option<String>,
    message: Option<String>,
    renaming: Option<(uuid::Uuid, String)>,
    confirm_remove: Option<uuid::Uuid>,
}

/// One line about a stored configuration: its address and server(s).
fn summary(e: &WgEntry) -> String {
    // The stored text has no key; a placeholder lets it parse for display.
    match WgConfig::parse_with_key(&e.config, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=") {
        Ok(c) => {
            let addr: Vec<String> = c.addresses.iter().map(|(a, l)| format!("{a}/{l}")).collect();
            let peers: Vec<String> = c.peers.iter().map(|p| p.endpoint.clone()).collect();
            let routes: Vec<String> =
                c.peers.iter().flat_map(|p| p.allowed_ips.iter().map(|(a, l)| format!("{a}/{l}"))).collect();
            format!("{} via {} · reaches {}", addr.join(", "), peers.join(", "), routes.join(", "))
        }
        Err(e) => format!("unreadable: {e:#}"),
    }
}

impl WireGuardWindow {
    /// What happened to an import, from the app once it is stored.
    pub fn imported(&mut self, msg: String) {
        self.message = Some(msg);
    }

    /// `used`: how many connections use each configuration.
    pub fn ui(&mut self, ctx: &egui::Context, store: &WgStore, used: &dyn Fn(uuid::Uuid) -> usize) -> Vec<WgAction> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        let mut open = true;
        let running = wireguard::running();
        egui::Window::new("WireGuard").open(&mut open).default_width(640.0).show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "MalasDoh can reach servers through a WireGuard tunnel of its own: nothing to install, no admin \
                     rights, and only connections set to use it go through it. Import the .conf you would give \
                     WireGuard, then choose it as the Proxy of a connection.",
                )
                .weak(),
            );
            ui.separator();
            if store.tunnels.is_empty() {
                ui.label("No configurations yet.");
            }
            for e in &store.tunnels {
                ui.horizontal(|ui| {
                    match &mut self.renaming {
                        Some((id, name)) if *id == e.id => {
                            let r = ui.text_edit_singleline(name);
                            if ui.button("Save").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                                if !name.trim().is_empty() {
                                    out.push(WgAction::Rename(e.id, name.trim().to_string()));
                                }
                                self.renaming = None;
                            }
                        }
                        _ => {
                            ui.label(RichText::new(&e.name).strong());
                            if ui.small_button("Rename").clicked() {
                                self.renaming = Some((e.id, e.name.clone()));
                            }
                        }
                    }
                    let n = used(e.id);
                    ui.label(RichText::new(format!("used by {n} connection(s)")).weak());
                    if self.confirm_remove == Some(e.id) {
                        ui.colored_label(ui.visuals().warn_fg_color, "Remove it and its key?");
                        if ui.small_button("Remove").clicked() {
                            out.push(WgAction::Remove(e.id));
                            self.confirm_remove = None;
                        }
                        if ui.small_button("Keep").clicked() {
                            self.confirm_remove = None;
                        }
                    } else if ui.small_button("Remove…").clicked() {
                        self.confirm_remove = Some(e.id);
                    }
                });
                ui.label(RichText::new(summary(e)).small());
                if e.key_in_file {
                    ui.label(
                        RichText::new("Private key kept in MalasDoh's settings file (no keychain on this computer).")
                            .small()
                            .weak(),
                    );
                }
                match running.iter().find(|(id, _)| *id == e.id) {
                    Some((_, st)) if st.handshake => {
                        ui.label(
                            RichText::new(format!(
                                "Running · sent {} · received {}",
                                human_size(st.tx_bytes),
                                human_size(st.rx_bytes)
                            ))
                            .small()
                            .color(egui::Color32::from_rgb(0x5c, 0xb8, 0x5c)),
                        );
                    }
                    Some(_) => {
                        ui.label(
                            RichText::new("Starting: waiting for the server's handshake")
                                .small()
                                .color(ui.visuals().warn_fg_color),
                        );
                    }
                    None => {
                        ui.label(RichText::new("not running (starts with the first connection that uses it)").small().weak());
                    }
                }
                ui.add_space(4.0);
            }
            ui.separator();
            ui.label(RichText::new("Import a configuration").strong());
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("e.g. office").desired_width(160.0));
                if ui.button("Open a .conf file…").clicked()
                    && let Some(f) =
                        rfd::FileDialog::new().add_filter("WireGuard", &["conf"]).set_title("WireGuard configuration").pick_file()
                {
                    match std::fs::read_to_string(&f) {
                        Ok(t) => {
                            self.text = t;
                            if self.name.trim().is_empty() {
                                self.name = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                            }
                            self.error = None;
                        }
                        Err(e) => self.error = Some(format!("{}: {e}", f.display())),
                    }
                }
            });
            ui.label(RichText::new("Or paste it here:").weak());
            egui::ScrollArea::vertical().id_salt("wg-text").max_height(160.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.text)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY)
                        .hint_text("[Interface]\nPrivateKey = …\nAddress = 10.0.0.2/32\n\n[Peer]\nPublicKey = …\nEndpoint = vpn.example.com:51820\nAllowedIPs = 10.0.0.0/24"),
                );
            });
            let can = !self.text.trim().is_empty() && !self.name.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Import")).clicked() {
                // Checked here first; the store adds it.
                let mut probe = WgStore::default();
                match probe.import(&self.name, &self.text) {
                    Ok((entry, key)) => {
                        out.push(WgAction::Add(entry, key, self.text.clone()));
                        self.text.clear();
                        self.name.clear();
                        self.error = None;
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            if let Some(m) = &self.message {
                ui.label(m);
            }
            ui.label(
                RichText::new(
                    "Only the connections set to use it go through it; the rest of this computer is not affected. \
                     Server names are looked up with the tunnel's DNS servers, if it lists any.",
                )
                .weak()
                .small(),
            );
            if !running.is_empty() {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            }
        });
        if !open {
            self.open = false;
            self.text.clear();
            self.message = None;
            self.error = None;
        }
        out
    }
}
