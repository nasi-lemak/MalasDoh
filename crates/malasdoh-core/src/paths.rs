//! Where MalasDoh keeps its files.
//!
//! Normal installs use the platform's config/data directories. A portable install
//! (a `malasdoh.portable` file next to the executable) keeps everything in a
//! `data` folder beside it, so the whole thing can live on a USB stick.
//! `MALASDOH_HOME` overrides both, which is also what the tests use.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl AppPaths {
    pub fn detect() -> Result<Self> {
        if let Some(home) = std::env::var_os("MALASDOH_HOME") {
            return Ok(Self::rooted(PathBuf::from(home)));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
            && dir.join("malasdoh.portable").exists()
        {
            return Ok(Self::rooted(dir.join("data")));
        }
        let dirs = directories::ProjectDirs::from("", "", "MalasDoh")
            .context("could not determine a home directory for MalasDoh's settings")?;
        Ok(Self { config_dir: dirs.config_dir().to_path_buf(), data_dir: dirs.data_dir().to_path_buf() })
    }

    /// Everything under one directory (portable mode, tests).
    pub fn rooted(root: PathBuf) -> Self {
        Self { config_dir: root.clone(), data_dir: root }
    }

    pub fn profiles_file(&self) -> PathBuf {
        self.config_dir.join("profiles.toml")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    /// Host keys the user accepted inside MalasDoh. `~/.ssh/known_hosts` is also
    /// consulted, but never written to.
    pub fn known_hosts_file(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    pub fn edit_cache_dir(&self) -> PathBuf {
        self.data_dir.join("edit")
    }
}

/// The user's `~/.ssh` directory, if there is a home directory.
pub fn user_ssh_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".ssh"))
}

/// Expand a leading `~` the way ssh does.
pub fn expand_tilde(path: &str) -> PathBuf {
    if (path == "~" || path.starts_with("~/") || path.starts_with("~\\"))
        && let Some(base) = directories::BaseDirs::new()
    {
        let rest = path[1..].trim_start_matches(['/', '\\']);
        return base.home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// Write a file by writing a sibling temp file and renaming it over the target,
/// so a crash never leaves a half-written settings file behind.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Create a directory (and parents) readable only by the current user. Backups
/// and edit copies of remote files can contain passwords and keys.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // An existing directory keeps its mode under DirBuilder; tighten it.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        // On Windows the per-user profile directory is already private.
        std::fs::create_dir_all(dir)
    }
}

/// Write a new file readable only by the current user.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents)
}

/// Delete entries directly inside `dir` not modified for `age`. Used to clean
/// up copies made for external editing. Errors are ignored: this is tidying.
pub fn remove_older_than(dir: &Path, age: std::time::Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d > age);
        if old {
            let p = e.path();
            let ok = if p.is_dir() { std::fs::remove_dir_all(&p).is_ok() } else { std::fs::remove_file(&p).is_ok() };
            removed += ok as usize;
        }
    }
    removed
}
