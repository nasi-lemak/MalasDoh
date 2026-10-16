//! What the WireGuard window asks of the app: storing configurations, with
//! their private keys in the keychain.

use malasdoh_core::proxy::Proxy;
use malasdoh_core::secrets::SecretKey;

use malasdoh_core::wireguard::{self, Edited, TestStep, WgConfig};

use super::App;
use crate::wireguard_ui::{TestSlot, WgAction};

impl App {
    pub(super) fn wireguard_actions(&mut self, ctx: &egui::Context, actions: Vec<WgAction>) {
        if actions.is_empty() {
            return;
        }
        let mut changed_store = false;
        for a in actions {
            if !matches!(a, WgAction::Test(..)) {
                changed_store = true;
            }
            match a {
                WgAction::Add(mut entry, key, full) => {
                    let msg = match self.shared.secrets.set(&SecretKey::WireGuardKey(entry.id), &key) {
                        Ok(()) => format!(
                            "Imported \"{}\". Its private key is in the system keychain, not in MalasDoh's files.",
                            entry.name
                        ),
                        Err(e) => {
                            // No keychain (some Linux setups): keep the key with
                            // the configuration, in the owner-only settings file.
                            entry.config = full;
                            entry.key_in_file = true;
                            format!(
                                "Imported \"{}\". This computer has no keychain ({e}), so its private key is kept in \
                                 MalasDoh's settings file, readable only by you.",
                                entry.name
                            )
                        }
                    };
                    self.wg_win.tell(msg);
                    self.wg_win.imported(entry.id, entry.name.clone());
                    let id = entry.id;
                    self.wg_store.tunnels.push(entry);
                    // A bad configuration shows up before any connection uses it.
                    self.start_wg_test(ctx, id, None);
                }
                WgAction::Rename(id, name) => {
                    if let Some(t) = self.wg_store.tunnels.iter_mut().find(|t| t.id == id) {
                        t.name = name.clone();
                    }
                    // Connections show the tunnel's name.
                    let mut changed = false;
                    for p in &mut self.profiles.profiles {
                        if let Proxy::WireGuard { tunnel, name: n } = &mut p.proxy
                            && *tunnel == id
                        {
                            *n = name.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        self.save_profiles();
                    }
                }
                WgAction::Edit(id, edited) => self.wireguard_edit(ctx, id, edited),
                WgAction::Test(id, target) => {
                    self.start_wg_test(ctx, id, target);
                    continue;
                }
                WgAction::Remove(id) => {
                    self.wg_store.tunnels.retain(|t| t.id != id);
                    self.shared.secrets.delete(&SecretKey::WireGuardKey(id));
                }
            }
        }
        if !changed_store {
            return;
        }
        if let Err(e) = self.wg_store.save() {
            self.notify(format!("Could not save the WireGuard configurations: {e:#}"), true);
        }
        *self.shared.wireguard.write().unwrap() = self.wg_store.configs();
    }
}

impl App {
    /// Tests a stored configuration in the background; the window shows
    /// the steps as they end.
    fn start_wg_test(&mut self, ctx: &egui::Context, id: uuid::Uuid, target: Option<(String, u16)>) {
        let Some(entry) = self.wg_store.tunnels.iter().find(|t| t.id == id) else { return };
        let text = entry.config.clone();
        let slot: TestSlot = Default::default();
        self.wg_win.testing(id, slot.clone());
        let secrets = self.shared.secrets.clone();
        let ctx = ctx.clone();
        self.shared.rt.spawn(async move {
            let key = if WgConfig::private_key_text(&text).is_some() {
                None
            } else {
                tokio::task::spawn_blocking(move || secrets.get(&SecretKey::WireGuardKey(id))).await.ok().flatten()
            };
            let steps = match WgConfig::from_stored(&text, key.as_deref()) {
                Ok(config) => wireguard::test(&config, target.as_ref().map(|(h, p)| (h.as_str(), *p))).await,
                Err(e) => vec![TestStep { what: "Configuration".into(), ok: Some(false), detail: format!("{e:#}") }],
            };
            *slot.lock().unwrap() = Some(steps);
            ctx.request_repaint();
        });
    }
}

impl App {
    /// Right-click on a group: every connection in it through a WireGuard
    /// tunnel, or (`Proxy::None`) those using WireGuard back to direct.
    /// Connections that start with a jump host, or log in with Teleport,
    /// are left as they are: their first hop decides how they are reached.
    pub(super) fn set_group_proxy(&mut self, group: &str, proxy: Proxy) {
        let (mut changed, mut replaced, mut skipped) = (0, Vec::new(), Vec::new());
        for p in &mut self.profiles.profiles {
            if p.group.trim() != group {
                continue;
            }
            let wg = matches!(p.proxy, Proxy::WireGuard { .. });
            if proxy.is_none() && !wg {
                continue;
            }
            if p.jump.is_some() || p.teleport.is_some() {
                if !proxy.is_none() {
                    skipped.push(p.display_name());
                }
                continue;
            }
            if p.proxy != proxy {
                if !wg && !p.proxy.is_none() {
                    replaced.push(p.display_name());
                }
                p.proxy = proxy.clone();
                changed += 1;
            }
        }
        if changed > 0 {
            self.save_profiles();
        }
        let mut msg = match &proxy {
            Proxy::None => format!("{changed} connection(s) back to direct."),
            p => format!("{changed} connection(s) now use {}.", p.label()),
        };
        if !replaced.is_empty() {
            msg.push_str(&format!(" Replaced the proxy they had: {}.", replaced.join(", ")));
        }
        if !skipped.is_empty() {
            msg.push_str(&format!(" Skipped (they start at a jump host or Teleport): {}.", skipped.join(", ")));
        }
        if changed > 0 {
            msg.push_str(" Open ones change when they reconnect.");
        }
        self.notify(msg, false);
    }
}

impl App {
    /// Stores an edited configuration under the same id, so the
    /// connections using it keep using it, and tests it.
    fn wireguard_edit(&mut self, ctx: &egui::Context, id: uuid::Uuid, edited: Edited) {
        let Some(entry) = self.wg_store.tunnels.iter_mut().find(|t| t.id == id) else { return };
        let mut note = String::new();
        let stored = match edited.new_key {
            // The keychain still has it; or the old text did.
            None if !entry.key_in_file => Ok(edited.config),
            None => match WgConfig::private_key_text(&entry.config) {
                Some(k) => WgConfig::with_private_key(&edited.config, &k),
                None => Err(anyhow::anyhow!("its saved private key is missing; paste it on the PrivateKey line")),
            },
            Some(k) => match self.shared.secrets.set(&SecretKey::WireGuardKey(id), &k) {
                Ok(()) => {
                    note = " The new private key is in the system keychain.".into();
                    entry.key_in_file = false;
                    Ok(edited.config)
                }
                Err(e) => {
                    // An old key there must not outlive this one.
                    self.shared.secrets.delete(&SecretKey::WireGuardKey(id));
                    note = format!(" No keychain ({e}): the new private key is kept in MalasDoh's settings file.");
                    entry.key_in_file = true;
                    WgConfig::with_private_key(&edited.config, &k)
                }
            },
        };
        match stored {
            Ok(text) => {
                entry.config = text;
                let name = entry.name.clone();
                let open = wireguard::running().iter().any(|(t, _)| *t == id);
                self.wg_win.tell(format!(
                    "Saved \"{name}\".{note}{}",
                    if open { " Connections open through it keep the old settings until they reconnect." } else { "" }
                ));
                self.start_wg_test(ctx, id, None);
            }
            Err(e) => self.wg_win.tell(format!("Not saved: {e:#}")),
        }
    }
}
