//! Compare a local folder with a remote one and bring them in line, with a
//! plan the user sees (and can trim) before anything is copied or deleted.
//!
//! Files are compared by size and modification time (to the second, with a
//! two-second allowance for FAT and SMB). After copying, the destination's
//! time is set to the source's, so the next comparison sees them as equal.
//! Symlinks and special files are reported and skipped, never followed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sftp::{EntryKind, Progress, RemoteFs, join, local_name};
use crate::transfer::{self, Checker, Ended, Status, TransferOptions, Way};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Direction {
    /// Make the server match this computer.
    #[default]
    Upload,
    /// Make this computer match the server.
    Download,
    /// Copy whichever side is newer, both ways. Never deletes.
    Both,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::Upload => "Local -> server",
            Direction::Download => "Server -> local",
            Direction::Both => "Both ways (newer wins)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncOptions {
    pub direction: Direction,
    /// Delete files on the destination that the source does not have.
    pub delete_extra: bool,
    /// Names to leave alone, e.g. `.git`, `node_modules`, `*.log`.
    pub exclude: Vec<String>,
    /// When size and time match, also compare the bytes. Slower (each such
    /// file is read on both sides) but catches same-size edits made within
    /// the same couple of seconds.
    pub compare_contents: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Upload,
            delete_extra: false,
            exclude: vec![".git".into(), ".DS_Store".into(), "Thumbs.db".into()],
            compare_contents: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    MkdirRemote,
    MkdirLocal,
    Upload { size: u64 },
    Download { size: u64 },
    DeleteRemote { dir: bool },
    DeleteLocal { dir: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    /// `/`-separated path relative to both roots.
    pub rel: String,
    pub action: Action,
    pub reason: &'static str,
    /// The user can untick items before running the plan.
    pub enabled: bool,
    /// For a copy over a file that is already there: that file's size.
    pub replaces: Option<u64>,
}

impl PlanItem {
    /// The size of what an upload or download copies.
    pub fn action_size(&self) -> u64 {
        match self.action {
            Action::Upload { size } | Action::Download { size } => size,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub unchanged: usize,
    /// Paths skipped (symlinks, special files, unsafe names).
    pub skipped: Vec<String>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.enabled)
            .map(|i| match i.action {
                Action::Upload { size } | Action::Download { size } => size,
                _ => 0,
            })
            .sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct Meta {
    dir: bool,
    size: u64,
    mtime: u64,
}

fn excluded(name: &str, exclude: &[String]) -> bool {
    exclude.iter().any(|p| crate::validate::matches_path(p, name))
}

fn scan_local(
    root: &Path,
    rel: &str,
    exclude: &[String],
    out: &mut BTreeMap<String, Meta>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if excluded(&name, exclude) {
            continue;
        }
        let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let ft = e.file_type()?;
        if ft.is_dir() {
            out.insert(path.clone(), Meta { dir: true, size: 0, mtime: 0 });
            scan_local(root, &path, exclude, out, skipped)?;
        } else if ft.is_file() {
            let m = e.metadata()?;
            let mtime =
                m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            out.insert(path, Meta { dir: false, size: m.len(), mtime });
        } else {
            skipped.push(path);
        }
    }
    Ok(())
}

async fn scan_remote(
    fs: &RemoteFs,
    root: &str,
    rel: &str,
    exclude: &[String],
    out: &mut BTreeMap<String, Meta>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let dir = if rel.is_empty() { root.to_string() } else { join(root, rel) };
    for e in fs.list(&dir).await? {
        if excluded(&e.name, exclude) {
            continue;
        }
        let path = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
        if local_name(&e.name).is_err() {
            skipped.push(path);
            continue;
        }
        match e.kind {
            EntryKind::Dir => {
                out.insert(path.clone(), Meta { dir: true, size: 0, mtime: 0 });
                Box::pin(scan_remote(fs, root, &path, exclude, out, skipped)).await?;
            }
            EntryKind::File => {
                out.insert(path, Meta { dir: false, size: e.size, mtime: e.mtime.unwrap_or(0) as u64 });
            }
            _ => skipped.push(path),
        }
    }
    Ok(())
}

async fn same_bytes(fs: &RemoteFs, local: &Path, remote: &str) -> Result<bool> {
    let here = std::fs::read(local).with_context(|| format!("reading {}", local.display()))?;
    let there = fs.read(remote, u64::MAX).await?;
    Ok(here == there)
}

/// Which of `rels` differ in contents. By SHA-256 computed on the server
/// when possible (nothing is downloaded); otherwise by reading both.
async fn differing(
    fs: &RemoteFs,
    local_root: &Path,
    remote_root: &str,
    rels: &[String],
    checker: Option<&Checker>,
) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let remote: Vec<String> = rels.iter().map(|r| join(remote_root, r)).collect();
    let hashes = match checker {
        Some(c) if !rels.is_empty() => c.sha256(&remote).await.unwrap_or_else(|_| vec![None; rels.len()]),
        _ => vec![None; rels.len()],
    };
    for ((rel, path), theirs) in rels.iter().zip(&remote).zip(hashes) {
        let local = local_root.join(rel);
        let same = match theirs {
            Some(theirs) => transfer::local_sha256(&local).await? == theirs,
            None => same_bytes(fs, &local, path).await?,
        };
        if !same {
            out.insert(rel.clone());
        }
    }
    Ok(out)
}

fn same(a: &Meta, b: &Meta) -> bool {
    a.size == b.size && a.mtime.abs_diff(b.mtime) <= 2
}

/// Work out what to do. Nothing is changed.
pub async fn plan(
    fs: &RemoteFs,
    local_root: &Path,
    remote_root: &str,
    opts: &SyncOptions,
    checker: Option<&Checker>,
) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut local = BTreeMap::new();
    let mut remote = BTreeMap::new();
    scan_local(local_root, "", &opts.exclude, &mut local, &mut plan.skipped)?;
    scan_remote(fs, remote_root, "", &opts.exclude, &mut remote, &mut plan.skipped).await?;
    let item =
        |rel: &str, action, reason| PlanItem { rel: rel.to_string(), action, reason, enabled: true, replaces: None };
    let over = |it: PlanItem, size: u64| PlanItem { replaces: Some(size), ..it };
    let candidates: Vec<String> = if opts.compare_contents {
        local
            .iter()
            .filter(|(rel, l)| !l.dir && remote.get(*rel).is_some_and(|r| !r.dir && same(l, r)))
            .map(|(rel, _)| rel.clone())
            .collect()
    } else {
        Vec::new()
    };
    let differ = differing(fs, local_root, remote_root, &candidates, checker).await?;

    let up = matches!(opts.direction, Direction::Upload | Direction::Both);
    let down = matches!(opts.direction, Direction::Download | Direction::Both);
    let deletes = opts.delete_extra && opts.direction != Direction::Both;

    for (rel, l) in &local {
        match remote.get(rel) {
            None if up => plan.items.push(if l.dir {
                item(rel, Action::MkdirRemote, "only here")
            } else {
                item(rel, Action::Upload { size: l.size }, "only here")
            }),
            None if deletes => plan.items.push(item(rel, Action::DeleteLocal { dir: l.dir }, "not on the server")),
            None => {}
            Some(r) if l.dir && r.dir => {}
            Some(r) if l.dir != r.dir => {
                plan.skipped.push(format!("{rel} (a file on one side, a folder on the other)"))
            }
            Some(r) if same(l, r) && !opts.compare_contents => plan.unchanged += 1,
            Some(r) if same(l, r) && !differ.contains(rel) => plan.unchanged += 1,
            Some(r) if same(l, r) => plan.items.push(match opts.direction {
                Direction::Download => over(item(rel, Action::Download { size: r.size }, "contents differ"), l.size),
                Direction::Upload => over(item(rel, Action::Upload { size: l.size }, "contents differ"), r.size),
                Direction::Both => {
                    plan.skipped.push(format!("{rel} (contents differ, same time on both sides)"));
                    continue;
                }
            }),
            Some(r) => {
                let local_newer = l.mtime > r.mtime;
                match opts.direction {
                    Direction::Upload => plan.items.push(over(
                        item(
                            rel,
                            Action::Upload { size: l.size },
                            if local_newer { "newer here" } else { "different here" },
                        ),
                        r.size,
                    )),
                    Direction::Download => plan.items.push(over(
                        item(
                            rel,
                            Action::Download { size: r.size },
                            if local_newer { "different on the server" } else { "newer on the server" },
                        ),
                        l.size,
                    )),
                    Direction::Both if local_newer => {
                        plan.items.push(over(item(rel, Action::Upload { size: l.size }, "newer here"), r.size))
                    }
                    Direction::Both if r.mtime > l.mtime => plan
                        .items
                        .push(over(item(rel, Action::Download { size: r.size }, "newer on the server"), l.size)),
                    // Same time, different size: no safe winner.
                    Direction::Both => plan.skipped.push(format!("{rel} (differs, same time on both sides)")),
                }
            }
        }
    }
    for (rel, r) in &remote {
        if local.contains_key(rel) {
            continue;
        }
        if down {
            plan.items.push(if r.dir {
                item(rel, Action::MkdirLocal, "only on the server")
            } else {
                item(rel, Action::Download { size: r.size }, "only on the server")
            });
        } else if deletes {
            let it = item(rel, Action::DeleteRemote { dir: r.dir }, "not here");
            plan.items.push(if r.dir { it } else { over(it, r.size) });
        }
    }
    // A deleted folder takes its contents with it: list only the top one.
    let deleted_dirs: Vec<String> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::DeleteLocal { dir: true } | Action::DeleteRemote { dir: true }))
        .map(|i| format!("{}/", i.rel))
        .collect();
    plan.items.retain(|i| {
        !matches!(i.action, Action::DeleteLocal { .. } | Action::DeleteRemote { .. })
            || !deleted_dirs.iter().any(|d| i.rel.starts_with(d.as_str()))
    });
    Ok(plan)
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub done: usize,
    pub failed: Vec<(String, String)>,
    /// Copies whose checksums were compared and matched.
    pub verified: usize,
    /// Why copies were not compared, if they were not.
    pub unverified: Option<String>,
    /// Server files backed up before being replaced or deleted: the path
    /// relative to the roots, the backup, and whether an upload replaced it.
    pub backups: Vec<(String, PathBuf, bool)>,
    /// Replaced or deleted without a backup, being too big.
    pub too_big: Vec<String>,
}

/// Carry out the enabled items: folders first, then copies (several at
/// once, through [`transfer::run`], so times are kept and copies are
/// checked), then deletions (deepest first). A failing item is recorded and
/// the rest continue. With `backups` (a host's backup folder), server files
/// are backed up before they are replaced or deleted; one that cannot be
/// is left alone.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    fs: &RemoteFs,
    local_root: &Path,
    remote_root: &str,
    plan: &Plan,
    progress: &Progress,
    topts: &TransferOptions,
    checker: Option<&Checker>,
    backups: Option<&Path>,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let local_path = |rel: &str| -> PathBuf { rel.split('/').fold(local_root.to_path_buf(), |p, c| p.join(c)) };
    let mut blocked = std::collections::HashSet::new();
    if let Some(host_dir) = backups {
        for it in plan.items.iter().filter(|i| i.enabled) {
            let (Some(size), upload) = (it.replaces, matches!(it.action, Action::Upload { .. })) else { continue };
            if !upload && !matches!(it.action, Action::DeleteRemote { dir: false }) {
                continue;
            }
            progress.check_cancel()?;
            progress.set_current(&format!("Backing up {}…", it.rel));
            match crate::replace::back_up_one(fs, &join(remote_root, &it.rel), size, host_dir).await {
                crate::replace::BackedUp::Saved(p) => report.backups.push((it.rel.clone(), p, upload)),
                crate::replace::BackedUp::TooBig => report.too_big.push(it.rel.clone()),
                crate::replace::BackedUp::Failed(e) => {
                    report.failed.push((it.rel.clone(), format!("left as it is: could not back it up first ({e})")));
                    blocked.insert(it.rel.clone());
                }
            }
        }
    }
    let enabled: Vec<&PlanItem> = plan.items.iter().filter(|i| i.enabled && !blocked.contains(&i.rel)).collect();
    fn record(report: &mut SyncReport, rel: &str, r: Result<()>) -> Result<()> {
        match r {
            Ok(()) => report.done += 1,
            Err(e) if e.is::<crate::ssh::Cancelled>() => return Err(e),
            Err(e) => report.failed.push((rel.to_string(), format!("{e:#}"))),
        }
        Ok(())
    }

    let mut dirs: Vec<&&PlanItem> =
        enabled.iter().filter(|i| matches!(i.action, Action::MkdirRemote | Action::MkdirLocal)).collect();
    dirs.sort_by_key(|i| i.rel.matches('/').count());
    for it in dirs {
        progress.check_cancel()?;
        let r = match it.action {
            Action::MkdirRemote => fs.mkdir(&join(remote_root, &it.rel)).await,
            _ => std::fs::create_dir_all(local_path(&it.rel)).map_err(Into::into),
        };
        record(&mut report, &it.rel, r)?;
    }

    let limit = topts.limit();
    for way in [Way::Up, Way::Down] {
        let files: Vec<(PathBuf, String, u64)> = enabled
            .iter()
            .filter_map(|i| match (i.action.clone(), way) {
                (Action::Upload { size }, Way::Up) | (Action::Download { size }, Way::Down) => {
                    Some((local_path(&i.rel), join(remote_root, &i.rel), size))
                }
                _ => None,
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        if way == Way::Down {
            for (l, ..) in &files {
                if let Some(p) = l.parent() {
                    std::fs::create_dir_all(p)?;
                }
            }
        }
        let mut job = transfer::job_of(way, files);
        match transfer::run(fs, &mut job, topts, progress, limit.clone(), checker).await? {
            Ended::Finished => {}
            Ended::Paused | Ended::Cancelled => return Err(crate::ssh::Cancelled.into()),
        }
        let rel = |f: &transfer::FileTask| {
            f.remote.strip_prefix(remote_root).unwrap_or(&f.remote).trim_start_matches('/').to_string()
        };
        for f in &job.files {
            match &f.status {
                Status::Done => report.done += 1,
                Status::Failed(e) => report.failed.push((rel(f), e.clone())),
                Status::Waiting => {}
            }
        }
        report.verified += job.verified();
        if report.unverified.is_none() {
            report.unverified = job.unverified.clone();
        }
    }

    let mut deletes: Vec<&&PlanItem> = enabled
        .iter()
        .filter(|i| matches!(i.action, Action::DeleteRemote { .. } | Action::DeleteLocal { .. }))
        .collect();
    deletes.sort_by_key(|i| std::cmp::Reverse(i.rel.matches('/').count()));
    for it in deletes {
        progress.check_cancel()?;
        let local = local_path(&it.rel);
        let r = match &it.action {
            Action::DeleteRemote { .. } => fs.remove(&join(remote_root, &it.rel)).await,
            Action::DeleteLocal { dir: true } => std::fs::remove_dir_all(&local).map_err(Into::into),
            _ => std::fs::remove_file(&local).map_err(Into::into),
        };
        record(&mut report, &it.rel, r)?;
    }
    Ok(report)
}
