//! Remote file operations over SFTP: browsing, transfers and the primitives the
//! editor builds on.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::Metadata;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symlink; `to_dir` says whether following it lands in a directory.
    Symlink {
        to_dir: bool,
    },
    Other,
}

#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Seconds since the epoch.
    pub mtime: Option<u32>,
    /// Permission bits (`0o644`), without the file type.
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl RemoteEntry {
    pub fn is_dir_like(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Symlink { to_dir: true })
    }
}

/// Shared progress for a running transfer. The UI reads it; the transfer task
/// writes it and checks `cancel` between chunks.
#[derive(Debug, Default)]
pub struct Progress {
    pub done_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    pub done_files: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
}

impl Progress {
    pub(crate) fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!(crate::ssh::Cancelled);
        }
        Ok(())
    }

    fn set_current(&self, s: &str) {
        *self.current.lock().unwrap() = s.to_string();
    }
}

#[derive(Clone)]
pub struct RemoteFs {
    s: Arc<SftpSession>,
}

/// Join remote (always `/`-separated) paths.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The parent of a remote path; `/` is its own parent.
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => ".".to_string(),
    }
}

pub fn file_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// `rwxr-xr-x` style.
pub fn mode_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn kind_of(m: &Metadata) -> EntryKind {
    if m.is_dir() {
        EntryKind::Dir
    } else if m.is_symlink() {
        EntryKind::Symlink { to_dir: false }
    } else if m.is_regular() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

impl RemoteFs {
    pub fn new(session: SftpSession) -> Self {
        Self { s: Arc::new(session) }
    }

    pub async fn canonicalize(&self, path: &str) -> Result<String> {
        self.s.canonicalize(path).await.with_context(|| format!("resolving {path}"))
    }

    pub async fn home(&self) -> Result<String> {
        self.canonicalize(".").await
    }

    pub async fn stat(&self, path: &str) -> Result<Metadata> {
        self.s.metadata(path).await.with_context(|| format!("stat {path}"))
    }

    pub async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.s.try_exists(path).await?)
    }

    /// Directory listing, directories first, then by name.
    pub async fn list(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
        let rd = self.s.read_dir(dir).await.with_context(|| format!("listing {dir}"))?;
        let mut out = Vec::new();
        for e in rd {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let path = join(dir, &name);
            let m = e.metadata();
            let mut kind = kind_of(&m);
            if let EntryKind::Symlink { .. } = kind {
                // Follow it once so links to directories can be opened.
                let to_dir = self.s.metadata(path.clone()).await.map(|t| t.is_dir()).unwrap_or(false);
                kind = EntryKind::Symlink { to_dir };
            }
            out.push(RemoteEntry {
                name,
                path,
                kind,
                size: m.size.unwrap_or(0),
                mtime: m.mtime,
                mode: m.permissions.map(|p| p & 0o7777),
                uid: m.uid,
                gid: m.gid,
            });
        }
        out.sort_by(|a, b| {
            b.is_dir_like().cmp(&a.is_dir_like()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Read a whole file, refusing anything larger than `max` bytes.
    pub async fn read(&self, path: &str, max: u64) -> Result<Vec<u8>> {
        let meta = self.stat(path).await?;
        if meta.is_dir() {
            bail!("{path} is a directory");
        }
        if let Some(size) = meta.size
            && size > max
        {
            bail!("{path} is {} — larger than the {} limit for this operation", human_size(size), human_size(max));
        }
        let mut f = self.s.open(path).await.with_context(|| format!("opening {path}"))?;
        let mut buf = Vec::with_capacity(meta.size.unwrap_or(0) as usize);
        f.read_to_end(&mut buf).await.with_context(|| format!("reading {path}"))?;
        if buf.len() as u64 > max {
            bail!("{path} grew past the size limit while being read");
        }
        Ok(buf)
    }

    /// Overwrite an existing file's contents without replacing the file, so its
    /// owner, group, permissions, hard links and ACLs stay as they are.
    pub async fn overwrite(&self, path: &str, data: &[u8]) -> Result<()> {
        let mut f = self
            .s
            .open_with_flags(path, OpenFlags::WRITE | OpenFlags::TRUNCATE)
            .await
            .with_context(|| format!("opening {path} for writing"))?;
        f.write_all(data).await.with_context(|| format!("writing {path}"))?;
        f.flush().await?;
        let _ = f.sync_all().await; // fsync@openssh.com when the server has it
        f.shutdown().await?;
        Ok(())
    }

    /// Create a new file; fails if it exists. `mode` sets its permissions.
    pub async fn create_new(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<()> {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = mode;
        let mut f = self
            .s
            .open_with_flags_and_attributes(path, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE, attrs)
            .await
            .with_context(|| format!("creating {path}"))?;
        f.write_all(data).await.with_context(|| format!("writing {path}"))?;
        f.flush().await?;
        f.shutdown().await?;
        Ok(())
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.s.create_dir(path).await.with_context(|| format!("creating directory {path}"))
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.s.rename(from, to).await.with_context(|| format!("renaming {from} to {to}"))
    }

    /// Set a file's modification (and access) time, in seconds since the epoch.
    pub async fn set_mtime(&self, path: &str, mtime: u32) -> Result<()> {
        let mut attrs = FileAttributes::empty();
        attrs.mtime = Some(mtime);
        attrs.atime = Some(mtime);
        self.s.set_metadata(path, attrs).await.with_context(|| format!("setting the time of {path}"))
    }

    pub async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some(mode & 0o7777);
        self.s.set_metadata(path, attrs).await.with_context(|| format!("changing permissions of {path}"))
    }

    /// Delete a file, symlink, or directory tree. Symlinks are removed, never
    /// followed.
    pub async fn remove(&self, path: &str) -> Result<()> {
        let m = self.s.symlink_metadata(path).await.with_context(|| format!("stat {path}"))?;
        if m.is_dir() {
            for e in self.list(path).await? {
                Box::pin(self.remove(&e.path)).await?;
            }
            self.s.remove_dir(path).await.with_context(|| format!("removing directory {path}"))?;
        } else {
            self.s.remove_file(path).await.with_context(|| format!("removing {path}"))?;
        }
        Ok(())
    }

    /// Total size and file count of a remote tree, for progress bars.
    async fn measure(&self, path: &str) -> Result<(u64, u64)> {
        let m = self.stat(path).await?;
        if !m.is_dir() {
            return Ok((m.size.unwrap_or(0), 1));
        }
        let (mut bytes, mut files) = (0, 0);
        for e in self.list(path).await? {
            if e.kind == EntryKind::Dir {
                let (b, f) = Box::pin(self.measure(&e.path)).await?;
                bytes += b;
                files += f;
            } else if e.kind == EntryKind::File {
                bytes += e.size;
                files += 1;
            }
        }
        Ok((bytes, files))
    }

    /// Download a file or directory tree into `local_dir`. Symlinks inside
    /// trees are skipped (following them can loop).
    pub async fn download(&self, remote: &str, local_dir: &Path, progress: &Progress) -> Result<PathBuf> {
        let (bytes, _) = self.measure(remote).await?;
        progress.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        let dest = local_dir.join(local_name(file_name(remote))?);
        self.download_inner(remote, &dest, progress).await?;
        Ok(dest)
    }

    pub(crate) async fn download_inner(&self, remote: &str, dest: &Path, progress: &Progress) -> Result<()> {
        progress.check_cancel()?;
        let m = self.stat(remote).await?;
        if m.is_dir() {
            tokio::fs::create_dir_all(dest).await.with_context(|| format!("creating {}", dest.display()))?;
            for e in self.list(remote).await? {
                if matches!(e.kind, EntryKind::Dir | EntryKind::File) {
                    Box::pin(self.download_inner(&e.path, &dest.join(local_name(&e.name)?), progress)).await?;
                }
            }
            return Ok(());
        }
        progress.set_current(remote);
        let mut src = self.s.open(remote).await.with_context(|| format!("opening {remote}"))?;
        let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
        let mut out = tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?;
        let mut buf = vec![0u8; CHUNK];
        let result: Result<()> = async {
            loop {
                progress.check_cancel()?;
                let n = src.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n]).await?;
                progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            out.flush().await?;
            Ok(())
        }
        .await;
        drop(out);
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e.context(format!("downloading {remote}")));
        }
        tokio::fs::rename(&part, dest).await.with_context(|| format!("saving {}", dest.display()))?;
        progress.done_files.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Upload a local file or directory into `remote_dir`. Existing files are
    /// overwritten in place (keeping their permissions); new ones get the local
    /// file's permissions on Unix and the server default elsewhere.
    pub async fn upload(&self, local: &Path, remote_dir: &str, progress: &Progress) -> Result<String> {
        progress.total_bytes.fetch_add(local_size(local)?, Ordering::Relaxed);
        let name = local.file_name().context("cannot upload a path without a file name")?.to_string_lossy().to_string();
        let dest = join(remote_dir, &name);
        self.upload_inner(local, &dest, progress).await?;
        Ok(dest)
    }

    pub(crate) async fn upload_inner(&self, local: &Path, dest: &str, progress: &Progress) -> Result<()> {
        progress.check_cancel()?;
        let meta = tokio::fs::symlink_metadata(local).await?;
        if meta.is_dir() {
            if !self.exists(dest).await? {
                self.mkdir(dest).await?;
            }
            let mut rd = tokio::fs::read_dir(local).await?;
            while let Some(e) = rd.next_entry().await? {
                let ft = e.file_type().await?;
                if ft.is_dir() || ft.is_file() {
                    let name = e.file_name().to_string_lossy().to_string();
                    Box::pin(self.upload_inner(&e.path(), &join(dest, &name), progress)).await?;
                }
            }
            return Ok(());
        }
        progress.set_current(dest);
        let exists = self.exists(dest).await?;
        let mut f = if exists {
            self.s.open_with_flags(dest, OpenFlags::WRITE | OpenFlags::TRUNCATE).await
        } else {
            let mut attrs = FileAttributes::empty();
            attrs.permissions = local_mode(&meta);
            self.s
                .open_with_flags_and_attributes(dest, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE, attrs)
                .await
        }
        .with_context(|| format!("opening {dest} for writing"))?;
        let mut src = tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            progress.check_cancel()?;
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).await.with_context(|| format!("writing {dest}"))?;
            progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        f.flush().await?;
        f.shutdown().await?;
        progress.done_files.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// A remote file name that is safe to use as one local path component. The
/// name comes from the server, so a hostile one like `../../.bashrc` must not
/// be able to write outside the download folder.
pub fn local_name(name: &str) -> Result<&str> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || (cfg!(windows) && (name.contains([':', '<', '>', '"', '|', '?', '*']) || name.ends_with([' ', '.'])));
    if bad {
        bail!("refusing to save a file with the unsafe name {name:?} on this computer");
    }
    Ok(name)
}

fn local_size(path: &Path) -> Result<u64> {
    let m = std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if m.is_dir() {
        let mut total = 0;
        for e in std::fs::read_dir(path)? {
            total += local_size(&e?.path())?;
        }
        Ok(total)
    } else if m.is_file() {
        Ok(m.len())
    } else {
        Ok(0)
    }
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

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(join("/etc", "hosts"), "/etc/hosts");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(parent("/etc/nginx/"), "/etc");
        assert_eq!(parent("/etc"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(file_name("/etc/nginx/nginx.conf"), "nginx.conf");
    }

    #[test]
    fn hostile_names_are_refused() {
        for bad in ["..", ".", "", "../../.bashrc", "a/b", "a\\b", "nul\0byte"] {
            assert!(local_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(local_name("nginx.conf").unwrap(), "nginx.conf");
        assert_eq!(local_name("..hidden").unwrap(), "..hidden");
    }

    #[test]
    fn formatting() {
        assert_eq!(mode_string(0o754), "rwxr-xr--");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
    }
}
