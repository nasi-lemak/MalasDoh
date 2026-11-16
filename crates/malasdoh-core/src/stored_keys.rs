//! Private keys pasted into MalasDoh instead of kept in files (from a
//! password manager, say). The key itself goes to the system keychain as
//! [`crate::secrets::SecretKey::StoredKey`]; its name, public key and
//! certificate are kept in `ssh_keys.toml`. On a computer without a
//! keychain the key stays in that (owner-only) file, like an unencrypted
//! key file in `~/.ssh`, and `key_in_file` says so.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use russh::keys::ssh_key::LineEnding;

/// A stored key's details (not the private key, unless `key_in_file`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredKey {
    pub id: uuid::Uuid,
    pub name: String,
    /// The public key line, `ssh-ed25519 AAAA… comment`.
    pub public: String,
    /// An OpenSSH certificate for the key (`ssh-ed25519-cert-v01@openssh.com
    /// AAAA…`), used before the plain key when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key_in_file: bool,
    /// The private key, only when there is no keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<String>,
}

impl StoredKey {
    /// `ED25519 · SHA256:…`, to tell keys apart.
    pub fn describe(&self) -> String {
        match russh::keys::PublicKey::from_openssh(&self.public) {
            Ok(k) => format!("{} · {}", k.algorithm().as_str(), k.fingerprint(Default::default())),
            Err(_) => "unreadable public key".into(),
        }
    }
}

/// What connections need of the stored keys, by id (shared with
/// [`crate::ssh::ConnectContext::stored_keys`]).
pub type StoredKeys = Arc<RwLock<HashMap<uuid::Uuid, StoredKey>>>;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default, rename = "key")]
    keys: Vec<StoredKey>,
}

/// The stored keys, in `ssh_keys.toml`.
#[derive(Debug, Default)]
pub struct KeyStore {
    path: PathBuf,
    pub keys: Vec<StoredKey>,
}

impl KeyStore {
    pub fn load(path: PathBuf) -> Self {
        let keys = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<StoreFile>(&t).ok())
            .map(|f| f.keys)
            .unwrap_or_default();
        Self { path, keys }
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string(&StoreFile { keys: self.keys.clone() })?;
        crate::paths::write_private_atomic(&self.path, text.as_bytes())
    }

    /// For [`crate::ssh::ConnectContext::stored_keys`].
    pub fn map(&self) -> HashMap<uuid::Uuid, StoredKey> {
        self.keys.iter().map(|k| (k.id, k.clone())).collect()
    }

    pub fn get(&self, id: uuid::Uuid) -> Option<&StoredKey> {
        self.keys.iter().find(|k| k.id == id)
    }
}

/// Why a pasted key was not taken.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PasteError {
    #[error("this key is protected by a passphrase: enter it")]
    NeedsPassphrase,
    #[error("the passphrase is not right")]
    WrongPassphrase,
    #[error("{0}")]
    Invalid(String),
}

/// A pasted private key, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pasted {
    /// The key in OpenSSH form, without a passphrase (for the keychain,
    /// which protects it instead).
    pub private: String,
    pub public: String,
}

/// Text as copied from a web page or a password manager: line breaks of
/// any kind, spaces around lines, and nothing outside the key.
fn tidy(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// Checks a pasted private key (OpenSSH, PEM or PuTTY `.ppk`), with its
/// passphrase if it has one.
pub fn check_pasted(text: &str, passphrase: Option<&str>) -> Result<Pasted, PasteError> {
    let text = tidy(text);
    if text.trim().is_empty() {
        return Err(PasteError::Invalid("paste the private key".into()));
    }
    if text.trim_start().starts_with("ssh-") || text.trim_start().starts_with("ecdsa-") {
        return Err(PasteError::Invalid(
            "this is a public key: paste the private one (it starts with -----BEGIN)".into(),
        ));
    }
    let pass = passphrase.filter(|p| !p.is_empty());
    let key = match russh::keys::decode_secret_key(&text, pass) {
        Ok(k) => k,
        Err(russh::keys::Error::KeyIsEncrypted) if pass.is_none() => return Err(PasteError::NeedsPassphrase),
        Err(russh::keys::Error::KeyIsEncrypted) => return Err(PasteError::WrongPassphrase),
        Err(_) if pass.is_some() && encrypted(&text) => return Err(PasteError::WrongPassphrase),
        Err(e) => return Err(PasteError::Invalid(format!("not a private key MalasDoh can read ({e})"))),
    };
    let public = key.public_key().to_openssh().map_err(|e| PasteError::Invalid(e.to_string()))?;
    let private = key.to_openssh(LineEnding::LF).map_err(|e| PasteError::Invalid(e.to_string()))?.to_string();
    Ok(Pasted { private, public })
}

/// Does `text` hold a key that needs a passphrase?
fn encrypted(text: &str) -> bool {
    matches!(russh::keys::decode_secret_key(text, None), Err(russh::keys::Error::KeyIsEncrypted))
}

/// Checks an OpenSSH certificate is one for `public`; returns it tidied.
pub fn check_certificate(line: &str, public: &str) -> Result<String> {
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let cert = russh::keys::Certificate::from_openssh(&line).context("not an OpenSSH certificate")?;
    let key = russh::keys::PublicKey::from_openssh(public).context("the stored public key is unreadable")?;
    if cert.public_key() != key.key_data() {
        bail!("this certificate is for another key");
    }
    Ok(line)
}

/// A stored key's private key: from the keychain, or the store file on a
/// computer without one.
pub fn private_key(entry: &StoredKey, secrets: &dyn crate::secrets::SecretStore) -> Result<String> {
    if entry.key_in_file {
        return entry.private.clone().context("its private key is missing from the settings file");
    }
    secrets
        .get(&crate::secrets::SecretKey::StoredKey(entry.id))
        .with_context(|| format!("the key \"{}\" is missing from the system keychain; paste it again", entry.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretStore;

    fn ed25519(pass: &str) -> (String, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let public = crate::keys::generate(&path, "me@pc", pass).unwrap();
        (std::fs::read_to_string(&path).unwrap(), public)
    }

    #[test]
    fn a_pasted_key_is_checked_and_its_passphrase_taken_off() {
        let (text, public) = ed25519("");
        let got = check_pasted(&text, None).unwrap();
        assert_eq!(got.public, public);
        assert!(got.private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));

        // As pasted from a web page: CRLF, indented, blank lines around.
        let messy =
            format!("\r\n\r\n{}\r\n", text.lines().map(|l| format!("   {l}  ")).collect::<Vec<_>>().join("\r\n"));
        assert_eq!(check_pasted(&messy, None).unwrap().public, public);

        // With a passphrase: asked for, checked, and taken off.
        let (enc, public) = ed25519("secret");
        assert_eq!(check_pasted(&enc, None), Err(PasteError::NeedsPassphrase));
        assert_eq!(check_pasted(&enc, Some("nope")), Err(PasteError::WrongPassphrase));
        let got = check_pasted(&enc, Some("secret")).unwrap();
        assert_eq!(got.public, public);
        assert!(russh::keys::decode_secret_key(&got.private, None).is_ok(), "stored without the passphrase");

        // Not a private key.
        assert!(matches!(check_pasted(&public, None), Err(PasteError::Invalid(m)) if m.contains("public key")));
        assert!(matches!(check_pasted("hello", None), Err(PasteError::Invalid(_))));
        assert!(matches!(check_pasted("  \n", None), Err(PasteError::Invalid(_))));
    }

    #[test]
    fn the_store_keeps_no_private_key_unless_it_must() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_keys.toml");
        let (text, public) = ed25519("");
        let mut store = KeyStore::load(path.clone());
        store.keys.push(StoredKey {
            id: uuid::Uuid::new_v4(),
            name: "proton".into(),
            public: public.clone(),
            certificate: None,
            key_in_file: false,
            private: None,
        });
        store.save().unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(&public) && !saved.contains("PRIVATE KEY"), "{saved}");
        let back = KeyStore::load(path.clone());
        assert_eq!(back.keys, store.keys);
        assert!(back.keys[0].describe().starts_with("ssh-ed25519 · SHA256:"), "{}", back.keys[0].describe());

        // The keychain has it; without one, the file does.
        let secrets = crate::secrets::MemoryStore::default();
        let id = back.keys[0].id;
        assert!(private_key(&back.keys[0], &secrets).is_err());
        secrets.set(&crate::secrets::SecretKey::StoredKey(id), &text).unwrap();
        assert_eq!(private_key(&back.keys[0], &secrets).unwrap(), text);
        let in_file = StoredKey { key_in_file: true, private: Some(text.clone()), ..back.keys[0].clone() };
        assert_eq!(private_key(&in_file, &crate::secrets::MemoryStore::default()).unwrap(), text);
    }
}
