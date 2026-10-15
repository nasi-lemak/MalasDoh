//! What the WireGuard window asks of the app: storing configurations, with
//! their private keys in the keychain.

use malasdoh_core::proxy::Proxy;
use malasdoh_core::secrets::SecretKey;

use super::App;
use crate::wireguard_ui::WgAction;

impl App {
    pub(super) fn wireguard_actions(&mut self, actions: Vec<WgAction>) {
        if actions.is_empty() {
            return;
        }
        for a in actions {
            match a {
                WgAction::Add(mut entry, key, full) => {
                    let msg = match self.shared.secrets.set(&SecretKey::WireGuardKey(entry.id), &key) {
                        Ok(()) => format!(
                            "Imported \"{}\". Its private key is in the system keychain, not in MalasDoh's files. \
                             Now edit a connection and set its Proxy to WireGuard, \"{}\".",
                            entry.name, entry.name
                        ),
                        Err(e) => {
                            // No keychain (some Linux setups): keep the key with
                            // the configuration, in the owner-only settings file.
                            entry.config = full;
                            entry.key_in_file = true;
                            format!(
                                "Imported \"{}\". This computer has no keychain ({e}), so its private key is kept in \
                                 MalasDoh's settings file, readable only by you. Now edit a connection and set its \
                                 Proxy to WireGuard, \"{}\".",
                                entry.name, entry.name
                            )
                        }
                    };
                    self.wg_win.imported(msg);
                    self.wg_store.tunnels.push(entry);
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
                WgAction::Remove(id) => {
                    self.wg_store.tunnels.retain(|t| t.id != id);
                    self.shared.secrets.delete(&SecretKey::WireGuardKey(id));
                }
            }
        }
        if let Err(e) = self.wg_store.save() {
            self.notify(format!("Could not save the WireGuard configurations: {e:#}"), true);
        }
        *self.shared.wireguard.write().unwrap() = self.wg_store.configs();
    }
}
