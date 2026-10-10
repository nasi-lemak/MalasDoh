//! What was open last time: connections, their folders, open files and
//! terminals, so the next start can pick up where you left off.
//!
//! Only paths are kept, never file contents or unsaved edits.

use std::path::{Path, PathBuf};

use malasdoh_core::profile::ProfileId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastSession {
    pub tabs: Vec<Tab>,
    /// Index into `tabs` of the tab that was in front.
    pub active: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub profile: ProfileId,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub files: Vec<OpenFile>,
    #[serde(default = "one")]
    pub terminals: usize,
    /// The local pane's folder, if it was open.
    #[serde(default)]
    pub local_dir: Option<PathBuf>,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenFile {
    pub path: String,
    #[serde(default)]
    pub sudo: bool,
}

impl LastSession {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Drop tabs whose connection no longer exists.
    pub fn retain_profiles(&mut self, exists: impl Fn(ProfileId) -> bool) {
        let active = self.active.and_then(|i| self.tabs.get(i)).map(|t| t.profile);
        self.tabs.retain(|t| exists(t.profile));
        self.active = active.and_then(|p| self.tabs.iter().position(|t| t.profile == p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing_profiles() {
        let (a, b) = (ProfileId::new(), ProfileId::new());
        let mut s = LastSession {
            tabs: vec![
                Tab { profile: a, dir: "/srv".into(), files: vec![], terminals: 2, local_dir: None },
                Tab {
                    profile: b,
                    dir: "/etc".into(),
                    files: vec![OpenFile { path: "/etc/hosts".into(), sudo: true }],
                    terminals: 1,
                    local_dir: Some("/home/me".into()),
                },
            ],
            active: Some(1),
        };
        let back: LastSession = toml::from_str(&s.to_toml()).unwrap();
        assert_eq!(back, s);
        s.retain_profiles(|p| p == b);
        assert_eq!((s.tabs.len(), s.active), (1, Some(0)));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(LastSession::load(&dir.path().join("missing.toml")), LastSession::default());
    }
}
