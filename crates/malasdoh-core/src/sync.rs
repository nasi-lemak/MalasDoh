//! Compare a local folder with a remote one and bring them in line, with a
//! plan the user sees (and can trim) before anything is copied or deleted.
//!
//! Files are compared by size and modification time (to the second, with a
//! two-second allowance for FAT and SMB). After copying, the destination's
//! time is set to the source's, so the next comparison sees them as equal.
//! Symlinks and special files are reported and skipped, never followed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sftp::{EntryKind, Progress, RemoteFs, join, local_name};

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

fn same(a: &Meta, b: &Meta) -> bool {
    a.size == b.size && a.mtime.abs_diff(b.mtime) <= 2
}

/// Work out what to do. Nothing is changed.
pub async fn plan(fs: &RemoteFs, local_root: &Path, remote_root: &str, opts: &SyncOptions) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut local = BTreeMap::new();
    let mut remote = BTreeMap::new();
    scan_local(local_root, "", &opts.exclude, &mut local, &mut plan.skipped)?;
    scan_remote(fs, remote_root, "", &opts.exclude, &mut remote, &mut plan.skipped).await?;
    let item = |rel: &str, action, reason| PlanItem { rel: rel.to_string(), action, reason, enabled: true };

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
            Some(r) if same(l, r) && same_bytes(fs, &local_root.join(rel), &join(remote_root, rel)).await? => {
                plan.unchanged += 1
            }
            Some(r) if same(l, r) => plan.items.push(match opts.direction {
                Direction::Download => item(rel, Action::Download { size: r.size }, "contents differ"),
                Direction::Upload => item(rel, Action::Upload { size: l.size }, "contents differ"),
                Direction::Both => {
                    plan.skipped.push(format!("{rel} (contents differ, same time on both sides)"));
                    continue;
                }
            }),
            Some(r) => {
                let local_newer = l.mtime > r.mtime;
                match opts.direction {
                    Direction::Upload => plan.items.push(item(
                        rel,
                        Action::Upload { size: l.size },
                        if local_newer { "newer here" } else { "different here" },
                    )),
                    Direction::Download => plan.items.push(item(
                        rel,
                        Action::Download { size: r.size },
                        if local_newer { "different on the server" } else { "newer on the server" },
                    )),
                    Direction::Both if local_newer => {
                        plan.items.push(item(rel, Action::Upload { size: l.size }, "newer here"))
                    }
                    Direction::Both if r.mtime > l.mtime => {
                        plan.items.push(item(rel, Action::Download { size: r.size }, "newer on the server"))
                    }
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
            plan.items.push(item(rel, Action::DeleteRemote { dir: r.dir }, "not here"));
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
}

/// Carry out the enabled items: folders first, then copies, then deletions
/// (deepest first). A failing item is recorded and the rest continue.
pub async fn execute(
    fs: &RemoteFs,
    local_root: &Path,
    remote_root: &str,
    plan: &Plan,
    progress: &Progress,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    progress.total_bytes.store(plan.bytes(), Ordering::Relaxed);
    let local_path = |rel: &str| -> PathBuf { rel.split('/').fold(local_root.to_path_buf(), |p, c| p.join(c)) };
    let mut items: Vec<&PlanItem> = plan.items.iter().filter(|i| i.enabled).collect();
    let rank = |a: &Action| match a {
        Action::MkdirRemote | Action::MkdirLocal => 0,
        Action::Upload { .. } | Action::Download { .. } => 1,
        _ => 2,
    };
    items.sort_by(|a, b| {
        rank(&a.action).cmp(&rank(&b.action)).then_with(|| {
            let depth = |s: &str| s.matches('/').count();
            if rank(&a.action) == 2 { depth(&b.rel).cmp(&depth(&a.rel)) } else { depth(&a.rel).cmp(&depth(&b.rel)) }
        })
    });
    for it in items {
        progress.check_cancel()?;
        let remote = join(remote_root, &it.rel);
        let local = local_path(&it.rel);
        let r: Result<()> = async {
            match &it.action {
                Action::MkdirRemote => fs.mkdir(&remote).await,
                Action::MkdirLocal => Ok(std::fs::create_dir_all(&local)?),
                Action::Upload { .. } => {
                    fs.upload_inner(&local, &remote, progress).await?;
                    if let Some(t) =
                        std::fs::metadata(&local)?.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    {
                        fs.set_mtime(&remote, t.as_secs() as u32).await?;
                    }
                    Ok(())
                }
                Action::Download { .. } => {
                    if let Some(parent) = local.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    fs.download_inner(&remote, &local, progress).await?;
                    if let Some(t) = fs.stat(&remote).await?.mtime {
                        let f = std::fs::File::options().write(true).open(&local)?;
                        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(t as u64))?;
                    }
                    Ok(())
                }
                Action::DeleteRemote { .. } => fs.remove(&remote).await,
                Action::DeleteLocal { dir: true } => Ok(std::fs::remove_dir_all(&local)?),
                Action::DeleteLocal { dir: false } => Ok(std::fs::remove_file(&local)?),
            }
        }
        .await;
        match r {
            Ok(()) => report.done += 1,
            Err(e) if e.is::<crate::ssh::Cancelled>() => return Err(e),
            Err(e) => report.failed.push((it.rel.clone(), format!("{e:#}"))),
        }
    }
    Ok(report)
}
