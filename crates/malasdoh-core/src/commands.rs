//! Shell commands for file operations SFTP does not cover well: owner,
//! symlinks, archives and searching. Every argument is quoted; each builder
//! is tested on its own and against a real server.

use anyhow::{Result, bail};

use crate::sftp::{file_name, join, parent};
use crate::ssh::shell_quote as q;

/// `user`, `user:group` or `:group`, with the characters account names use.
pub fn check_owner(owner: &str) -> Result<()> {
    let ok = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let (u, g) = owner.split_once(':').unwrap_or((owner, ""));
    if owner.is_empty()
        || (u.is_empty() && g.is_empty())
        || !ok(u)
        || !ok(g)
        || u.starts_with('-')
        || g.starts_with('-')
    {
        bail!("owner must look like user, user:group or :group");
    }
    Ok(())
}

/// Change owner; `-h` changes a symlink itself, not what it points to.
pub fn chown(paths: &[String], owner: &str, recursive: bool) -> Result<String> {
    check_owner(owner)?;
    if paths.is_empty() {
        bail!("nothing selected");
    }
    let r = if recursive { "-R " } else { "" };
    Ok(format!("chown -h {r}-- {} {}", q(owner), paths.iter().map(|p| q(p)).collect::<Vec<_>>().join(" ")))
}

/// A new symlink at `link` pointing to `target`. Never replaces anything.
pub fn symlink(target: &str, link: &str) -> Result<String> {
    if target.is_empty() || link.is_empty() {
        bail!("both the link and its target are needed");
    }
    Ok(format!("ln -s -- {} {}", q(target), q(link)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    Zip,
    /// A single gzip-compressed file.
    Gz,
}

impl Archive {
    /// The archive type from a file name, and the name without the suffix.
    pub fn detect(name: &str) -> Option<(Archive, &str)> {
        let lower = name.to_ascii_lowercase();
        for (suffix, kind) in [
            (".tar.gz", Archive::TarGz),
            (".tgz", Archive::TarGz),
            (".tar.bz2", Archive::TarBz2),
            (".tbz2", Archive::TarBz2),
            (".tar.xz", Archive::TarXz),
            (".txz", Archive::TarXz),
            (".tar", Archive::Tar),
            (".zip", Archive::Zip),
            (".gz", Archive::Gz),
        ] {
            if lower.ends_with(suffix) && lower.len() > suffix.len() {
                return Some((kind, &name[..name.len() - suffix.len()]));
            }
        }
        None
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Archive::Tar => ".tar",
            Archive::TarGz => ".tar.gz",
            Archive::TarBz2 => ".tar.bz2",
            Archive::TarXz => ".tar.xz",
            Archive::Zip => ".zip",
            Archive::Gz => ".gz",
        }
    }
}

/// Unpack `archive` into a new folder next to it, named after it, so nothing
/// that exists is overwritten. A single `.gz` becomes the file without the
/// suffix, which must not exist yet. Returns (command, where it goes).
pub fn extract(archive: &str) -> Result<(String, String)> {
    let Some((kind, stem)) = Archive::detect(file_name(archive)) else {
        bail!("{} is not an archive this can unpack", file_name(archive));
    };
    let dest = join(&parent(archive), stem);
    let (a, d) = (q(archive), q(&dest));
    let cmd = match kind {
        // Checked, then written with noclobber; never deletes anything, so a
        // file that appeared in between is safe too.
        Archive::Gz => format!(
            "if [ -e {d} ]; then echo \"{} already exists\" >&2; exit 1; fi; set -C; gzip -dc -- {a} > {d}",
            file_name(&dest).replace(['"', '$', '`', '\\'], "_")
        ),
        Archive::Zip => format!("mkdir -- {d} && unzip -q {a} -d {d}"),
        _ => {
            let flag = match kind {
                Archive::TarGz => "z",
                Archive::TarBz2 => "j",
                Archive::TarXz => "J",
                _ => "",
            };
            format!("mkdir -- {d} && tar -x{flag}f {a} -C {d}")
        }
    };
    Ok((cmd, dest))
}

/// Pack `names` (in `dir`) into `dir/out`, refusing to replace a file.
pub fn create(dir: &str, names: &[String], out: &str, kind: Archive) -> Result<String> {
    if names.is_empty() {
        bail!("nothing selected");
    }
    if out.contains('/') || out.is_empty() {
        bail!("the archive name must not contain /");
    }
    let items = names.iter().map(|n| q(n)).collect::<Vec<_>>().join(" ");
    let o = q(out);
    let pack = match kind {
        Archive::Zip => format!("zip -q -r {o} -- {items}"),
        Archive::Gz => bail!("use .tar.gz to pack several items"),
        _ => {
            let flag = match kind {
                Archive::TarGz => "z",
                Archive::TarBz2 => "j",
                Archive::TarXz => "J",
                _ => "",
            };
            format!("tar -c{flag}f {o} -- {items}")
        }
    };
    Ok(format!(
        "cd {} || exit 1; if [ -e {o} ]; then echo \"{} already exists\" >&2; exit 1; fi; {pack}",
        q(dir),
        out.replace(['"', '$', '`', '\\'], "_")
    ))
}

/// Search for files under `dir`. `name` is a shell pattern matched without
/// case (`*.conf`; empty means any); `contains` limits to text files holding
/// that text (case-insensitive). Stays on one filesystem, gives up after a
/// minute, and lists at most `limit` + 1 paths (so the caller can tell the
/// list was cut).
pub fn find(dir: &str, name: &str, contains: &str, max_depth: Option<u32>, limit: usize) -> Result<String> {
    if dir.is_empty() {
        bail!("choose a folder to search");
    }
    if name.is_empty() && contains.is_empty() {
        bail!("give a name pattern or text to look for");
    }
    let mut f = format!("find {} -xdev", q(dir));
    if let Some(d) = max_depth {
        f.push_str(&format!(" -maxdepth {d}"));
    }
    if !name.is_empty() {
        f.push_str(&format!(" -iname {}", q(name)));
    }
    if contains.is_empty() {
        f.push_str(" -print");
    } else {
        // -I skips binary files; the size cap keeps a search from reading logs
        // and databases for minutes.
        f.push_str(&format!(" -type f -size -10M -exec grep -l -I -i -F -- {} {{}} +", q(contains)));
    }
    Ok(format!(
        "if command -v timeout >/dev/null 2>&1; then T='timeout 60'; else T=; fi; $T {f} 2>/dev/null | head -n {}",
        limit + 1
    ))
}

/// Follow a file as it grows, starting with its last `lines` lines. `-F`
/// keeps going across log rotation.
pub fn follow(path: &str, lines: u32) -> String {
    format!("exec tail -n {lines} -F -- {}", q(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners() {
        for ok in ["www-data", "root:root", ":adm", "deploy:"] {
            assert!(check_owner(ok).is_ok(), "{ok}");
        }
        for bad in ["", ":", "a b", "x;rm", "-R", "u:-g", "$(id)"] {
            assert!(check_owner(bad).is_err(), "{bad}");
        }
        assert_eq!(
            chown(&["/a b".into()], "www-data:www-data", true).unwrap(),
            "chown -h -R -- 'www-data:www-data' '/a b'"
        );
    }

    #[test]
    fn archives() {
        assert_eq!(Archive::detect("site.tar.gz"), Some((Archive::TarGz, "site")));
        assert_eq!(Archive::detect("Backup.TGZ"), Some((Archive::TarGz, "Backup")));
        assert_eq!(Archive::detect("x.zip"), Some((Archive::Zip, "x")));
        assert_eq!(Archive::detect("access.log.gz"), Some((Archive::Gz, "access.log")));
        assert_eq!(Archive::detect(".gz"), None);
        assert_eq!(Archive::detect("notes.txt"), None);

        let (cmd, dest) = extract("/srv/it's.tar.xz").unwrap();
        assert_eq!(dest, "/srv/it's");
        assert_eq!(cmd, r"mkdir -- '/srv/it'\''s' && tar -xJf '/srv/it'\''s.tar.xz' -C '/srv/it'\''s'");
        assert!(extract("/srv/notes.txt").is_err());

        let c = create("/srv", &["a".into(), "b c".into()], "out.tar.gz", Archive::TarGz).unwrap();
        assert!(c.ends_with("tar -czf 'out.tar.gz' -- 'a' 'b c'"), "{c}");
        assert!(create("/srv", &["a".into()], "../x.tar", Archive::Tar).is_err());
    }

    #[test]
    fn searching() {
        let c = find("/etc", "*.conf", "", Some(3), 100).unwrap();
        assert!(c.contains("find '/etc' -xdev -maxdepth 3 -iname '*.conf' -print 2>/dev/null | head -n 101"), "{c}");
        let c = find("/etc", "", "listen 80", None, 10).unwrap();
        assert!(c.contains("-type f -size -10M -exec grep -l -I -i -F -- 'listen 80' {} +"), "{c}");
        assert!(find("/etc", "", "", None, 10).is_err());
        assert_eq!(follow("/var/log/x y.log", 200), "exec tail -n 200 -F -- '/var/log/x y.log'");
    }
}
