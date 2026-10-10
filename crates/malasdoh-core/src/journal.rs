//! The change journal: one line per file saved from MalasDoh, with when, on
//! which server, what changed (as a patch) and how the config check went.
//!
//! It is a JSON Lines file in the data directory, readable only by the user,
//! because the patches can contain secrets from config files. Appending is
//! the only write; when the file passes [`ROTATE_AT`] it is moved aside to
//! `journal.1.jsonl` (replacing an older one) and a new one is started.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Start a new file once the journal is this big.
pub const ROTATE_AT: u64 = 20 * 1024 * 1024;
/// Patches longer than this are cut (the backup still has the whole file).
pub const MAX_PATCH: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Written and verified.
    Saved,
    /// Written, failed its check, and the original was put back.
    RolledBack,
    /// Written, failed its check, and putting the original back failed too:
    /// the server may have the new, failing version.
    RollbackFailed,
    /// Written, but reading it back did not match: the server's contents are
    /// unknown.
    VerifyFailed,
}

impl Outcome {
    /// The server may not be in a known-good state.
    pub fn is_alarming(self) -> bool {
        matches!(self, Outcome::RollbackFailed | Outcome::VerifyFailed)
    }

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Saved => "saved",
            Outcome::RolledBack => "rolled back",
            Outcome::RollbackFailed => "check failed, ROLLBACK FAILED",
            Outcome::VerifyFailed => "written, read-back DID NOT MATCH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Seconds since the epoch (UTC).
    pub time: i64,
    /// The saved connection's name.
    pub connection: String,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub sudo: bool,
    pub added: usize,
    pub removed: usize,
    /// e.g. "nginx: passed", "sudoers: failed".
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub local_backup: Option<PathBuf>,
    #[serde(default)]
    pub remote_backup: Option<String>,
    /// `diff -u` of the file before and after.
    pub patch: String,
    #[serde(default)]
    pub truncated: bool,
}

impl Entry {
    /// Fill in the patch and line counts from the old and new text.
    pub fn with_change(mut self, old: &str, new: &str) -> Self {
        let d = crate::diff::diff(old, new, 0);
        self.added = d.added;
        self.removed = d.removed;
        let mut patch = crate::diff::unified(old, new, &self.path);
        if patch.len() > MAX_PATCH {
            let mut cut = MAX_PATCH;
            while !patch.is_char_boundary(cut) {
                cut -= 1;
            }
            patch.truncate(cut);
            self.truncated = true;
        }
        self.patch = patch;
        self
    }

    /// Does `query` (lowercased words) match this entry's connection, host,
    /// path, user or patch? Every word must match somewhere.
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!(
            "{}\n{}@{}:{}\n{}\n{}\n{}",
            self.connection,
            self.user,
            self.host,
            self.port,
            self.path,
            self.outcome.label(),
            self.check.as_deref().unwrap_or("")
        )
        .to_lowercase();
        query.split_whitespace().all(|w| {
            let w = w.to_lowercase();
            hay.contains(&w) || self.patch.to_lowercase().contains(&w)
        })
    }
}

/// The journal entry for a save attempt, filled in from how it went;
/// `None` when nothing reached the server (a conflict, a check that refused
/// it before writing, a connection error). `base` names the connection and
/// file; `after` is the session's text after the attempt (after a rollback
/// that is what is back on the server) and `text` what was being saved.
pub fn entry_for(
    base: Entry,
    saved: &anyhow::Result<crate::edit::SaveReport>,
    after: &str,
    text: &str,
) -> Option<Entry> {
    use crate::edit::SaveError;
    use crate::validate::CheckOutcome;
    let e = match saved {
        Ok(report) => Entry {
            check: report.check.as_ref().map(|c| {
                let how = match c.outcome {
                    CheckOutcome::Passed { .. } => "passed",
                    CheckOutcome::Unavailable { .. } => "not installed",
                };
                format!("{}: {how}", c.rule)
            }),
            local_backup: report.local_backup.clone(),
            remote_backup: report.remote_backup.clone(),
            ..base
        }
        .with_change(&report.previous, text),
        Err(e) => match e.downcast_ref::<SaveError>() {
            Some(SaveError::ValidationFailed { rule, rolled_back: true, .. }) => {
                Entry { outcome: Outcome::RolledBack, check: Some(format!("{rule}: failed")), ..base }
                    .with_change(after, text)
            }
            Some(SaveError::RollbackFailed { rule, backup, .. }) => Entry {
                outcome: Outcome::RollbackFailed,
                check: Some(format!("{rule}: failed")),
                local_backup: backup.clone(),
                ..base
            }
            .with_change(after, text),
            Some(SaveError::VerifyFailed { backup }) => {
                Entry { outcome: Outcome::VerifyFailed, local_backup: backup.clone(), ..base }.with_change(after, text)
            }
            _ => return None,
        },
    };
    Some(Entry { time: now(), ..e })
}

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// "2026-10-10 11:23 UTC".
pub fn format_time(t: i64) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute] UTC");
    time::OffsetDateTime::from_unix_timestamp(t).ok().and_then(|d| d.format(&fmt).ok()).unwrap_or_else(|| t.to_string())
}

#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotated(&self) -> PathBuf {
        self.path.with_extension("1.jsonl")
    }

    pub fn append(&self, entry: &Entry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            crate::paths::create_private_dir(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= ROTATE_AT) {
            std::fs::rename(&self.path, self.rotated()).context("rotating the change journal")?;
        }
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path).with_context(|| format!("opening {}", self.path.display()))?;
        // One write per line, so a crash leaves at most one partial line.
        f.write_all(line.as_bytes()).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Every entry, newest first. Lines that do not parse (a partial line
    /// after a crash, or from a newer version) are skipped.
    pub fn read(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for p in [self.rotated(), self.path.clone()] {
            let f = match std::fs::File::open(&p) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
            };
            for line in std::io::BufReader::new(f).lines() {
                let line = line?;
                if let Ok(e) = serde_json::from_str::<Entry>(&line) {
                    out.push(e);
                }
            }
        }
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            time: 1_760_000_000,
            connection: "web-1".into(),
            user: "root".into(),
            host: "10.0.0.5".into(),
            port: 22,
            path: path.into(),
            outcome: Outcome::Saved,
            sudo: false,
            added: 0,
            removed: 0,
            check: Some("nginx: passed".into()),
            local_backup: None,
            remote_backup: None,
            patch: String::new(),
            truncated: false,
        }
    }

    #[test]
    fn appends_and_reads_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path().join("j").join("journal.jsonl"));
        assert!(j.read().unwrap().is_empty());
        j.append(&entry("/etc/a").with_change("x\n", "y\n")).unwrap();
        j.append(&entry("/etc/b")).unwrap();
        // A torn last line is ignored.
        std::fs::OpenOptions::new().append(true).open(j.path()).unwrap().write_all(b"{\"time\":1").unwrap();
        let all = j.read().unwrap();
        assert_eq!(all.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/etc/b", "/etc/a"]);
        assert_eq!((all[1].added, all[1].removed), (1, 1));
        assert!(all[1].patch.contains("-x\n+y\n"), "{}", all[1].patch);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(j.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn rotates_when_big() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path().join("journal.jsonl"));
        j.append(&entry("/old")).unwrap();
        let f = std::fs::OpenOptions::new().append(true).open(j.path()).unwrap();
        f.set_len(ROTATE_AT + 1).unwrap(); // pad with NULs: one unparsable line
        j.append(&entry("/new")).unwrap();
        assert!(dir.path().join("journal.1.jsonl").exists());
        let all = j.read().unwrap();
        assert_eq!(all.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/new", "/old"]);
    }

    #[test]
    fn search_and_truncation() {
        let e = entry("/etc/nginx/nginx.conf").with_change("listen 80;\n", "listen 8080;\n");
        assert!(e.matches("web-1 nginx"));
        assert!(e.matches("8080"), "patch text is searchable");
        assert!(!e.matches("db-2"));
        let big = "é".repeat(MAX_PATCH);
        let e = entry("/big").with_change("", &big);
        assert!(e.truncated && e.patch.len() <= MAX_PATCH);
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
    }
}
