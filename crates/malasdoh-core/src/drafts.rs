//! Unsaved edits, written to disk while a file is modified, so a crash, a
//! power cut or a forced quit loses nothing. Each draft keeps the version it
//! was based on, so restoring it onto a file that changed meanwhile goes
//! through the usual conflict and three-way-merge screen.
//!
//! Drafts live in the data directory, readable only by the user, and are
//! removed when the file is saved or the edits are discarded.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::edit::EditSession;
use crate::profile::ProfileId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub profile: ProfileId,
    pub sudo: bool,
    /// The file as it was opened, with the path.
    pub session: EditSession,
    /// The unsaved text.
    pub text: String,
    /// Seconds since the epoch (UTC).
    pub saved: i64,
}

#[derive(Debug, Clone)]
pub struct Drafts {
    dir: PathBuf,
}

impl Drafts {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn file(&self, profile: ProfileId, path: &str, sudo: bool) -> PathBuf {
        let key = format!("{profile}\n{path}\n{sudo}");
        let hash = hex::encode(&Sha256::digest(key.as_bytes())[..16]);
        self.dir.join(format!("{hash}.json"))
    }

    pub fn save(&self, d: &Draft) -> Result<()> {
        let path = self.file(d.profile, &d.session.path, d.sudo);
        crate::paths::write_private_atomic(&path, &serde_json::to_vec(d)?)
            .with_context(|| format!("saving the draft of {}", d.session.path))
    }

    pub fn remove(&self, profile: ProfileId, path: &str, sudo: bool) {
        let _ = std::fs::remove_file(self.file(profile, path, sudo));
    }

    /// Drafts for one connection, oldest first. Unreadable ones are skipped.
    pub fn for_profile(&self, profile: ProfileId) -> Vec<Draft> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut v: Vec<Draft> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice::<Draft>(&std::fs::read(e.path()).ok()?).ok())
            .filter(|d| d.profile == profile)
            .collect();
        v.sort_by_key(|d| d.saved);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{LineEnding, TextFormat};

    fn session(path: &str) -> EditSession {
        let fmt = TextFormat { line_ending: LineEnding::CrLf, bom: false };
        let mut s =
            EditSession { path: path.into(), format: fmt, base_text: String::new(), base_hash: [0; 32], size: 0 };
        s.rebase(b"listen 80\r\n").unwrap();
        s
    }

    #[test]
    fn save_list_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let drafts = Drafts::new(dir.path().join("drafts"));
        let (a, b) = (ProfileId::new(), ProfileId::new());
        let d =
            Draft { profile: a, sudo: false, session: session("/etc/x.conf"), text: "listen 81\n".into(), saved: 2 };
        drafts.save(&d).unwrap();
        drafts.save(&Draft { saved: 3, text: "listen 82\n".into(), ..d.clone() }).unwrap();
        drafts.save(&Draft { sudo: true, saved: 1, ..d.clone() }).unwrap();
        drafts.save(&Draft { profile: b, ..d.clone() }).unwrap();

        let found = drafts.for_profile(a);
        assert_eq!(found.len(), 2, "one per file and access, the newest kept");
        assert_eq!((found[0].sudo, found[1].text.as_str()), (true, "listen 82\n"));
        assert_eq!(found[1].session.base_text, "listen 80\n");
        assert!(!found[1].session.is_modified("listen 80\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::read_dir(dir.path().join("drafts")).unwrap().next().unwrap().unwrap().path();
            assert_eq!(std::fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        drafts.remove(a, "/etc/x.conf", false);
        assert_eq!(drafts.for_profile(a).len(), 1);
    }
}
