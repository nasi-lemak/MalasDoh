//! What the SSH keys window asks of the app: keeping pasted keys (in the
//! keychain), and switching connections to a key.

use malasdoh_core::profile::AuthMethod;
use malasdoh_core::secrets::SecretKey;

use super::App;
use crate::keys_ui::{KeyChoice, KeysAction};

impl App {
    pub(super) fn key_actions(&mut self, actions: Vec<KeysAction>) {
        if actions.is_empty() {
            return;
        }
        let mut store_changed = false;
        for a in actions {
            match a {
                KeysAction::UseKey(id, choice) => {
                    let Some(mut p) = self.profiles.get(id).cloned() else { continue };
                    let what = match &choice {
                        KeyChoice::File(path) => path.display().to_string(),
                        KeyChoice::Stored(_, name) => format!("the stored key \"{name}\""),
                    };
                    p.auth = match choice {
                        KeyChoice::File(path) => AuthMethod::Key { path: path.display().to_string() },
                        KeyChoice::Stored(id, name) => AuthMethod::Stored { id, name },
                    };
                    let name = p.display_name();
                    if self.profiles.upsert(p).is_ok() {
                        self.save_profiles();
                        self.notify(format!("{name} now logs in with {what}"), false);
                    }
                }
                KeysAction::AddStored(mut entry, private) => {
                    let msg = match self.shared.secrets.set(&SecretKey::StoredKey(entry.id), &private) {
                        Ok(()) => (format!("Kept \"{}\" in the system keychain.", entry.name), false),
                        Err(e) => {
                            // No keychain (some Linux setups): the owner-only settings file.
                            entry.key_in_file = true;
                            entry.private = Some(private);
                            (
                                format!(
                                    "Kept \"{}\". This computer has no keychain ({e}), so the key is in MalasDoh's \
                                     settings file, readable only by you.",
                                    entry.name
                                ),
                                true,
                            )
                        }
                    };
                    self.keys_win.stored_message = Some(msg);
                    self.key_store.keys.push(entry);
                    store_changed = true;
                }
                KeysAction::RenameStored(id, name) => {
                    if let Some(k) = self.key_store.keys.iter_mut().find(|k| k.id == id) {
                        k.name = name.clone();
                        store_changed = true;
                    }
                    // Connections show the key's name.
                    let mut changed = false;
                    for p in &mut self.profiles.profiles {
                        if let AuthMethod::Stored { id: k, name: n } = &mut p.auth
                            && *k == id
                        {
                            *n = name.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        self.save_profiles();
                    }
                }
                KeysAction::SetCertificate(id, cert) => {
                    if let Some(k) = self.key_store.keys.iter_mut().find(|k| k.id == id) {
                        k.certificate = cert;
                        store_changed = true;
                    }
                }
                KeysAction::RemoveStored(id) => {
                    let Some(k) = self.key_store.get(id).cloned() else { continue };
                    self.key_store.keys.retain(|k| k.id != id);
                    self.shared.secrets.delete(&SecretKey::StoredKey(id));
                    let using: Vec<String> = self
                        .profiles
                        .profiles
                        .iter()
                        .filter(|p| matches!(&p.auth, AuthMethod::Stored { id: k, .. } if *k == id))
                        .map(|p| p.display_name())
                        .collect();
                    self.keys_win.stored_message = Some(if using.is_empty() {
                        (format!("Removed \"{}\".", k.name), false)
                    } else {
                        (
                            format!(
                                "Removed \"{}\". These connections used it; choose another way to log in for them: {}.",
                                k.name,
                                using.join(", ")
                            ),
                            true,
                        )
                    });
                    store_changed = true;
                }
            }
        }
        if store_changed {
            if let Err(e) = self.key_store.save() {
                self.notify(format!("Could not save the stored keys: {e:#}"), true);
            }
            *self.shared.stored_keys.write().unwrap() = self.key_store.map();
        }
    }
}
