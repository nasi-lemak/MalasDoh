//! Import saved sessions from PuTTY and WinSCP, so moving over is painless.
//!
//! Both keep a session as named fields (`HostName`, `PortNumber`,
//! `UserName`, `PublicKeyFile`, …): in the Windows registry, or in files
//! (`~/.putty/sessions/*` on Linux, `WinSCP.ini`). Each source is read into
//! fields, and [`from_fields`] turns them into a [`Profile`]. Saved passwords
//! are not imported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::profile::{AuthMethod, Profile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Putty,
    WinScp,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Putty => "PuTTY",
            Source::WinScp => "WinSCP",
        }
    }
}

/// `%20` and friends, as both programs write names and paths.
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A profile from one saved session, or `None` for something that is not an
/// SSH/SFTP session (FTP, Telnet, defaults, no host).
pub fn from_fields(source: Source, raw_name: &str, f: &HashMap<String, String>) -> Option<Profile> {
    let name = unescape(raw_name);
    if name.is_empty() || name == "Default Settings" {
        return None;
    }
    let get = |k: &str| f.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    match source {
        Source::Putty => {
            if get("Protocol").is_some_and(|p| !p.eq_ignore_ascii_case("ssh")) {
                return None;
            }
        }
        Source::WinScp => {
            // 0 SCP, 1 SFTP with SCP fallback, 2 SFTP; 5 FTP, 6 WebDAV, 7 S3.
            if get("FSProtocol").is_some_and(|p| !matches!(p, "0" | "1" | "2")) {
                return None;
            }
        }
    }
    let mut host = unescape(get("HostName")?);
    let mut user = get("UserName").map(unescape).unwrap_or_default();
    // PuTTY accepts "user@host" in the host field.
    if let Some((u, h)) = host.clone().rsplit_once('@') {
        if user.is_empty() {
            user = u.to_string();
        }
        host = h.to_string();
    }
    let port = get("PortNumber").and_then(|p| p.parse().ok()).filter(|p| *p != 0).unwrap_or(22);
    // WinSCP folders are part of the name: "Customers/Acme/web-1".
    let (group, short) = match (source, name.rsplit_once('/')) {
        (Source::WinScp, Some((folder, n))) => (format!("WinSCP / {}", folder.replace('/', " / ")), n.to_string()),
        _ => (format!("Imported from {}", source.label()), name.clone()),
    };
    let mut p = Profile { name: short, group, host, port, user, ..Default::default() };
    if let Some(key) = get("PublicKeyFile") {
        p.auth = AuthMethod::Key { path: unescape(key) };
    }
    if source == Source::WinScp
        && let Some(dir) = get("RemoteDirectory")
    {
        p.start_dir = unescape(dir);
    }
    Some(p)
}

/// `[Sessions\name]` sections of a `WinSCP.ini`.
pub fn parse_winscp_ini(text: &str) -> Vec<Profile> {
    let mut out = Vec::new();
    let mut current: Option<(String, HashMap<String, String>)> = None;
    let flush = |c: Option<(String, HashMap<String, String>)>, out: &mut Vec<Profile>| {
        if let Some((name, fields)) = c
            && let Some(p) = from_fields(Source::WinScp, &name, &fields)
        {
            out.push(p);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(current.take(), &mut out);
            current = section.strip_prefix("Sessions\\").map(|n| (n.to_string(), HashMap::new()));
        } else if let Some((k, v)) = line.split_once('=')
            && let Some((_, fields)) = &mut current
        {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    flush(current, &mut out);
    out
}

/// One file from `~/.putty/sessions` (PuTTY on Linux and macOS).
pub fn parse_putty_file(file_name: &str, text: &str) -> Option<Profile> {
    let fields = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    from_fields(Source::Putty, file_name, &fields)
}

pub fn putty_sessions_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".putty").join("sessions"))
}

/// Where WinSCP keeps its ini file when it is not using the registry.
pub fn winscp_ini_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        v.push(PathBuf::from(appdata).join("WinSCP.ini"));
    }
    v
}

pub fn read_putty_dir(dir: &Path) -> Result<Vec<Profile>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        if let Ok(text) = std::fs::read_to_string(e.path())
            && let Some(p) = parse_putty_file(&e.file_name().to_string_lossy(), &text)
        {
            out.push(p);
        }
    }
    Ok(out)
}

/// Sessions from PuTTY: the registry on Windows, `~/.putty/sessions`
/// elsewhere.
pub fn putty() -> Result<Vec<Profile>> {
    #[cfg(windows)]
    {
        registry(Source::Putty, r"Software\SimonTatham\PuTTY\Sessions")
    }
    #[cfg(not(windows))]
    {
        match putty_sessions_dir() {
            Some(d) if d.is_dir() => read_putty_dir(&d),
            _ => Ok(Vec::new()),
        }
    }
}

/// Sessions from WinSCP: the registry (its default) on Windows, plus the
/// ini file in its usual place.
pub fn winscp() -> Result<Vec<Profile>> {
    #[allow(unused_mut)]
    let mut out = Vec::new();
    #[cfg(windows)]
    out.extend(registry(Source::WinScp, r"Software\Martin Prikryl\WinSCP 2\Sessions")?);
    for p in winscp_ini_candidates() {
        if let Ok(text) = std::fs::read_to_string(&p) {
            out.extend(parse_winscp_ini(&text));
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn registry(source: Source, path: &str) -> Result<Vec<Profile>> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, REG_DWORD};
    let root = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(path) {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading HKCU\\{path}")),
    };
    let mut out = Vec::new();
    for name in root.enum_keys().filter_map(|k| k.ok()) {
        let Ok(key) = root.open_subkey(&name) else { continue };
        let mut fields = HashMap::new();
        for (k, v) in key.enum_values().filter_map(|x| x.ok()) {
            let value = if v.vtype == REG_DWORD && v.bytes.len() >= 4 {
                u32::from_le_bytes([v.bytes[0], v.bytes[1], v.bytes[2], v.bytes[3]]).to_string()
            } else {
                v.to_string()
            };
            fields.insert(k, value);
        }
        if let Some(p) = from_fields(source, &name, &fields) {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winscp_ini_sessions() {
        let ini = "[Configuration\\Interface]\nFoo=1\n\
            [Sessions\\Default%20Settings]\nHostName=x\n\
            [Sessions\\Customers/Acme/web%201]\nHostName=10.0.0.5\nPortNumber=2222\nUserName=deploy\n\
            PublicKeyFile=C:%5CUsers%5Cme%5Cacme.ppk\nRemoteDirectory=/var/www\n\
            [Sessions\\ftp-box]\nHostName=ftp.example\nFSProtocol=5\n\
            [Sessions\\plain]\nHostName=plain.example\n";
        let found = parse_winscp_ini(ini);
        assert_eq!(found.len(), 2, "{found:?}");
        let p = &found[0];
        assert_eq!((p.name.as_str(), p.group.as_str()), ("web 1", "WinSCP / Customers / Acme"));
        assert_eq!((p.host.as_str(), p.port, p.user.as_str()), ("10.0.0.5", 2222, "deploy"));
        assert_eq!(p.auth, AuthMethod::Key { path: r"C:\Users\me\acme.ppk".into() });
        assert_eq!(p.start_dir, "/var/www");
        assert_eq!((found[1].name.as_str(), found[1].port), ("plain", 22));
    }

    #[test]
    fn putty_session_files() {
        let p = parse_putty_file("db%20primary", "Protocol=ssh\nHostName=admin@db.example\nPortNumber=22\n").unwrap();
        assert_eq!((p.name.as_str(), p.host.as_str(), p.user.as_str()), ("db primary", "db.example", "admin"));
        assert_eq!(p.group, "Imported from PuTTY");
        assert!(parse_putty_file("router", "Protocol=telnet\nHostName=r\n").is_none());
        assert!(parse_putty_file("Default%20Settings", "HostName=x\n").is_none());
        assert!(parse_putty_file("empty", "Protocol=ssh\nHostName=\n").is_none());
    }
}
