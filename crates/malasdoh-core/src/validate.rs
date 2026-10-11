//! "Is this config still valid?" checks that run as part of saving.
//!
//! A rule matches remote paths with a glob and names a check. Checks either run
//! on the new contents *before* anything is written (needed where a broken file
//! locks you out, like sudoers, and for plain syntax checks), or run *after*
//! writing, against the real config tree, with the original put back if the
//! check fails (`nginx -t` has to see the file in place).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CheckWhen {
    /// Check a temporary copy; nothing is written unless it passes. `{file}` is
    /// the temporary copy.
    BeforeWrite,
    /// Write, then check; restore the original if the check fails. `{file}` is
    /// the real path.
    #[default]
    AfterWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationRule {
    pub name: String,
    /// Glob on the remote path. Without a `/` it matches the file name only
    /// (`*.json`); with one, the whole path (`/etc/nginx/**`). `*` stays within
    /// one path segment, `**` crosses them. Several globs can be separated by
    /// `|`.
    pub pattern: String,
    /// Shell command run on the server, or `builtin:json` / `builtin:toml` /
    /// `builtin:yaml` for a local syntax check. `{file}` is replaced by the
    /// quoted path. A command that is not installed (exit status 127) does not
    /// block the save; it is reported as not checked.
    pub check: String,
    pub when: CheckWhen,
    /// Run after a passing check, e.g. `systemctl reload nginx`. Empty: none.
    /// A failing reload is reported but does not undo the save.
    pub reload: String,
    pub enabled: bool,
}

impl Default for ValidationRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            pattern: String::new(),
            check: String::new(),
            when: CheckWhen::AfterWrite,
            reload: String::new(),
            enabled: true,
        }
    }
}

fn rule(name: &str, pattern: &str, check: &str, when: CheckWhen) -> ValidationRule {
    ValidationRule { name: name.into(), pattern: pattern.into(), check: check.into(), when, ..Default::default() }
}

/// Checks for the configs people most often break.
pub fn builtin_rules() -> Vec<ValidationRule> {
    use CheckWhen::*;
    vec![
        rule("sudoers", "/etc/sudoers|/etc/sudoers.d/*", "visudo -cf {file}", BeforeWrite),
        rule("sshd (main config)", "/etc/ssh/sshd_config", "sshd -t -f {file}", BeforeWrite),
        rule("sshd (drop-in)", "/etc/ssh/sshd_config.d/**", "sshd -t", AfterWrite),
        rule("nginx", "/etc/nginx/**", "nginx -t", AfterWrite),
        rule("Apache", "/etc/apache2/**|/etc/httpd/**", "apachectl configtest", AfterWrite),
        rule("HAProxy", "/etc/haproxy/**", "haproxy -c -f /etc/haproxy/haproxy.cfg", AfterWrite),
        rule("BIND", "/etc/bind/**|/etc/named.conf|/etc/named/**", "named-checkconf", AfterWrite),
        rule("systemd unit", "/etc/systemd/**", "systemd-analyze verify {file}", AfterWrite),
        rule("fstab", "/etc/fstab", "findmnt --verify --tab-file {file}", BeforeWrite),
        rule(
            "PostgreSQL",
            "/etc/postgresql/**|postgresql.conf|postgresql.auto.conf|pg_hba.conf|pg_ident.conf",
            crate::pgconf::CHECK,
            AfterWrite,
        ),
        rule("Caddy", "/etc/caddy/Caddyfile", "caddy validate --adapter caddyfile --config {file}", BeforeWrite),
        rule("JSON syntax", "*.json", "builtin:json", BeforeWrite),
        rule("TOML syntax", "*.toml", "builtin:toml", BeforeWrite),
        rule("YAML syntax", "*.yml|*.yaml", "builtin:yaml", BeforeWrite),
    ]
}

/// The first enabled rule that matches `path`.
pub fn find_rule<'a>(rules: &'a [ValidationRule], path: &str) -> Option<&'a ValidationRule> {
    rules.iter().find(|r| r.enabled && !r.check.trim().is_empty() && matches_path(&r.pattern, path))
}

pub fn matches_path(pattern: &str, path: &str) -> bool {
    pattern.split('|').map(str::trim).filter(|p| !p.is_empty()).any(|p| {
        if p.contains('/') {
            glob(p.as_bytes(), path.as_bytes())
        } else {
            glob(p.as_bytes(), crate::sftp::file_name(path).as_bytes())
        }
    })
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        [b'*', rest @ ..] => {
            let max = s.iter().position(|&c| c == b'/').unwrap_or(s.len());
            (0..=max).any(|i| glob(rest, &s[i..]))
        }
        [b'?', rest @ ..] => !s.is_empty() && s[0] != b'/' && glob(rest, &s[1..]),
        [c, rest @ ..] => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

/// A local syntax check, for `builtin:` rules. `Err` is the parser's message.
pub fn builtin_check(kind: &str, text: &str) -> Option<Result<(), String>> {
    Some(match kind {
        "builtin:json" => serde_json::from_str::<serde_json::Value>(text).map(|_| ()).map_err(|e| e.to_string()),
        "builtin:toml" => text.parse::<toml::Table>().map(|_| ()).map_err(|e| e.to_string()),
        "builtin:yaml" => yaml_rust2::YamlLoader::load_from_str(text).map(|_| ()).map_err(|e| e.to_string()),
        _ => return None,
    })
}

/// The command line for a rule, with `{file}` filled in. The PostgreSQL
/// built-ins stand for scripts (see [`crate::pgconf`]).
pub fn command_for(template: &str, path: &str) -> String {
    match template.trim() {
        crate::pgconf::CHECK => crate::pgconf::check_script(path),
        crate::pgconf::RELOAD => crate::pgconf::reload_script(path),
        _ => template.replace("{file}", &crate::ssh::shell_quote(path)),
    }
}

/// A command as people should read it: the built-ins say what they do.
pub fn describe(command: &str) -> String {
    match command.trim() {
        crate::pgconf::CHECK => {
            "`postgres -C` on the whole configuration (and, for pg_hba.conf, the running server)".into()
        }
        crate::pgconf::RELOAD => "a PostgreSQL reload".into(),
        "builtin:json" | "builtin:toml" | "builtin:yaml" => "a syntax check".into(),
        c => format!("`{c}`"),
    }
}

/// Built-ins that run on the server and explain themselves: their output is
/// worth showing even when they pass.
pub fn speaks_for_itself(command: &str) -> bool {
    matches!(command.trim(), crate::pgconf::CHECK | crate::pgconf::RELOAD)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Passed {
        output: String,
    },
    /// The check command is not installed on the server.
    Unavailable {
        output: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(matches_path("/etc/nginx/**", "/etc/nginx/nginx.conf"));
        assert!(matches_path("/etc/nginx/**", "/etc/nginx/sites-enabled/shop"));
        assert!(!matches_path("/etc/nginx/**", "/etc/nginxx/a"));
        assert!(matches_path("/etc/sudoers|/etc/sudoers.d/*", "/etc/sudoers.d/90-ops"));
        assert!(!matches_path("/etc/sudoers.d/*", "/etc/sudoers.d/a/b"));
        assert!(matches_path("*.json", "/srv/app/config.json"));
        assert!(!matches_path("*.json", "/srv/app/config.json.bak"));
        assert!(matches_path("*.yml|*.yaml", "/x/compose.yaml"));
    }

    #[test]
    fn first_enabled_rule_wins() {
        let mut rules = builtin_rules();
        assert_eq!(find_rule(&rules, "/etc/sudoers").unwrap().name, "sudoers");
        assert_eq!(find_rule(&rules, "/etc/ssh/sshd_config").unwrap().when, CheckWhen::BeforeWrite);
        assert_eq!(find_rule(&rules, "/etc/ssh/sshd_config.d/10.conf").unwrap().check, "sshd -t");
        assert!(find_rule(&rules, "/home/me/notes.txt").is_none());
        assert_eq!(find_rule(&rules, "/etc/postgresql/17/main/conf.d/tuning.conf").unwrap().name, "PostgreSQL");
        assert_eq!(find_rule(&rules, "/var/lib/pgsql/16/data/pg_hba.conf").unwrap().name, "PostgreSQL");
        rules.iter_mut().find(|r| r.name == "nginx").unwrap().enabled = false;
        assert!(find_rule(&rules, "/etc/nginx/nginx.conf").is_none());
    }

    #[test]
    fn builtin_checks() {
        assert_eq!(builtin_check("builtin:json", r#"{"a": 1}"#), Some(Ok(())));
        assert!(matches!(builtin_check("builtin:json", r#"{"a": 1,}"#), Some(Err(_))));
        assert_eq!(builtin_check("builtin:toml", "a = 1\n"), Some(Ok(())));
        assert!(matches!(builtin_check("builtin:toml", "a = \n"), Some(Err(_))));
        assert_eq!(builtin_check("builtin:yaml", "a:\n  - 1\n"), Some(Ok(())));
        assert!(matches!(builtin_check("builtin:yaml", "a: [1, 2\n"), Some(Err(_))));
        assert_eq!(builtin_check("nginx -t", ""), None);
    }

    #[test]
    fn file_placeholder_is_quoted() {
        assert_eq!(command_for("visudo -cf {file}", "/tmp/it's"), r"visudo -cf '/tmp/it'\''s'");
    }
}
