//! Importing connections from `~/.ssh/config`, so hosts the user already set up
//! for `ssh` appear in MalasDoh without retyping them.
//!
//! Only concrete `Host` entries are imported (patterns like `*.corp` are
//! defaults, not machines). Settings from wildcard blocks that match a host are
//! applied to it, first value wins, as ssh does. `Match` blocks are skipped.

use std::collections::HashMap;
use std::path::Path;

use crate::profile::{AuthMethod, Profile, ProfileId};

#[derive(Debug, Default, Clone)]
struct Block {
    patterns: Vec<String>,
    options: Vec<(String, String)>,
}

fn parse(text: &str) -> Vec<Block> {
    let mut blocks = vec![Block { patterns: vec!["*".into()], options: vec![] }];
    let mut in_match = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_lowercase(), v.trim().trim_start_matches('=').trim().to_string()),
            None => continue,
        };
        let value = value.trim_matches('"').to_string();
        match key.as_str() {
            "host" => {
                in_match = false;
                blocks
                    .push(Block { patterns: value.split_whitespace().map(str::to_string).collect(), options: vec![] });
            }
            "match" => in_match = true,
            _ if in_match => {}
            _ => blocks.last_mut().unwrap().options.push((key, value)),
        }
    }
    blocks
}

fn matches(patterns: &[String], alias: &str) -> bool {
    let mut hit = false;
    for p in patterns {
        if let Some(neg) = p.strip_prefix('!') {
            if glob(neg, alias) {
                return false;
            }
        } else if glob(p, alias) {
            hit = true;
        }
    }
    hit
}

fn glob(pattern: &str, s: &str) -> bool {
    fn inner(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], s) || (!s.is_empty() && inner(p, &s[1..])),
            (Some(b'?'), Some(_)) => inner(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => inner(&p[1..], &s[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), s.as_bytes())
}

fn is_concrete(alias: &str) -> bool {
    !alias.contains(['*', '?', '!'])
}

/// Profiles for every concrete host in the file. `ProxyJump` to another
/// imported alias becomes a jump link; anything else (a `user@host:port`
/// string, a chain) is noted in the profile's notes instead of guessed at.
pub fn import(text: &str, default_user: &str) -> Vec<Profile> {
    let blocks = parse(text);
    let aliases: Vec<String> =
        blocks.iter().skip(1).flat_map(|b| b.patterns.iter().filter(|p| is_concrete(p)).cloned()).collect();
    let mut ids: HashMap<String, ProfileId> = HashMap::new();
    let mut out = Vec::new();
    let mut jumps = Vec::new();
    for alias in aliases {
        if ids.contains_key(&alias) {
            continue;
        }
        let mut opts: HashMap<String, String> = HashMap::new();
        for b in &blocks {
            if matches(&b.patterns, &alias) {
                for (k, v) in &b.options {
                    opts.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        let mut p = Profile {
            name: alias.clone(),
            group: "Imported from ssh config".into(),
            host: opts.get("hostname").cloned().unwrap_or_else(|| alias.clone()).replace("%h", &alias),
            port: opts.get("port").and_then(|v| v.parse().ok()).unwrap_or(22),
            user: opts.get("user").cloned().unwrap_or_else(|| default_user.to_string()),
            ..Default::default()
        };
        if let Some(key) = opts.get("identityfile") {
            p.auth = AuthMethod::Key { path: key.clone() };
        }
        if let Some(j) = opts.get("proxyjump").filter(|j| !j.eq_ignore_ascii_case("none")) {
            jumps.push((p.id, j.clone()));
        }
        ids.insert(alias, p.id);
        out.push(p);
    }
    for (id, jump) in jumps {
        let target = ids.get(&jump).copied();
        let p = out.iter_mut().find(|p| p.id == id).unwrap();
        match target {
            Some(t) if t != id => p.jump = Some(t),
            _ => p.notes = format!("ssh config has ProxyJump {jump}; set the jump host here by hand."),
        }
    }
    out
}

pub fn import_file(path: &Path, default_user: &str) -> std::io::Result<Vec<Profile>> {
    Ok(import(&std::fs::read_to_string(path)?, default_user))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# defaults
Host *.corp
    User ops
    Port 2200

Host bastion
    HostName bastion.example.com
    User jump

Host web1.corp db1.corp
    IdentityFile ~/.ssh/corp_ed25519
    ProxyJump bastion

Host legacy
    HostName=10.1.2.3
    ProxyJump admin@other:2222

Match host foo
    User shouldnotapply

Host *
    User fallback
"#;

    #[test]
    fn imports_concrete_hosts_with_inherited_options() {
        let ps = import(CONFIG, "me");
        let names: Vec<_> = ps.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["bastion", "web1.corp", "db1.corp", "legacy"]);

        let bastion = &ps[0];
        assert_eq!((bastion.host.as_str(), bastion.user.as_str(), bastion.port), ("bastion.example.com", "jump", 22));

        let web = &ps[1];
        assert_eq!((web.host.as_str(), web.user.as_str(), web.port), ("web1.corp", "ops", 2200));
        assert_eq!(web.auth, AuthMethod::Key { path: "~/.ssh/corp_ed25519".into() });
        assert_eq!(web.jump, Some(bastion.id));

        let legacy = &ps[3];
        assert_eq!((legacy.host.as_str(), legacy.user.as_str()), ("10.1.2.3", "fallback"));
        assert_eq!(legacy.jump, None);
        assert!(legacy.notes.contains("admin@other:2222"));
    }
}
