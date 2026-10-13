//! Passwords and key passphrases, kept in the OS keychain (Windows Credential
//! Manager, macOS Keychain, Secret Service on Linux) and never in MalasDoh's
//! own files. Only stored when the user ticks "remember".

use std::collections::HashMap;
use std::sync::Mutex;

use crate::profile::ProfileId;

const SERVICE: &str = "MalasDoh";

/// Which secret, for which connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretKey {
    Password(ProfileId),
    /// Keyed by the key file path, since one key is often shared by many hosts.
    Passphrase(String),
    /// The proxy password for a connection.
    ProxyPassword(ProfileId),
    /// The sudo password on a connection's server.
    SudoPassword(ProfileId),
    /// The password for a Teleport cluster's user.
    TeleportPassword(uuid::Uuid),
}

impl SecretKey {
    fn account(&self) -> String {
        match self {
            SecretKey::Password(id) => format!("password:{id}"),
            SecretKey::Passphrase(path) => format!("passphrase:{path}"),
            SecretKey::ProxyPassword(id) => format!("proxy-password:{id}"),
            SecretKey::SudoPassword(id) => format!("sudo-password:{id}"),
            SecretKey::TeleportPassword(id) => format!("teleport-password:{id}"),
        }
    }
}

/// Storage for secrets. Calls may block (the keychain can show a prompt), so
/// call them off the UI thread.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &SecretKey) -> Option<String>;
    fn set(&self, key: &SecretKey, secret: &str) -> Result<(), String>;
    fn delete(&self, key: &SecretKey);
}

/// The OS keychain. If there is none (a headless Linux box without a Secret
/// Service), reads return nothing and writes report why, so the user is simply
/// asked each time.
pub struct OsKeychain;

impl SecretStore for OsKeychain {
    fn get(&self, key: &SecretKey) -> Option<String> {
        keyring::Entry::new(SERVICE, &key.account()).ok()?.get_password().ok()
    }

    fn set(&self, key: &SecretKey, secret: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, &key.account())
            .and_then(|e| e.set_password(secret))
            .map_err(|e| format!("could not save to the system keychain: {e}"))
    }

    fn delete(&self, key: &SecretKey) {
        if let Ok(e) = keyring::Entry::new(SERVICE, &key.account()) {
            let _ = e.delete_credential();
        }
    }
}

/// For tests, and for "don't remember anything" mode.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<SecretKey, String>>);

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Option<String> {
        self.0.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: &SecretKey, secret: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.clone(), secret.to_string());
        Ok(())
    }

    fn delete(&self, key: &SecretKey) {
        self.0.lock().unwrap().remove(key);
    }
}
