//! Editing a remote file safely.
//!
//! The rules, in order:
//! 1. Never overwrite a change you have not seen. Before saving, the file is
//!    read again and compared with what was opened; if someone (or something,
//!    like a config-management run) changed it, the save stops and shows both.
//! 2. Always keep the original. Every save first stores the previous contents
//!    on this machine, and optionally next to the file on the server.
//! 3. Change only the contents. The file is rewritten in place, so its owner,
//!    group, mode, hard links and ACLs are untouched, and line endings, BOM and
//!    final newline stay as they were (see [`crate::text`]).
//! 4. Check the result. After writing, the file is read back and must match.
//!
//! Files only root can write are edited through `sudo` ([`Access::Sudo`]): read
//! with `sudo cat`, written by uploading to a private temp file and having
//! `sudo` copy it into place. The password goes to sudo's stdin and nowhere else.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::sftp::{RemoteFs, file_name, join, parent};
use crate::ssh::{Connection, shell_quote};
use crate::text::{self, TextFormat};

/// Files bigger than this are not opened in the editor.
pub const MAX_EDIT_SIZE: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Sftp,
    /// Through `sudo`. `password` is `None` when sudo needs none (NOPASSWD).
    Sudo {
        password: Option<String>,
    },
}

impl Access {
    pub fn is_sudo(&self) -> bool {
        matches!(self, Access::Sudo { .. })
    }
}

/// How the editor reaches the server. Cheap to clone into background tasks.
#[derive(Clone)]
pub struct RemoteIo {
    pub fs: RemoteFs,
    pub conn: Arc<Connection>,
    pub access: Access,
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("the file changed on the server since you opened it")]
    Conflict { current: Vec<u8> },
    #[error("the file was deleted on the server since you opened it")]
    Deleted,
    #[error("the file on the server does not match what was written (a local backup was kept at {})", .backup.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "—".into()))]
    VerifyFailed { backup: Option<PathBuf> },
}

#[derive(Debug, Error)]
pub enum SudoError {
    #[error("sudo needs a password")]
    PasswordRequired,
    #[error("sudo: {0}")]
    Failed(String),
}

/// An open remote file.
#[derive(Debug, Clone)]
pub struct EditSession {
    pub path: String,
    pub format: TextFormat,
    /// The text as opened (or as last saved), `\n` line endings.
    pub base_text: String,
    base_hash: [u8; 32],
    pub size: u64,
}

impl EditSession {
    /// Treat `current` (the server's bytes now) as what was opened, e.g. after
    /// merging the user's edits onto someone else's newer version. Fails if it
    /// is not editable text.
    pub fn rebase(&mut self, current: &[u8]) -> Result<()> {
        let (text, format) = text::decode(current)?;
        self.base_text = text;
        self.format = format;
        self.base_hash = sha256(current);
        self.size = current.len() as u64;
        Ok(())
    }

    /// Does `text` differ from what is on the server (as far as we know)?
    pub fn is_modified(&self, text: &str) -> bool {
        text != self.base_text
    }
}

#[derive(Debug, Clone, Default)]
pub struct SaveOptions {
    /// Directory for local backups (normally `AppPaths::backups_dir()` plus the
    /// host). `None` skips local backups; the app always passes one.
    pub local_backup_dir: Option<PathBuf>,
    /// Also copy the original to `<file>.bak-<timestamp>` on the server.
    pub remote_backup: bool,
    /// Save even though the file changed on the server.
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct SaveReport {
    pub bytes: usize,
    pub local_backup: Option<PathBuf>,
    pub remote_backup: Option<String>,
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn timestamp() -> String {
    let fmt = time::macros::format_description!("[year][month][day]-[hour][minute][second]Z");
    time::OffsetDateTime::now_utc().format(&fmt).unwrap_or_else(|_| "unknown-time".into())
}

impl RemoteIo {
    /// Read a file's raw bytes. Returns `None` if it does not exist.
    pub async fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        match &self.access {
            Access::Sftp => {
                if !self.fs.exists(path).await? {
                    return Ok(None);
                }
                Ok(Some(self.fs.read(path, MAX_EDIT_SIZE).await?))
            }
            Access::Sudo { password } => {
                let q = shell_quote(path);
                // Print a marker for "missing" so a deleted file is not confused
                // with a permission problem.
                let cmd = format!(
                    "sudo -S -p '' -- sh -c 'if [ -e \"$1\" ]; then exec cat -- \"$1\"; else echo MALASDOH-MISSING >&2; exit 3; fi' sh {q}"
                );
                let out = self.sudo(&cmd, password.as_deref()).await?;
                if out.status == Some(3) && out.stderr_text().contains("MALASDOH-MISSING") {
                    return Ok(None);
                }
                if !out.success() {
                    return Err(sudo_error(&out).into());
                }
                if out.stdout.len() as u64 > MAX_EDIT_SIZE {
                    bail!("{path} is larger than the editor's size limit");
                }
                Ok(Some(out.stdout))
            }
        }
    }

    async fn sudo(&self, cmd: &str, password: Option<&str>) -> Result<crate::ssh::ExecOutput> {
        // `-S` reads the password from stdin. When sudo needs no password the
        // line is simply never read, because the command reads files, not stdin.
        let stdin = password.map(|p| format!("{p}\n").into_bytes()).unwrap_or_default();
        self.conn.exec(cmd, Some(&stdin)).await
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<()> {
        match &self.access {
            Access::Sftp => self.fs.overwrite(path, data).await,
            Access::Sudo { password } => {
                let tmp = self.private_temp_path().await?;
                self.fs.create_new(&tmp, data, Some(0o600)).await?;
                let cmd = format!(
                    "sudo -S -p '' -- sh -c 'cat -- \"$1\" > \"$2\"' sh {} {}",
                    shell_quote(&tmp),
                    shell_quote(path)
                );
                let result = self.sudo(&cmd, password.as_deref()).await;
                let _ = self.fs.remove(&tmp).await;
                let out = result?;
                if !out.success() {
                    return Err(sudo_error(&out).into());
                }
                Ok(())
            }
        }
    }

    async fn private_temp_path(&self) -> Result<String> {
        let home = self.fs.home().await?;
        Ok(join(&home, &format!(".malasdoh-upload-{}", uuid::Uuid::new_v4().simple())))
    }

    async fn remote_backup(&self, path: &str, original: &[u8]) -> Result<String> {
        let backup = join(&parent(path), &format!("{}.bak-{}", file_name(path), timestamp()));
        match &self.access {
            Access::Sftp => {
                let mode = self.fs.stat(path).await.ok().and_then(|m| m.permissions).map(|p| p & 0o7777);
                self.fs.create_new(&backup, original, mode).await?;
            }
            Access::Sudo { password } => {
                let cmd = format!("sudo -S -p '' -- cp -p -- {} {}", shell_quote(path), shell_quote(&backup));
                let out = self.sudo(&cmd, password.as_deref()).await?;
                if !out.success() {
                    return Err(sudo_error(&out).into());
                }
            }
        }
        Ok(backup)
    }

    /// Check that sudo works with the given password (or without one).
    pub async fn check_sudo(conn: &Connection, password: Option<&str>) -> Result<()> {
        let stdin = password.map(|p| format!("{p}\n").into_bytes()).unwrap_or_default();
        let out = conn.exec("sudo -S -p '' -- true", Some(&stdin)).await?;
        if out.success() { Ok(()) } else { Err(sudo_error(&out).into()) }
    }

    /// Open a file for editing.
    pub async fn open(&self, path: &str) -> Result<EditSession> {
        let bytes = self.read_bytes(path).await?.with_context(|| format!("{path} does not exist"))?;
        let (text, format) = text::decode(&bytes).with_context(|| format!("cannot edit {path}"))?;
        Ok(EditSession {
            path: path.to_string(),
            format,
            base_text: text,
            base_hash: sha256(&bytes),
            size: bytes.len() as u64,
        })
    }

    /// What is on the server now, compared with what was opened.
    pub async fn remote_state(&self, s: &EditSession) -> Result<RemoteState> {
        Ok(match self.read_bytes(&s.path).await? {
            None => RemoteState::Deleted,
            Some(b) if sha256(&b) == s.base_hash => RemoteState::Unchanged,
            Some(b) => RemoteState::Changed(b),
        })
    }

    /// Save `new_text`. On success the session's base becomes the new text.
    pub async fn save(&self, s: &mut EditSession, new_text: &str, opts: &SaveOptions) -> Result<SaveReport> {
        let new_bytes = text::encode(new_text, &s.format);
        let original = match self.remote_state(s).await? {
            RemoteState::Unchanged => text::encode(&s.base_text, &s.format),
            RemoteState::Changed(current) if opts.force => current,
            RemoteState::Changed(current) => return Err(SaveError::Conflict { current }.into()),
            RemoteState::Deleted => return Err(SaveError::Deleted.into()),
        };

        let local_backup = match &opts.local_backup_dir {
            Some(dir) => Some(write_local_backup(dir, &s.path, &original)?),
            None => None,
        };
        let remote_backup = if opts.remote_backup { Some(self.remote_backup(&s.path, &original).await?) } else { None };

        self.write_bytes(&s.path, &new_bytes).await?;

        let written = self.read_bytes(&s.path).await?.unwrap_or_default();
        if sha256(&written) != sha256(&new_bytes) {
            return Err(SaveError::VerifyFailed { backup: local_backup }.into());
        }
        s.base_text = text::decode(&new_bytes).map(|(t, _)| t).unwrap_or_else(|_| new_text.to_string());
        s.base_hash = sha256(&new_bytes);
        s.size = new_bytes.len() as u64;
        Ok(SaveReport { bytes: new_bytes.len(), local_backup, remote_backup })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    Unchanged,
    Changed(Vec<u8>),
    Deleted,
}

fn sudo_error(out: &crate::ssh::ExecOutput) -> SudoError {
    let msg = out.stderr_text();
    let lower = msg.to_lowercase();
    if lower.contains("password is required") || lower.contains("incorrect password") || lower.contains("try again") {
        SudoError::PasswordRequired
    } else if msg.is_empty() {
        SudoError::Failed(format!(
            "command exited with status {}",
            out.status.map(|s| s.to_string()).unwrap_or_else(|| "unknown".into())
        ))
    } else {
        SudoError::Failed(msg)
    }
}

/// Where local backups of `remote_path` live under a host's backup directory.
pub fn backup_dir_for(host_dir: &Path, remote_path: &str) -> PathBuf {
    let mut dir = host_dir.to_path_buf();
    for part in parent(remote_path).split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..") {
        dir.push(sanitize(part));
    }
    dir
}

/// Directory name for a host's backups: `host` or `host_2222`.
pub fn host_backup_dir(backups_root: &Path, host: &str, port: u16) -> PathBuf {
    let name = if port == 22 { sanitize(host) } else { format!("{}_{port}", sanitize(host)) };
    backups_root.join(name)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || c.is_control() { '_' } else { c })
        .collect()
}

fn write_local_backup(host_dir: &Path, remote_path: &str, original: &[u8]) -> Result<PathBuf> {
    let dir = backup_dir_for(host_dir, remote_path);
    crate::paths::create_private_dir(&dir).with_context(|| format!("creating backup folder {}", dir.display()))?;
    let stem = format!("{}.{}", sanitize(file_name(remote_path)), timestamp());
    let mut path = dir.join(&stem);
    // Two saves in the same second must not overwrite each other's backup.
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}"));
        n += 1;
    }
    crate::paths::write_private(&path, original).with_context(|| format!("writing backup {}", path.display()))?;
    Ok(path)
}

/// Local backups of a file, newest first.
pub fn list_local_backups(host_dir: &Path, remote_path: &str) -> Vec<PathBuf> {
    let dir = backup_dir_for(host_dir, remote_path);
    let prefix = format!("{}.", sanitize(file_name(remote_path)));
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.reverse();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_paths_mirror_the_remote_tree() {
        let root = Path::new("/b");
        let host = host_backup_dir(root, "web:1", 2222);
        assert_eq!(host, Path::new("/b/web_1_2222"));
        assert_eq!(backup_dir_for(&host, "/etc/nginx/nginx.conf"), Path::new("/b/web_1_2222/etc/nginx"));
        assert_eq!(backup_dir_for(&host, "../../x"), Path::new("/b/web_1_2222"));
    }

    #[test]
    fn local_backups_are_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_local_backup(dir.path(), "/etc/hosts", b"one").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = write_local_backup(dir.path(), "/etc/hosts", b"two").unwrap();
        write_local_backup(dir.path(), "/etc/hostname", b"other").unwrap();
        assert_eq!(list_local_backups(dir.path(), "/etc/hosts"), vec![b.clone(), a]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Backups hold config files and secrets: owner-only.
            assert_eq!(std::fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(b.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}
