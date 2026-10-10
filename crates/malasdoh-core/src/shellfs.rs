//! File operations through plain shell commands, for servers without the
//! SFTP subsystem (busybox, dropbear, routers and appliances). Everything
//! goes through `sh -c` with quoted arguments, so the user's login shell
//! (bash, fish, csh…) does not matter.
//!
//! Listings are NUL-separated so any file name survives; file contents are
//! streamed through `cat` in both directions, which is binary-safe because
//! exec channels have no terminal.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use russh::ChannelMsg;
use russh_sftp::protocol::FileAttributes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sftp::{EntryKind, Progress, RemoteEntry, join};
use crate::ssh::{Connection, ExecOutput, shell_quote};

const CHUNK: usize = 64 * 1024;

pub(crate) enum WriteMode {
    /// The file must exist; its contents are replaced in place.
    Overwrite,
    /// The file must not exist. `Some(mode)` sets its permissions (it is
    /// created owner-only first, so it is never briefly readable by others).
    CreateNew(Option<u32>),
    /// Replace if it exists, otherwise create with `mode`.
    Upload(Option<u32>),
}

pub(crate) struct ShellFs {
    conn: Arc<Connection>,
}

fn q(s: &str) -> String {
    shell_quote(s)
}

fn sh(script: &str) -> String {
    format!("sh -c {}", shell_quote(script))
}

impl ShellFs {
    pub(crate) fn new(conn: Arc<Connection>) -> Self {
        Self { conn }
    }

    /// Run a script; fail with its stderr unless it exits 0.
    async fn run(&self, script: &str, what: &str) -> Result<ExecOutput> {
        let out = self.conn.exec(&sh(script), None).await.with_context(|| what.to_string())?;
        if !out.success() {
            let err = out.stderr_text();
            bail!("{what}: {}", if err.is_empty() { format!("exit status {:?}", out.status) } else { err });
        }
        Ok(out)
    }

    pub(crate) async fn canonicalize(&self, path: &str) -> Result<String> {
        let script = format!(
            "p={}; if [ -d \"$p\" ]; then cd -- \"$p\" && pwd -P; else cd -- \"$(dirname -- \"$p\")\" || exit 1; d=$(pwd -P); \
             [ \"$d\" = / ] && d=; printf '%s/%s\\n' \"$d\" \"$(basename -- \"$p\")\"; fi",
            q(if path.is_empty() { "." } else { path })
        );
        let out = self.run(&script, &format!("resolving {path}")).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
    }

    pub(crate) async fn stat(&self, path: &str, follow: bool) -> Result<FileAttributes> {
        let script = format!("stat {}-c '%s %Y %f %u %g' -- {}", if follow { "-L " } else { "" }, q(path));
        let out = self.run(&script, &format!("stat {path}")).await?;
        parse_stat(String::from_utf8_lossy(&out.stdout).trim())
            .with_context(|| format!("unexpected stat output for {path}"))
    }

    pub(crate) async fn exists(&self, path: &str) -> Result<bool> {
        let out = self.conn.exec(&sh(&format!("[ -e {p} ] || [ -L {p} ]", p = q(path))), None).await?;
        Ok(out.success())
    }

    pub(crate) async fn list(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
        // One record per entry: "<is-dir> <size> <mtime> <hex mode> <uid> <gid> <name>\0".
        let script = format!(
            "cd -- {} || exit 1; for f in .[!.]* ..?* *; do if [ -e \"$f\" ] || [ -L \"$f\" ]; then \
             if [ -d \"$f\" ]; then d=1; else d=0; fi; m=$(stat -c '%s %Y %f %u %g' -- \"$f\") || continue; \
             printf '%s %s %s\\000' \"$d\" \"$m\" \"$f\"; fi; done",
            q(dir)
        );
        let out = self.run(&script, &format!("listing {dir}")).await?;
        let mut entries = Vec::new();
        for rec in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let rec = String::from_utf8_lossy(rec);
            let mut parts = rec.splitn(7, ' ');
            let (Some(d), Some(size), Some(mtime), Some(mode), Some(uid), Some(gid), Some(name)) =
                (parts.next(), parts.next(), parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(attrs) = parse_stat(&format!("{size} {mtime} {mode} {uid} {gid}")) else { continue };
            let kind = if attrs.is_dir() {
                EntryKind::Dir
            } else if attrs.is_symlink() {
                EntryKind::Symlink { to_dir: d == "1" }
            } else if attrs.is_regular() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.push(RemoteEntry {
                name: name.to_string(),
                path: join(dir, name),
                kind,
                size: attrs.size.unwrap_or(0),
                mtime: attrs.mtime,
                mode: attrs.permissions.map(|p| p & 0o7777),
                uid: attrs.uid,
                gid: attrs.gid,
            });
        }
        Ok(entries)
    }

    pub(crate) async fn mkdir(&self, path: &str) -> Result<()> {
        self.run(&format!("mkdir -- {}", q(path)), &format!("creating directory {path}")).await.map(drop)
    }

    /// Like SFTP rename: refuses to replace an existing target.
    pub(crate) async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let script = format!(
            "if [ -e {t} ] || [ -L {t} ]; then echo 'the target already exists' >&2; exit 1; fi; mv -- {f} {t}",
            f = q(from),
            t = q(to)
        );
        self.run(&script, &format!("renaming {from} to {to}")).await.map(drop)
    }

    pub(crate) async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        self.run(&format!("chmod {:o} -- {}", mode & 0o7777, q(path)), &format!("changing permissions of {path}"))
            .await
            .map(drop)
    }

    pub(crate) async fn set_mtime(&self, path: &str, mtime: u32) -> Result<()> {
        // POSIX touch -t takes local time; pin the zone to UTC.
        let t = time::OffsetDateTime::from_unix_timestamp(mtime as i64)?;
        let stamp = t.format(time::macros::format_description!("[year][month][day][hour][minute].[second]"))?;
        self.run(&format!("TZ=UTC0 touch -m -t {stamp} -- {}", q(path)), &format!("setting the time of {path}"))
            .await
            .map(drop)
    }

    /// Remove a file, symlink or tree (symlinks are removed, not followed).
    pub(crate) async fn remove(&self, path: &str) -> Result<()> {
        if path.trim_end_matches('/').is_empty() {
            bail!("refusing to remove /");
        }
        self.run(&format!("rm -rf -- {}", q(path)), &format!("removing {path}")).await.map(drop)
    }

    /// Stream a file's contents into `w`.
    pub(crate) async fn read_to(
        &self,
        path: &str,
        w: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<&Progress>,
        max: u64,
    ) -> Result<u64> {
        let mut ch = self.conn.handle().channel_open_session().await?;
        ch.exec(true, sh(&format!("cat -- {}", q(path)))).await?;
        let (mut n, mut stderr, mut status) = (0u64, Vec::new(), None);
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if let Some(p) = progress {
                        p.check_cancel()?;
                        p.done_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    n += data.len() as u64;
                    if n > max {
                        let _ = ch.close().await;
                        bail!("{path} is larger than the limit for this operation");
                    }
                    w.write_all(&data).await?;
                }
                ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        w.flush().await?;
        if status != Some(0) {
            bail!("reading {path}: {}", String::from_utf8_lossy(&stderr).trim());
        }
        Ok(n)
    }

    /// Stream `r` into a file.
    pub(crate) async fn write_from(
        &self,
        path: &str,
        r: &mut (dyn AsyncRead + Unpin + Send),
        mode: WriteMode,
        progress: Option<&Progress>,
    ) -> Result<()> {
        let p = q(path);
        let create = |m: u32| format!("umask 077; cat > {p} && chmod {m:o} -- {p}");
        let script = match mode {
            WriteMode::Overwrite => format!("[ -f {p} ] || {{ echo 'not an existing file' >&2; exit 2; }}; cat > {p}"),
            WriteMode::CreateNew(Some(m)) => format!("set -C; {}", create(m)),
            WriteMode::CreateNew(None) => format!("set -C; cat > {p}"),
            WriteMode::Upload(Some(m)) => format!("if [ -e {p} ]; then cat > {p}; else {}; fi", create(m)),
            WriteMode::Upload(None) => format!("cat > {p}"),
        };
        let mut ch = self.conn.handle().channel_open_session().await?;
        ch.exec(true, sh(&script)).await?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if let Some(p) = progress {
                p.check_cancel()?;
            }
            let n = r.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ch.data(&buf[..n]).await?;
            if let Some(p) = progress {
                p.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        ch.eof().await?;
        let (mut stderr, mut status) = (Vec::new(), None);
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        if status != Some(0) {
            bail!("writing {path}: {}", String::from_utf8_lossy(&stderr).trim());
        }
        Ok(())
    }
}

/// "<size> <mtime> <hex st_mode> <uid> <gid>" as printed by `stat -c '%s %Y %f %u %g'`.
fn parse_stat(s: &str) -> Result<FileAttributes> {
    let v: Vec<&str> = s.split_whitespace().collect();
    if v.len() != 5 {
        bail!("{s:?}");
    }
    let mut a = FileAttributes::empty();
    a.size = Some(v[0].parse()?);
    a.mtime = Some(v[1].parse::<i64>()?.clamp(0, u32::MAX as i64) as u32);
    a.atime = a.mtime;
    a.permissions = Some(u32::from_str_radix(v[2], 16)?);
    a.uid = Some(v[3].parse()?);
    a.gid = Some(v[4].parse()?);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_output() {
        let a = parse_stat("1234 1700000000 81a4 0 1000").unwrap();
        assert_eq!((a.size, a.mtime, a.uid, a.gid), (Some(1234), Some(1_700_000_000), Some(0), Some(1000)));
        assert!(a.is_regular() && !a.is_dir());
        assert_eq!(a.permissions.unwrap() & 0o7777, 0o644);
        assert!(parse_stat("41ed 1 2").is_err());
        assert!(parse_stat("4096 1 41ed 0 0").unwrap().is_dir());
    }
}
