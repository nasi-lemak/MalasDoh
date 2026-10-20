//! Before a transfer replaces files: which ones it would replace, whether
//! their contents really differ (by SHA-256, computed where each file is),
//! both versions as text to compare, and backups of the server's copies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::sftp::{EntryKind, RemoteFs, file_name, human_size, parent};
use crate::text::{self, LineEnding};
use crate::transfer::{Checker, FileTask, Job, Way, local_sha256};

/// Files up to this size are compared by checksum before asking.
pub const SAME_CHECK_MAX: u64 = 64 * 1024 * 1024;
/// Files up to this size can be shown side by side.
pub const DIFF_MAX: u64 = 1024 * 1024;
/// Server files up to this size are backed up before an upload replaces them.
pub const BACKUP_MAX: u64 = 20 * 1024 * 1024;

/// A file of a job that already exists where it is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// Its index in [`Job::files`].
    pub index: usize,
    /// The path shown: relative to the folder the transfer goes into.
    pub rel: String,
    /// The copy that would be replaced.
    pub old_size: u64,
    pub old_mtime: Option<u64>,
    /// The copy that would replace it.
    pub new_size: u64,
    pub new_mtime: Option<u64>,
    /// Same contents (`Some(true)`), different (`Some(false)`), or not
    /// compared (too big, or no checksum tool on the server).
    pub same: Option<bool>,
}

impl Replaced {
    /// Small enough on both sides to be shown side by side.
    pub fn can_show(&self) -> bool {
        self.old_size <= DIFF_MAX && self.new_size <= DIFF_MAX
    }
}

/// The files of `job` that already exist at their destination; `base` is
/// the folder the transfer goes into (for the paths shown). Folders that
/// would be replaced by a file are not listed: that copy fails by itself.
pub async fn find(fs: &RemoteFs, job: &Job, base: &str, checker: Option<&Checker>) -> Vec<Replaced> {
    let mut found = Vec::new();
    match job.way {
        Way::Up => {
            // Each folder's files by name: kind, size, time (None: no such folder).
            type Listing = Option<HashMap<String, (EntryKind, u64, Option<u32>)>>;
            let mut listings: HashMap<String, Listing> = HashMap::new();
            for (i, f) in job.files.iter().enumerate() {
                let dir = parent(&f.remote);
                if !listings.contains_key(&dir) {
                    let l = fs
                        .list(&dir)
                        .await
                        .ok()
                        .map(|l| l.into_iter().map(|e| (e.name, (e.kind, e.size, e.mtime))).collect());
                    listings.insert(dir.clone(), l);
                }
                let Some(Some(names)) = listings.get(&dir) else { continue };
                match names.get(file_name(&f.remote)) {
                    Some((EntryKind::Dir, ..)) | None => {}
                    Some((_, size, mtime)) => found.push(Replaced {
                        index: i,
                        rel: relative(&f.remote, base),
                        old_size: *size,
                        old_mtime: mtime.map(u64::from),
                        new_size: f.size,
                        new_mtime: f.mtime,
                        same: None,
                    }),
                }
            }
        }
        Way::Down => {
            for (i, f) in job.files.iter().enumerate() {
                let Ok(m) = std::fs::metadata(&f.local) else { continue };
                if m.is_dir() {
                    continue;
                }
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                found.push(Replaced {
                    index: i,
                    rel: f.local.strip_prefix(base).unwrap_or(&f.local).to_string_lossy().replace('\\', "/"),
                    old_size: m.len(),
                    old_mtime: mtime.map(|d| d.as_secs()),
                    new_size: f.size,
                    new_mtime: f.mtime,
                    same: None,
                });
            }
        }
    }
    compare(job, &mut found, checker).await;
    found
}

fn relative(path: &str, base: &str) -> String {
    let base = base.trim_end_matches('/');
    path.strip_prefix(base).map(|r| r.trim_start_matches('/')).filter(|r| !r.is_empty()).unwrap_or(path).to_string()
}

/// Fill in `same`: different sizes differ; equal sizes are compared by
/// SHA-256, the server's copies hashed on the server.
async fn compare(job: &Job, found: &mut [Replaced], checker: Option<&Checker>) {
    let mut ask = Vec::new();
    for r in found.iter_mut() {
        if r.old_size != r.new_size {
            r.same = Some(false);
        } else if r.old_size == 0 {
            r.same = Some(true);
        } else if r.old_size <= SAME_CHECK_MAX {
            ask.push(r.index);
        }
    }
    if ask.is_empty() {
        return;
    }
    let Some(checker) = checker else { return };
    let remote: Vec<String> = ask.iter().map(|&i| job.files[i].remote.clone()).collect();
    let Ok(remote) = checker.sha256(&remote).await else { return };
    for (&i, theirs) in ask.iter().zip(remote) {
        let Some(theirs) = theirs else { continue };
        let Ok(ours) = local_sha256(&job.files[i].local).await else { continue };
        if let Some(r) = found.iter_mut().find(|r| r.index == i) {
            r.same = Some(ours == theirs);
        }
    }
}

/// Both versions of a replaced file, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texts {
    /// The copy that would be replaced, and the one replacing it.
    pub old: String,
    pub new: String,
    /// What differs that the text does not show (line endings, a BOM).
    pub note: Option<String>,
}

/// Reads both versions of a file of a job going `way`, for showing side by
/// side. Fails for files that are not text or are over [`DIFF_MAX`].
pub async fn texts(fs: &RemoteFs, way: Way, f: &FileTask) -> Result<Texts> {
    let remote = fs.read(&f.remote, DIFF_MAX).await?;
    let local = {
        let path = f.local.clone();
        let m = std::fs::metadata(&path)?;
        if m.len() > DIFF_MAX {
            bail!("{} is {}, too big to show here", path.display(), human_size(m.len()));
        }
        tokio::task::spawn_blocking(move || std::fs::read(path)).await??
    };
    let (old, new) = match way {
        Way::Up => (remote, local),
        Way::Down => (local, remote),
    };
    let (old_text, old_fmt) = text::decode(&old).map_err(|e| anyhow::anyhow!("the current copy: {e}"))?;
    let (new_text, new_fmt) = text::decode(&new).map_err(|e| anyhow::anyhow!("the new copy: {e}"))?;
    let ending = |e: LineEnding| match e {
        LineEnding::Lf => "LF (Unix)",
        LineEnding::CrLf => "CRLF (Windows)",
    };
    let mut notes = Vec::new();
    if old_fmt.line_ending != new_fmt.line_ending {
        notes.push(format!(
            "line endings change from {} to {}",
            ending(old_fmt.line_ending),
            ending(new_fmt.line_ending)
        ));
    }
    if old_fmt.bom != new_fmt.bom {
        notes.push(if new_fmt.bom {
            "a byte-order mark is added".into()
        } else {
            "its byte-order mark is removed".into()
        });
    }
    let note = (!notes.is_empty()).then(|| {
        let mut s = notes.join("; ");
        s[..1].make_ascii_uppercase();
        s + "."
    });
    Ok(Texts { old: old_text, new: new_text, note })
}

/// What [`back_up`] did.
#[derive(Debug, Default)]
pub struct Backups {
    /// Job index and where its backup is.
    pub saved: Vec<(usize, PathBuf)>,
    /// Job index and why it could not be backed up.
    pub failed: Vec<(usize, String)>,
    /// Over [`BACKUP_MAX`]: replaced without a backup.
    pub too_big: Vec<usize>,
}

/// Backs up the server's copies of `replaced` (an upload's) under
/// `host_dir`, where the editor keeps its backups.
pub async fn back_up(fs: &RemoteFs, job: &Job, replaced: &[Replaced], host_dir: &Path) -> Backups {
    let mut out = Backups::default();
    for r in replaced {
        match back_up_one(fs, &job.files[r.index].remote, r.old_size, host_dir).await {
            BackedUp::Saved(p) => out.saved.push((r.index, p)),
            BackedUp::Failed(e) => out.failed.push((r.index, e)),
            BackedUp::TooBig => out.too_big.push(r.index),
        }
    }
    out
}

/// How backing up one server file went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackedUp {
    Saved(PathBuf),
    Failed(String),
    /// Over [`BACKUP_MAX`]: not backed up.
    TooBig,
}

/// Backs up the server file `path` (`size` bytes) under `host_dir`.
pub async fn back_up_one(fs: &RemoteFs, path: &str, size: u64, host_dir: &Path) -> BackedUp {
    if size > BACKUP_MAX {
        return BackedUp::TooBig;
    }
    let saved = async {
        let data = fs.read(path, BACKUP_MAX).await?;
        let (dir, path) = (host_dir.to_path_buf(), path.to_string());
        tokio::task::spawn_blocking(move || crate::edit::write_local_backup(&dir, &path, &data)).await?
    }
    .await;
    match saved {
        Ok(p) => BackedUp::Saved(p),
        Err(e) => BackedUp::Failed(format!("{e:#}")),
    }
}

/// The change journal's entry for a server file an upload replaced:
/// `base` names the connection and file, `backup` holds the old contents
/// and `local` the new. Text files up to [`DIFF_MAX`] get their changes.
pub fn journal_entry(base: crate::journal::Entry, backup: &Path, local: &Path) -> crate::journal::Entry {
    let read = |p: &Path| -> Option<String> {
        let m = std::fs::metadata(p).ok()?;
        if m.len() > DIFF_MAX {
            return None;
        }
        text::decode(&std::fs::read(p).ok()?).ok().map(|(t, _)| t)
    };
    let e = crate::journal::Entry {
        time: crate::journal::now(),
        outcome: crate::journal::Outcome::Uploaded,
        local_backup: Some(backup.to_path_buf()),
        ..base
    };
    match (read(backup), read(local)) {
        (Some(old), Some(new)) => e.with_change(&old, &new),
        _ => crate::journal::Entry {
            patch: format!("(not text, or over {}: no line changes recorded)", human_size(DIFF_MAX)),
            ..e
        },
    }
}

/// Takes files out of `job` (by index), noting each with its reason in
/// [`Job::skipped`].
pub fn leave_out(job: &mut Job, drop: &[(usize, String)]) {
    if drop.is_empty() {
        return;
    }
    let reasons: HashMap<usize, &str> = drop.iter().map(|(i, r)| (*i, r.as_str())).collect();
    let files = std::mem::take(&mut job.files);
    for (i, f) in files.into_iter().enumerate() {
        match reasons.get(&i) {
            Some(why) => {
                let name = match job.way {
                    Way::Up => f.remote.clone(),
                    Way::Down => f.local.display().to_string(),
                };
                job.skipped.push(format!("{name} ({why})"));
            }
            None => job.files.push(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_out_keeps_the_rest_in_order() {
        let mut job = crate::transfer::job_of(
            Way::Up,
            ["a", "b", "c", "d"].iter().map(|n| (PathBuf::from(format!("/l/{n}")), format!("/r/{n}"), 1)).collect(),
        );
        leave_out(&mut job, &[(1, "same content".into()), (3, "kept".into())]);
        let left: Vec<&str> = job.files.iter().map(|f| f.remote.as_str()).collect();
        assert_eq!(left, ["/r/a", "/r/c"]);
        assert_eq!(job.skipped, ["/r/b (same content)", "/r/d (kept)"]);
    }

    #[test]
    fn paths_are_shown_from_the_destination() {
        assert_eq!(relative("/etc/nginx/sites/a.conf", "/etc/nginx"), "sites/a.conf");
        assert_eq!(relative("/etc/nginx/sites/a.conf", "/etc/nginx/"), "sites/a.conf");
        assert_eq!(relative("/a.conf", "/"), "a.conf");
        assert_eq!(relative("/x/a.conf", "/y"), "/x/a.conf");
    }
}
