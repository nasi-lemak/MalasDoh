//! Saved connections ("profiles") and the file they live in.
//!
//! Secrets are never written here; passwords and key passphrases go to the OS
//! keychain (see [`crate::secrets`]).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::write_atomic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum AuthMethod {
    /// What `ssh` does: agent, then the default key files, then password /
    /// keyboard-interactive.
    #[default]
    Auto,
    Agent,
    /// A private key file (OpenSSH, PEM or PuTTY `.ppk`).
    Key {
        path: String,
    },
    Password,
}

impl AuthMethod {
    pub fn label(&self) -> &'static str {
        match self {
            AuthMethod::Auto => "Automatic",
            AuthMethod::Agent => "SSH agent",
            AuthMethod::Key { .. } => "Key file",
            AuthMethod::Password => "Password",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    /// Free-form folder, e.g. "Production / Web". Empty means ungrouped.
    pub group: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    /// Connect through this profile first (ProxyJump). Chains are allowed.
    pub jump: Option<ProfileId>,
    /// Where the file browser starts. Empty means the login directory.
    pub start_dir: String,
    /// Also keep a copy of the original next to the file on the server when
    /// saving an edit. Local backups are always made.
    pub remote_backup: bool,
    pub notes: String,
    /// Typed into each new terminal after login, e.g. `sudo -i` or
    /// `cd /var/www`. Empty: nothing.
    pub startup_command: String,
    /// Move the file browser to the terminal's folder after each command.
    /// Works with bash and zsh (a prompt hook is set up quietly), and with any
    /// shell that reports its folder itself (OSC 7).
    pub follow_terminal: bool,
    /// Port forwards for this connection.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<crate::forward::ForwardSpec>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: ProfileId::new(),
            name: String::new(),
            group: String::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthMethod::Auto,
            jump: None,
            start_dir: String::new(),
            remote_backup: false,
            notes: String::new(),
            startup_command: String::new(),
            follow_terminal: false,
            forwards: Vec::new(),
        }
    }
}

impl Profile {
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() { self.name.trim().to_string() } else { self.destination() }
    }

    /// `user@host:port` in the form people type.
    pub fn destination(&self) -> String {
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(&self.host);
        if self.port != 22 {
            s.push_str(&format!(":{}", self.port));
        }
        s
    }

    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("host is required");
        }
        if self.host.contains(char::is_whitespace) {
            bail!("host must not contain spaces");
        }
        if self.port == 0 {
            bail!("port must be between 1 and 65535");
        }
        if self.user.trim().is_empty() {
            bail!("user is required");
        }
        if let AuthMethod::Key { path } = &self.auth
            && path.trim().is_empty()
        {
            bail!("choose a key file, or switch authentication to Automatic");
        }
        if self.jump == Some(self.id) {
            bail!("a connection cannot jump through itself");
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProfileStore {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

impl ProfileStore {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        write_atomic(path, text.as_bytes())
    }

    pub fn get(&self, id: ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Insert or replace by id.
    pub fn upsert(&mut self, profile: Profile) -> Result<()> {
        profile.validate()?;
        let id = profile.id;
        let previous = self.profiles.clone();
        match self.profiles.iter_mut().find(|p| p.id == id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
        // Reject a jump loop now rather than at connect time.
        if let Err(e) = self.chain(id) {
            self.profiles = previous;
            return Err(e);
        }
        Ok(())
    }

    /// Remove a profile. Profiles that jumped through it fall back to connecting
    /// directly, and the caller is told which ones so it can say so.
    pub fn remove(&mut self, id: ProfileId) -> Vec<ProfileId> {
        self.profiles.retain(|p| p.id != id);
        let mut orphaned = Vec::new();
        for p in &mut self.profiles {
            if p.jump == Some(id) {
                p.jump = None;
                orphaned.push(p.id);
            }
        }
        orphaned
    }

    /// The hops to reach `id`, outermost first: `[bastion, ..., target]`.
    pub fn chain(&self, id: ProfileId) -> Result<Vec<Profile>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = Some(id);
        while let Some(cid) = cur {
            if !seen.insert(cid) {
                bail!("jump hosts form a loop");
            }
            let p = self.get(cid).context("a jump host for this connection was deleted")?;
            chain.push(p.clone());
            cur = p.jump;
        }
        chain.reverse();
        Ok(chain)
    }

    /// Group names in display order.
    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self.profiles.iter().map(|p| p.group.trim().to_string()).collect();
        groups.sort_by_key(|g| g.to_lowercase());
        groups.dedup();
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> Profile {
        Profile { name: name.into(), host: format!("{name}.example"), user: "me".into(), ..Default::default() }
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let mut store = ProfileStore::default();
        let mut p = profile("web");
        p.auth = AuthMethod::Key { path: "~/.ssh/id_ed25519".into() };
        store.upsert(p.clone()).unwrap();
        store.save(&path).unwrap();
        let loaded = ProfileStore::load(&path).unwrap();
        assert_eq!(loaded.profiles, vec![p]);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ProfileStore::load(&dir.path().join("nope.toml")).unwrap().profiles.is_empty());
    }

    #[test]
    fn chain_is_outermost_first_and_rejects_loops() {
        let mut store = ProfileStore::default();
        let bastion = profile("bastion");
        let mut inner = profile("inner");
        inner.jump = Some(bastion.id);
        let mut target = profile("target");
        target.jump = Some(inner.id);
        store.upsert(bastion.clone()).unwrap();
        store.upsert(inner.clone()).unwrap();
        store.upsert(target.clone()).unwrap();
        let names: Vec<_> = store.chain(target.id).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["bastion", "inner", "target"]);

        let mut looped = bastion.clone();
        looped.jump = Some(target.id);
        assert!(store.upsert(looped).is_err());
    }

    #[test]
    fn removing_a_jump_host_detaches_dependants() {
        let mut store = ProfileStore::default();
        let bastion = profile("bastion");
        let mut target = profile("target");
        target.jump = Some(bastion.id);
        store.upsert(bastion.clone()).unwrap();
        store.upsert(target.clone()).unwrap();
        assert_eq!(store.remove(bastion.id), vec![target.id]);
        assert_eq!(store.get(target.id).unwrap().jump, None);
    }

    #[test]
    fn validation() {
        assert!(profile("ok").validate().is_ok());
        let mut p = profile("x");
        p.host = "".into();
        assert!(p.validate().is_err());
        let mut p = profile("x");
        p.auth = AuthMethod::Key { path: " ".into() };
        assert!(p.validate().is_err());
    }
}
