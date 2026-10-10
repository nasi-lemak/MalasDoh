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
