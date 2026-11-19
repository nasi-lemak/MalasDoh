//! User preferences, in `settings.toml`.

use std::path::Path;

use anyhow::{Context, Result};
use malasdoh_core::validate::{ValidationRule, builtin_rules};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub terminal_font_size: f32,
    pub editor_font_size: f32,
    /// Show the diff and ask before every save.
    pub review_before_save: bool,
    /// Command for "Open in external editor". `{file}` is replaced by the path;
    /// without it the path is appended. Empty means the system default.
    pub external_editor: String,
    /// Colours of the whole app: a theme's name (see `look.rs`), or
    /// "Follow system". Empty only in files from before themes, which had
    /// `dark_mode` instead.
    pub ui_theme: String,
    /// With "Follow system": the themes for the system's dark and light mode.
    pub system_dark_theme: String,
    pub system_light_theme: String,
    /// "#rrggbb" in place of the theme's accent colour; empty: the theme's.
    pub accent: String,
    /// Before themes: dark (true) or light. Read once, to choose a theme.
    #[serde(skip_serializing)]
    pub dark_mode: Option<bool>,
    /// Ask before deleting remote files.
    pub confirm_delete: bool,
    /// Terminal colours, by name (see `themes.rs`), or "Same as the app".
    pub terminal_theme: String,
    /// A TTF/OTF file used for the terminal and editor. Empty: the built-in
    /// monospace font.
    pub mono_font_file: String,
    /// Copy text to the clipboard as soon as it is selected in a terminal.
    pub copy_on_select: bool,
    /// Ask before pasting several lines into a shell.
    pub confirm_paste: bool,
    /// Reconnect to what was open last time when MalasDoh starts. Off: the
    /// start screen offers it instead.
    pub reopen_on_start: bool,
    /// Config checks run when saving matching files; first match wins.
    pub validation_rules: Vec<ValidationRule>,
    /// Names of the built-in checks this settings file has been offered, so
    /// checks added in later versions are added once, and deleted ones stay
    /// deleted. Empty in files written before it existed.
    #[serde(default)]
    pub builtin_rules_seen: Vec<String>,
    /// Uploads, downloads and folder sync.
    pub transfer: malasdoh_core::transfer::TransferOptions,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal_font_size: 14.0,
            editor_font_size: 14.0,
            review_before_save: true,
            external_editor: String::new(),
            ui_theme: "Dark".into(),
            system_dark_theme: "Dark".into(),
            system_light_theme: "Light".into(),
            accent: String::new(),
            dark_mode: None,
            confirm_delete: true,
            reopen_on_start: false,
            terminal_theme: crate::look::SAME_AS_APP.into(),
            mono_font_file: String::new(),
            copy_on_select: false,
            confirm_paste: true,
            validation_rules: builtin_rules(),
            builtin_rules_seen: builtin_rules().into_iter().map(|r| r.name).collect(),
            transfer: Default::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut s = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        s.add_new_builtin_rules();
        s.choose_theme_from_dark_mode();
        s
    }

    /// A settings file from before themes: its "Dark mode" becomes the Dark
    /// or Light theme, and a dark terminal left at the old default follows
    /// the app (which looks the same).
    fn choose_theme_from_dark_mode(&mut self) {
        if !self.ui_theme.is_empty() && self.dark_mode.is_none() {
            return;
        }
        let dark = self.dark_mode.take().unwrap_or(true);
        if self.ui_theme.is_empty() || self.ui_theme == "Dark" {
            self.ui_theme = if dark { "Dark" } else { "Light" }.into();
            if dark && self.terminal_theme == "Tomorrow Night" {
                self.terminal_theme = crate::look::SAME_AS_APP.into();
            }
        }
    }

    /// Add built-in checks that are newer than this settings file.
    fn add_new_builtin_rules(&mut self) {
        // Files from before the list was kept had been offered these.
        const FIRST: &[&str] = &[
            "sudoers",
            "sshd (main config)",
            "sshd (drop-in)",
            "nginx",
            "Apache",
            "HAProxy",
            "BIND",
            "systemd unit",
            "fstab",
            "Caddy",
            "JSON syntax",
            "TOML syntax",
            "YAML syntax",
        ];
        if self.builtin_rules_seen.is_empty() {
            self.builtin_rules_seen = FIRST.iter().map(|s| s.to_string()).collect();
        }
        for r in builtin_rules() {
            if self.builtin_rules_seen.contains(&r.name) {
                continue;
            }
            self.builtin_rules_seen.push(r.name.clone());
            if !self.validation_rules.iter().any(|x| x.name == r.name) {
                self.validation_rules.push(r);
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding settings")?;
        malasdoh_core::paths::write_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_builtin_checks_are_added_once() {
        // An older file: no list, and the user deleted the nginx rule.
        let mut s = Settings::default();
        s.builtin_rules_seen.clear();
        s.validation_rules.retain(|r| r.name != "nginx" && r.name != "PostgreSQL");
        s.add_new_builtin_rules();
        assert!(s.validation_rules.iter().any(|r| r.name == "PostgreSQL"));
        assert!(!s.validation_rules.iter().any(|r| r.name == "nginx"));
        // Deleting it afterwards sticks.
        s.validation_rules.retain(|r| r.name != "PostgreSQL");
        s.add_new_builtin_rules();
        assert!(!s.validation_rules.iter().any(|r| r.name == "PostgreSQL"));
    }

    #[test]
    fn dark_mode_from_before_themes_becomes_a_theme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let old = Settings { terminal_theme: "Tomorrow Night".into(), ..Default::default() };
        let text = toml::to_string(&old).unwrap().replace("ui_theme = \"Dark\"\n", "");
        // Dark, with the old default terminal: looks the same as before.
        std::fs::write(&path, format!("dark_mode = true\n{text}")).unwrap();
        let s = Settings::load(&path);
        assert_eq!((s.ui_theme.as_str(), s.terminal_theme.as_str()), ("Dark", crate::look::SAME_AS_APP));
        // Light keeps its terminal colours.
        std::fs::write(&path, format!("dark_mode = false\n{text}")).unwrap();
        let s = Settings::load(&path);
        assert_eq!((s.ui_theme.as_str(), s.terminal_theme.as_str()), ("Light", "Tomorrow Night"));
        // Saved again, the old key is gone and the theme stays.
        s.save(&path).unwrap();
        let again = std::fs::read_to_string(&path).unwrap();
        assert!(!again.contains("dark_mode") && again.contains("ui_theme = \"Light\""), "{again}");
        assert_eq!(Settings::load(&path).ui_theme, "Light");
    }
}
