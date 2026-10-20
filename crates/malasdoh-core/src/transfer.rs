//! Copying files and folder trees between this computer and a server, the
//! way a transfer queue needs it:
//!
//! - several files at once, within a limit shared by everything running on
//!   the connection (SFTP waits for a round trip per request, so one file at
//!   a time leaves the link idle);
//! - an interrupted file continues where it stopped on the next run, after
//!   checking that what is already there matches the source at its start
//!   and end;
//! - nobody sees half a file: downloads and new uploads are written under a
//!   temporary name (`name.malasdoh-part`, `.name.malasdoh-part` on the
//!   server) and renamed when complete. Files that already exist on the
//!   server are written in place instead, so their owner, group,
//!   permissions and hard links stay as they are (the same choice as the
//!   editor);
//! - modification times are kept both ways;
//! - finished files are compared by SHA-256, computed on the server with
//!   `sha256sum` (or `shasum`), so nothing is read back over the network.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{OnceCell, Semaphore};

use crate::sftp::{EntryKind, Progress, RemoteFs, file_name, join, local_name, parent};
use crate::ssh::{Connection, shell_quote};

/// Suffix of a file still being copied.
pub const PART: &str = ".malasdoh-part";

/// How much of each end of a partial file is compared before continuing it.
const PROBE: u64 = 64 * 1024;

/// Files hashed per command on the server.
const HASH_BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferOptions {
    /// Files copied at the same time, per connection.
    pub parallel: usize,
    /// Compare SHA-256 on both ends after copying.
    pub verify: bool,
    /// Left out of folder transfers: names like `node_modules`, `*.tmp`, or
    /// paths with `/`. What you select yourself is always copied.
    pub skip: Vec<String>,
    /// Keep a copy of each file on the server an upload replaces, with
    /// the editor's backups.
    pub backup: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self { parallel: 4, verify: true, skip: Vec::new(), backup: true }
    }
}

impl TransferOptions {
    pub fn limit(&self) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(self.parallel.clamp(1, 16)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Way {
    #[default]
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Waiting,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct FileTask {
    pub local: PathBuf,
    pub remote: String,
    /// The source's size and modification time when the job was planned.
    pub size: u64,
    pub mtime: Option<u64>,
    /// The source's permission bits, for files created by uploading.
    pub mode: Option<u32>,
    pub status: Status,
    /// A previous run left part of this file: continue it if it checks out.
    pub resume: bool,
    /// An upload over a file that was already on the server, written in
    /// place rather than under a temporary name.
    pub in_place: bool,
    verified: bool,
}

impl FileTask {
    fn new(local: PathBuf, remote: String, size: u64, mtime: Option<u64>, mode: Option<u32>) -> Self {
        Self {
            local,
            remote,
            size,
            mtime,
            mode,
            status: Status::Waiting,
            resume: false,
            in_place: false,
            verified: false,
        }
    }
}

/// One queued transfer: the folders to create and the files to copy.
#[derive(Debug, Clone, Default)]
pub struct Job {
    pub way: Way,
    pub remote_dirs: Vec<String>,
    pub local_dirs: Vec<PathBuf>,
    pub files: Vec<FileTask>,
    /// Left out (symlinks, special files, unsafe names, skip patterns).
    pub skipped: Vec<String>,
    /// Why files were not compared by checksum, if they were not.
    pub unverified: Option<String>,
}

impl Job {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn done(&self) -> usize {
        self.files.iter().filter(|f| f.status == Status::Done).count()
    }

    pub fn verified(&self) -> usize {
        self.files.iter().filter(|f| f.verified).count()
    }

    pub fn failures(&self) -> Vec<(String, String)> {
        self.files
            .iter()
            .filter_map(|f| match &f.status {
                Status::Failed(e) => Some((self.shown(f), e.clone())),
                _ => None,
            })
            .collect()
    }

    /// Files written in place on the server and not finished: the server
    /// has an incomplete copy until the job is resumed.
    pub fn incomplete_on_server(&self) -> Vec<String> {
        if self.way != Way::Up {
            return Vec::new();
        }
        self.files
            .iter()
            .filter(|f| f.status != Status::Done && f.resume && f.in_place)
            .map(|f| f.remote.clone())
            .collect()
    }

    fn shown(&self, f: &FileTask) -> String {
        match self.way {
            Way::Up => f.remote.clone(),
            Way::Down => f.local.display().to_string(),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.files.iter().all(|f| f.status == Status::Done)
    }
}

fn skip(opts: &TransferOptions, rel: &str) -> bool {
    opts.skip.iter().any(|p| crate::validate::matches_path(p, rel))
}

fn local_mtime(m: &std::fs::Metadata) -> Option<u64> {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs())
}

#[cfg(unix)]
fn local_mode(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn local_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}

// ---------------------------------------------------------------------------
// Planning

/// Upload `locals` (files or folders) into `remote_dir`.
/// A job for files whose paths are already known (folder sync).
pub fn job_of(way: Way, files: Vec<(PathBuf, String, u64)>) -> Job {
    let files = files
        .into_iter()
        .map(|(local, remote, size)| {
            let mode = match way {
                Way::Up => std::fs::metadata(&local).ok().and_then(|m| local_mode(&m)),
                Way::Down => None,
            };
            FileTask::new(local, remote, size, None, mode)
        })
        .collect();
    Job { way, files, ..Default::default() }
}

pub fn plan_upload(locals: &[PathBuf], remote_dir: &str, opts: &TransferOptions) -> Result<Job> {
    let mut job = Job { way: Way::Up, ..Default::default() };
    for l in locals {
        let name = l.file_name().context("cannot upload a path without a file name")?.to_string_lossy().to_string();
        walk_local(l, &join(remote_dir, &name), &name, opts, &mut job, true)?;
    }
    Ok(job)
}

fn walk_local(local: &Path, remote: &str, rel: &str, opts: &TransferOptions, job: &mut Job, top: bool) -> Result<()> {
    let m = std::fs::symlink_metadata(local).with_context(|| format!("reading {}", local.display()))?;
    if !top && skip(opts, rel) {
        job.skipped.push(rel.to_string());
        return Ok(());
    }
    if m.is_dir() {
        job.remote_dirs.push(remote.to_string());
        let mut entries: Vec<_> = std::fs::read_dir(local)
            .with_context(|| format!("reading {}", local.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            walk_local(&e.path(), &join(remote, &name), &format!("{rel}/{name}"), opts, job, false)?;
        }
    } else if m.is_file() {
        job.files.push(FileTask::new(
            local.to_path_buf(),
            remote.to_string(),
            m.len(),
            local_mtime(&m),
            local_mode(&m),
        ));
    } else {
        job.skipped.push(format!("{rel} (not a regular file)"));
    }
    Ok(())
}

/// Download `remotes` (files or folders) into `local_dir`. Symlinks inside
/// folders are left out (following them can loop); a selected symlink is
/// followed.
pub async fn plan_download(fs: &RemoteFs, remotes: &[String], local_dir: &Path, opts: &TransferOptions) -> Result<Job> {
    let mut job = Job { way: Way::Down, ..Default::default() };
    for r in remotes {
        let name = file_name(r).to_string();
        let dest = local_dir.join(local_name(&name)?);
        let m = fs.stat(r).await?;
        if m.is_dir() {
            walk_remote(fs, r, &dest, &name, opts, &mut job).await?;
        } else {
            job.files.push(FileTask::new(
                dest,
                r.clone(),
                m.size.unwrap_or(0),
                m.mtime.map(u64::from),
                m.permissions.map(|p| p & 0o7777),
            ));
        }
    }
    Ok(job)
}

async fn walk_remote(
    fs: &RemoteFs,
    remote: &str,
    local: &Path,
    rel: &str,
    opts: &TransferOptions,
    job: &mut Job,
) -> Result<()> {
    job.local_dirs.push(local.to_path_buf());
    for e in fs.list(remote).await? {
        let r = format!("{rel}/{}", e.name);
        if skip(opts, &r) {
            job.skipped.push(r);
            continue;
        }
        let Ok(name) = local_name(&e.name) else {
            job.skipped.push(format!("{r} (a name this computer cannot use)"));
            continue;
        };
        let dest = local.join(name);
        match e.kind {
            EntryKind::Dir => Box::pin(walk_remote(fs, &e.path, &dest, &r, opts, job)).await?,
            EntryKind::File => {
                job.files.push(FileTask::new(dest, e.path.clone(), e.size, e.mtime.map(u64::from), e.mode))
            }
            EntryKind::Symlink { .. } => job.skipped.push(format!("{r} (symlink)")),
            EntryKind::Other => job.skipped.push(format!("{r} (not a regular file)")),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Running

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Every file was tried; see each file's status.
    Finished,
    /// Stopped with [`Progress::pause`]: partial files are kept to continue.
    Paused,
    /// Stopped with [`Progress::cancel`] alone: partial copies were removed.
    Cancelled,
}

/// Copy the job's files that are not done yet. Several run at once, each
/// holding a permit of `limit` while it copies. A file that fails is marked
/// and the others continue; the run fails as a whole only if a folder
/// cannot be created.
pub async fn run(
    fs: &RemoteFs,
    job: &mut Job,
    opts: &TransferOptions,
    progress: &Progress,
    limit: Arc<Semaphore>,
    checker: Option<&Checker>,
) -> Result<Ended> {
    let todo: Vec<usize> = (0..job.files.len()).filter(|&i| job.files[i].status != Status::Done).collect();
    // Added to, so one progress can follow several jobs (sync runs two).
    progress.total_bytes.fetch_add(todo.iter().map(|&i| job.files[i].size).sum(), Ordering::Relaxed);
    progress.total_files.fetch_add(todo.len() as u64, Ordering::Relaxed);

    let mut created = std::collections::HashSet::new();
    for d in &job.remote_dirs {
        if !fs.exists(d).await? {
            fs.mkdir(d).await?;
            created.insert(d.clone());
        }
    }
    for d in &job.local_dirs {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }

    // Which files are already on the server, from one listing per folder
    // rather than a lookup per file.
    let mut present: std::collections::HashMap<String, Option<std::collections::HashSet<String>>> = Default::default();
    if job.way == Way::Up && !fs.is_shell() {
        for &i in &todo {
            let dir = parent(&job.files[i].remote);
            if let std::collections::hash_map::Entry::Vacant(slot) = present.entry(dir) {
                let names = if created.contains(slot.key()) {
                    Some(Default::default())
                } else {
                    fs.list(slot.key()).await.ok().map(|l| l.into_iter().map(|e| e.name).collect())
                };
                slot.insert(names);
            }
        }
    }
    let present = &present;

    let way = job.way;
    // Built up front (not lazily in the stream) so the future stays Send.
    let mut futs = Vec::with_capacity(todo.len());
    for &i in &todo {
        let mut task = job.files[i].clone();
        let limit = limit.clone();
        futs.push(async move {
            let r = async {
                let _permit = limit.acquire_owned().await?;
                progress.check_cancel()?;
                match way {
                    Way::Down => download(fs, &mut task, progress).await,
                    Way::Up => {
                        let known = present
                            .get(&parent(&task.remote))
                            .and_then(|names| names.as_ref())
                            .map(|names| names.contains(file_name(&task.remote)));
                        upload(fs, &mut task, known, progress).await
                    }
                }
            }
            .await;
            (i, task, r)
        });
    }
    let results: Vec<(usize, FileTask, Result<()>)> = futures::stream::iter(futs).buffer_unordered(64).collect().await;

    let mut stopped = false;
    for (i, task, r) in results {
        job.files[i] = task;
        let f = &mut job.files[i];
        match r {
            Ok(()) => {
                f.status = Status::Done;
                f.resume = false;
                f.verified = false;
            }
            Err(e) if e.is::<crate::ssh::Cancelled>() => {
                stopped = true;
                f.status = Status::Waiting;
                f.resume = true;
            }
            Err(e) => {
                f.status = Status::Failed(format!("{e:#}"));
                f.resume = true;
            }
        }
    }
    if stopped {
        if progress.pause.load(Ordering::Relaxed) {
            return Ok(Ended::Paused);
        }
        discard_partials(fs, job).await;
        return Ok(Ended::Cancelled);
    }
    if opts.verify {
        verify(job, progress, checker).await;
    }
    Ok(Ended::Finished)
}

/// Remove what unfinished files left behind (not files written in place,
/// which are the user's own files).
pub async fn discard_partials(fs: &RemoteFs, job: &mut Job) {
    for f in job.files.iter_mut().filter(|f| f.status != Status::Done && f.resume) {
        match job.way {
            Way::Down => {
                let _ = std::fs::remove_file(local_part(&f.local));
            }
            Way::Up if !f.in_place && !fs.is_shell() => {
                let _ = fs.remove(&remote_part(&f.remote)).await;
            }
            Way::Up => {}
        }
        f.resume = false;
    }
}

fn local_part(dest: &Path) -> PathBuf {
    dest.with_file_name(format!("{}{PART}", dest.file_name().unwrap_or_default().to_string_lossy()))
}

fn remote_part(dest: &str) -> String {
    join(&parent(dest), &format!(".{}{PART}", file_name(dest)))
}

fn set_local_mtime(path: &Path, mtime: Option<u64>) -> Result<()> {
    if let Some(t) = mtime {
        let f = std::fs::File::options().write(true).open(path)?;
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(t))?;
    }
    Ok(())
}

fn read_local_range(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0; len as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Whether the first `len` bytes of `local` and `remote` look the same: the
/// first and last [`PROBE`] bytes are compared.
async fn prefix_matches(fs: &RemoteFs, local: &Path, remote: &str, len: u64) -> Result<bool> {
    if len == 0 {
        return Ok(true);
    }
    let head = len.min(PROBE);
    let tail_at = len.saturating_sub(PROBE).max(head);
    for (at, n) in [(0, head), (tail_at, len - tail_at)] {
        if n == 0 {
            continue;
        }
        if read_local_range(local, at, n)? != fs.read_range(remote, at, n).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The source changed since the job was planned: start that file over.
fn changed(task: &mut FileTask, size: u64, mtime: Option<u64>) {
    if task.size != size || task.mtime != mtime {
        task.size = size;
        task.mtime = mtime;
        task.resume = false;
    }
}

async fn download(fs: &RemoteFs, task: &mut FileTask, progress: &Progress) -> Result<()> {
    progress.set_current(&task.remote);
    // The plan's listing is fresh on a first try; before continuing a
    // partial, check the file has not changed since.
    if task.resume || task.mtime.is_none() {
        let m = fs.stat(&task.remote).await?;
        if m.is_dir() {
            bail!("{} is now a folder", task.remote);
        }
        changed(task, m.size.unwrap_or(0), m.mtime.map(u64::from));
    }
    let part = local_part(&task.local);
    let mut start = 0;
    if task.resume
        && !fs.is_shell()
        && let Ok(pm) = std::fs::metadata(&part)
        && pm.len() <= task.size
        && prefix_matches(fs, &part, &task.remote, pm.len()).await?
    {
        start = pm.len();
    }
    let mut out = std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(start == 0)
        .open(&part)
        .with_context(|| format!("creating {}", part.display()))?;
    out.set_len(start)?;
    out.seek(SeekFrom::Start(start))?;
    progress.done_bytes.fetch_add(start, Ordering::Relaxed);
    progress.resumed_bytes.fetch_add(start, Ordering::Relaxed);
    task.resume = true;
    let mut out = tokio::fs::File::from_std(out);
    fs.read_into(&task.remote, start, &mut out, progress)
        .await
        .with_context(|| format!("downloading {}", task.remote))?;
    use tokio::io::AsyncWriteExt;
    out.flush().await?;
    drop(out);
    let size = std::fs::metadata(&part)?.len();
    if size != task.size {
        bail!("{} changed while it was being downloaded ({} bytes, expected {})", task.remote, size, task.size);
    }
    set_local_mtime(&part, task.mtime)?;
    std::fs::rename(&part, &task.local).with_context(|| format!("saving {}", task.local.display()))?;
    progress.done_files.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// `known`: whether the file is on the server, if a folder listing said so.
async fn upload(fs: &RemoteFs, task: &mut FileTask, known: Option<bool>, progress: &Progress) -> Result<()> {
    progress.set_current(&task.remote);
    let m = std::fs::metadata(&task.local).with_context(|| format!("reading {}", task.local.display()))?;
    changed(task, m.len(), local_mtime(&m));
    if fs.is_shell() {
        // Through shell commands: one stream per file, no continuing.
        let mut src = tokio::fs::File::open(&task.local).await?;
        fs.write_stream(&task.remote, &mut src, task.mode, progress).await?;
        set_remote_mtime(fs, &task.remote, task.mtime).await;
        progress.done_files.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let existing = match known {
        Some(false) if !task.resume => None,
        _ => match fs.stat(&task.remote).await {
            Ok(m) if m.is_dir() => bail!("{} is a folder on the server", task.remote),
            Ok(m) => Some(m.size.unwrap_or(0)),
            Err(_) => None,
        },
    };
    // Decided on the first try; a resumed file carries on the same way.
    if !task.resume {
        task.in_place = existing.is_some();
    } else if task.in_place && existing.is_none() {
        // Deleted on the server since: start it as a new file.
        task.in_place = false;
        task.resume = false;
    }
    let target = if task.in_place { task.remote.clone() } else { remote_part(&task.remote) };
    let mut start = 0;
    if task.resume {
        let have = if task.in_place { existing } else { fs.stat(&target).await.ok().and_then(|m| m.size) };
        if let Some(have) = have
            && have <= task.size
            && prefix_matches(fs, &task.local, &target, have).await?
        {
            start = have;
        }
    }
    let mode = if task.in_place { None } else { task.mode };
    progress.done_bytes.fetch_add(start, Ordering::Relaxed);
    progress.resumed_bytes.fetch_add(start, Ordering::Relaxed);
    task.resume = true;
    let mut src = std::fs::File::open(&task.local)?;
    src.seek(SeekFrom::Start(start))?;
    let mut src = tokio::fs::File::from_std(src);
    let mtime = task.mtime.map(|t| t as u32);
    let written = fs
        .write_from_offset(&target, start, &mut src, mode, mtime, progress)
        .await
        .with_context(|| format!("uploading {}", task.remote))?;
    if start + written != task.size {
        bail!("{} changed while it was being uploaded", task.local.display());
    }
    if !task.in_place {
        // The time set on the temporary file goes with it.
        fs.rename(&target, &task.remote).await.with_context(|| {
            format!("{} appeared on the server meanwhile; the upload is kept as {target}", task.remote)
        })?;
    }
    progress.done_files.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Best effort: some servers do not allow setting times (and on files
/// written in place, someone else may own the file).
async fn set_remote_mtime(fs: &RemoteFs, path: &str, mtime: Option<u64>) {
    if let Some(t) = mtime {
        let _ = fs.set_mtime(path, t as u32).await;
    }
}

// ---------------------------------------------------------------------------
// Checksums

/// Computes SHA-256 of files on the server with its own tools.
pub struct Checker {
    conn: Arc<Connection>,
    /// As root through sudo (with this password, if it needs one).
    sudo: Option<Option<String>>,
    tool: OnceCell<Option<&'static str>>,
}

impl Checker {
    pub fn new(conn: Arc<Connection>, sudo: Option<Option<String>>) -> Self {
        Self { conn, sudo, tool: OnceCell::new() }
    }

    async fn tool(&self) -> Option<&'static str> {
        *self
            .tool
            .get_or_init(|| async {
                let out = self.conn.exec("sh -c 'command -v sha256sum || command -v shasum'", None).await.ok()?;
                let found = String::from_utf8_lossy(&out.stdout).to_string();
                if found.contains("sha256sum") {
                    Some("sha256sum")
                } else if found.contains("shasum") {
                    Some("shasum -a 256")
                } else {
                    None
                }
            })
            .await
    }

    /// The SHA-256 of each path, in order, or `None` for a file that could
    /// not be read. Fails when the server has no tool for it.
    pub async fn sha256(&self, paths: &[String]) -> Result<Vec<Option<String>>> {
        let Some(tool) = self.tool().await else { bail!("the server has neither sha256sum nor shasum") };
        let mut out = Vec::with_capacity(paths.len());
        for batch in paths.chunks(HASH_BATCH) {
            let script = format!(
                "for f do if h=$({tool} < \"$f\" 2>/dev/null); then echo \"${{h%% *}}\"; else echo -; fi; done"
            );
            let args: Vec<String> = batch.iter().map(|p| shell_quote(p)).collect();
            let (cmd, stdin) = match &self.sudo {
                None => (format!("sh -c {} sh {}", shell_quote(&script), args.join(" ")), None),
                Some(pw) => (
                    format!("sudo -S -p '' -- sh -c {} sh {}", shell_quote(&script), args.join(" ")),
                    Some(format!("{}\n", pw.as_deref().unwrap_or("")).into_bytes()),
                ),
            };
            let r = self.conn.exec(&cmd, stdin.as_deref()).await?;
            let lines: Vec<String> = String::from_utf8_lossy(&r.stdout).lines().map(str::to_string).collect();
            if lines.len() != batch.len() {
                bail!("hashing on the server failed: {}", String::from_utf8_lossy(&r.stderr).trim());
            }
            out.extend(lines.into_iter().map(|l| {
                let l = l.trim().to_lowercase();
                (l.len() == 64 && l.bytes().all(|b| b.is_ascii_hexdigit())).then_some(l)
            }));
        }
        Ok(out)
    }
}

/// SHA-256 of a file on this computer.
pub async fn local_sha256(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(hex::encode(h.finalize()))
    })
    .await?
}

async fn verify(job: &mut Job, progress: &Progress, checker: Option<&Checker>) {
    let idx: Vec<usize> =
        (0..job.files.len()).filter(|&i| job.files[i].status == Status::Done && !job.files[i].verified).collect();
    if idx.is_empty() {
        return;
    }
    let Some(checker) = checker else {
        job.unverified = Some("checksums need a shell on the server".into());
        return;
    };
    progress.set_current(&format!("Comparing checksums of {} file(s)…", idx.len()));
    let paths: Vec<String> = idx.iter().map(|&i| job.files[i].remote.clone()).collect();
    let remote = match checker.sha256(&paths).await {
        Ok(h) => h,
        Err(e) => {
            job.unverified = Some(format!("{e:#}"));
            return;
        }
    };
    job.unverified = None;
    let mut unreadable = 0;
    for (&i, theirs) in idx.iter().zip(remote) {
        let f = &mut job.files[i];
        let Some(theirs) = theirs else {
            unreadable += 1;
            continue;
        };
        match local_sha256(&f.local).await {
            Ok(ours) if ours == theirs => f.verified = true,
            Ok(_) => {
                f.status =
                    Status::Failed("the copy's checksum differs from the original; Retry copies it again".into());
                f.resume = false;
            }
            Err(e) => {
                f.status = Status::Failed(format!("{e:#}"));
                f.resume = false;
            }
        }
    }
    if unreadable > 0 {
        job.unverified = Some(format!("{unreadable} file(s) could not be read on the server to compare"));
    }
}
