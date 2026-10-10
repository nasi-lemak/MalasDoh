//! Host key verification against OpenSSH-format `known_hosts` files.
//!
//! MalasDoh reads the user's `~/.ssh/known_hosts` (so hosts already trusted by
//! `ssh` are trusted here too) and its own file, and only ever writes its own.
//! The parser understands what OpenSSH writes: comma-separated host patterns
//! with `*`/`?` wildcards and `!` negation, `[host]:port` entries, hashed
//! (`|1|salt|hash`) hosts, and `@revoked` / `@cert-authority` markers. A line it
//! cannot understand is skipped rather than failing the whole check.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use russh::keys::ssh_key::{self, HashAlg, PublicKey};
use sha1::Sha1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is on record for this host.
    Trusted,
    /// Nothing on record for this host. `other_keys` lists algorithms that *are*
    /// recorded, which usually means the server rotated or added a key type.
    Unknown { other_keys: Vec<String> },
    /// A key of the same type is recorded and it is different. This is what a
    /// man-in-the-middle looks like; never connect silently.
    Changed { recorded_in: PathBuf, line: usize, recorded_fingerprint: String },
    /// The key is explicitly marked `@revoked`.
    Revoked { recorded_in: PathBuf, line: usize },
}

#[derive(Debug, Clone)]
pub struct KnownHosts {
    /// Files consulted, in order. The last one is where new keys are learnt.
    read: Vec<PathBuf>,
    write: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

#[derive(Debug)]
struct Entry {
    line: usize,
    marker: Marker,
    patterns: String,
    key: PublicKey,
}

impl KnownHosts {
    pub fn new(app_file: PathBuf, also_read: Vec<PathBuf>) -> Self {
        let mut read = also_read;
        read.push(app_file.clone());
        Self { read, write: app_file }
    }

    /// The usual setup: `~/.ssh/known_hosts` plus MalasDoh's own file.
    pub fn standard(app_file: PathBuf) -> Self {
        let user = crate::paths::user_ssh_dir().map(|d| d.join("known_hosts")).into_iter().collect();
        Self::new(app_file, user)
    }

    pub fn app_file(&self) -> &Path {
        &self.write
    }

    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let name = host_port(host, port);
        let mut trusted = false;
        let mut changed = None;
        let mut other_keys = Vec::new();
        for file in &self.read {
            for e in read_entries(file) {
                if e.marker == Marker::CertAuthority || !host_matches(&name, &e.patterns) {
                    continue;
                }
                let same_key = e.key.key_data() == key.key_data();
                if e.marker == Marker::Revoked {
                    if same_key {
                        return HostKeyStatus::Revoked { recorded_in: file.clone(), line: e.line };
                    }
                    continue;
                }
                if same_key {
                    trusted = true;
                } else if e.key.algorithm() == key.algorithm() {
                    changed.get_or_insert(HostKeyStatus::Changed {
                        recorded_in: file.clone(),
                        line: e.line,
                        recorded_fingerprint: fingerprint(&e.key),
                    });
                } else {
                    other_keys.push(e.key.algorithm().to_string());
                }
            }
        }
        // A stale same-type key elsewhere does not override an exact match: that
        // is how OpenSSH behaves when a key was re-learnt in a second file.
        if trusted {
            HostKeyStatus::Trusted
        } else if let Some(c) = changed {
            c
        } else {
            other_keys.sort();
            other_keys.dedup();
            HostKeyStatus::Unknown { other_keys }
        }
    }

    /// CA keys trusted (`@cert-authority`) to sign host certificates for this host.
    pub fn trusted_cas(&self, host: &str, port: u16) -> Vec<PublicKey> {
        let name = host_port(host, port);
        let mut out = Vec::new();
        for file in &self.read {
            for e in read_entries(file) {
                if e.marker == Marker::CertAuthority && host_matches(&name, &e.patterns) {
                    out.push(e.key);
                }
            }
        }
        out
    }

    fn is_revoked(&self, key: &ssh_key::public::KeyData) -> bool {
        self.read.iter().flat_map(|f| read_entries(f)).any(|e| e.marker == Marker::Revoked && e.key.key_data() == key)
    }

    /// Accept a host certificate the way OpenSSH does: signed by a CA trusted
    /// for this host, currently valid, a *host* certificate, naming this host
    /// among its principals (or none), with no critical options, and neither
    /// the host key nor the CA revoked. `Err` says why not.
    pub fn check_certificate(&self, host: &str, port: u16, cert: &ssh_key::Certificate) -> Result<(), String> {
        if self.is_revoked(cert.public_key()) || self.is_revoked(cert.signature_key()) {
            return Err("the certificate's key or its CA is marked @revoked".into());
        }
        let cas = self.trusted_cas(host, port);
        if cas.is_empty() {
            return Err("no @cert-authority is trusted for this host".into());
        }
        let fps: Vec<_> = cas.iter().map(|k| k.fingerprint(HashAlg::Sha256)).collect();
        cert.validate(fps.iter())
            .map_err(|_| "not signed by a trusted CA, or outside its validity period".to_string())?;
        if cert.cert_type() != ssh_key::certificate::CertType::Host {
            return Err("this is a user certificate, not a host certificate".into());
        }
        let principals = cert.valid_principals();
        if !principals.is_empty() && !principals.iter().any(|p| p.eq_ignore_ascii_case(host)) {
            return Err(format!("the certificate is for {}, not {host}", principals.join(", ")));
        }
        if !cert.critical_options().is_empty() {
            return Err("the certificate has critical options this client does not understand".into());
        }
        Ok(())
    }

    /// Algorithms already recorded for the host, so negotiation can prefer them
    /// (otherwise a server offering a new key type looks like an unknown host).
    pub fn known_algorithms(&self, host: &str, port: u16) -> Vec<ssh_key::Algorithm> {
        let name = host_port(host, port);
        let mut algs = Vec::new();
        for file in &self.read {
            for e in read_entries(file) {
                if e.marker == Marker::None && host_matches(&name, &e.patterns) && !algs.contains(&e.key.algorithm()) {
                    algs.push(e.key.algorithm());
                }
            }
        }
        algs
    }

    /// Trust `key` for the host from now on.
    pub fn learn(&self, host: &str, port: u16, key: &PublicKey) -> Result<()> {
        if let Some(dir) = self.write.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let existing = std::fs::read(&self.write).unwrap_or_default();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.write)
            .with_context(|| format!("opening {}", self.write.display()))?;
        if !existing.is_empty() && !existing.ends_with(b"\n") {
            f.write_all(b"\n")?;
        }
        let key_text = key.to_openssh().context("encoding host key")?;
        // Drop the comment ssh-key may carry; known_hosts lines are host, type, key.
        let mut parts = key_text.split_whitespace();
        let (alg, data) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        writeln!(f, "{} {alg} {data}", host_port(host, port))?;
        Ok(())
    }

    /// Remove every entry for the host from MalasDoh's own file (used after the
    /// user confirms a key change). Entries in `~/.ssh/known_hosts` are left for
    /// `ssh-keygen -R`, since that file belongs to OpenSSH.
    pub fn forget(&self, host: &str, port: u16) -> Result<usize> {
        let name = host_port(host, port);
        let text = match std::fs::read_to_string(&self.write) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut removed = 0;
        let mut kept = String::with_capacity(text.len());
        for line in text.lines() {
            let drop = parse_line(line)
                .is_some_and(|(marker, patterns, _)| marker == Marker::None && host_matches(&name, patterns));
            if drop {
                removed += 1;
            } else {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        crate::paths::write_atomic(&self.write, kept.as_bytes())?;
        Ok(removed)
    }
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

fn host_port(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

fn read_entries(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let (marker, patterns, key) = parse_line(line)?;
            let key = PublicKey::from_openssh(key).ok()?;
            Some(Entry { line: i + 1, marker, patterns: patterns.to_string(), key })
        })
        .collect()
}

/// Split a line into (marker, host patterns, "type base64").
fn parse_line(line: &str) -> Option<(Marker, &str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (first, rest) = split_field(line)?;
    let (marker, patterns, rest) = match first {
        "@revoked" => {
            let (p, r) = split_field(rest)?;
            (Marker::Revoked, p, r)
        }
        "@cert-authority" => {
            let (p, r) = split_field(rest)?;
            (Marker::CertAuthority, p, r)
        }
        m if m.starts_with('@') => return None,
        p => (Marker::None, p, rest),
    };
    let (alg, rest) = split_field(rest)?;
    let data = rest.split_whitespace().next()?;
    // Re-join without the comment; from_openssh wants "alg data".
    let key_start = alg.as_ptr() as usize - line.as_ptr() as usize;
    let key_end = data.as_ptr() as usize - line.as_ptr() as usize + data.len();
    Some((marker, patterns, &line[key_start..key_end]))
}

fn split_field(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(i) => Some((&s[..i], &s[i..])),
        None => Some((s, "")),
    }
}

/// OpenSSH semantics: any positive pattern must match and no negated one may.
fn host_matches(name: &str, patterns: &str) -> bool {
    let mut matched = false;
    for pat in patterns.split(',') {
        if let Some(neg) = pat.strip_prefix('!') {
            if wildcard_match(neg, name) {
                return false;
            }
        } else if pat.starts_with("|1|") {
            matched |= hashed_match(pat, name);
        } else {
            matched |= wildcard_match(pat, name);
        }
    }
    matched
}

fn hashed_match(pat: &str, name: &str) -> bool {
    let mut parts = pat.split('|').skip(2);
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (B64.decode(salt), B64.decode(hash)) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(name.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// Case-insensitive glob with `*` and `?`, as hostnames are.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAID08X9HYCVKh/5/fyLWPZV7meOo4sTMyDrTUBkW/T9E0";
    const ED_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH+XELadQ0/njJ2XASrMTCopINQzOuQz1LudoB1BiToO";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn setup(user_file: &str) -> (tempfile::TempDir, KnownHosts) {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user_known_hosts");
        std::fs::write(&user, user_file).unwrap();
        let kh = KnownHosts::new(dir.path().join("app_known_hosts"), vec![user]);
        (dir, kh)
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "web.example.com"));
        assert!(wildcard_match("web?", "WEB1"));
        assert!(!wildcard_match("*.example.com", "example.com"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("a*b", "acd"));
    }

    #[test]
    fn negation_wins() {
        assert!(host_matches("db1.corp", "*.corp,!web*.corp"));
        assert!(!host_matches("web1.corp", "*.corp,!web*.corp"));
    }

    #[test]
    fn trusted_unknown_changed() {
        let (_d, kh) = setup(&format!("# comment\nweb.example.com,10.0.0.5 {ED_A} root@web\n"));
        assert_eq!(kh.check("web.example.com", 22, &key(ED_A)), HostKeyStatus::Trusted);
        assert_eq!(kh.check("10.0.0.5", 22, &key(ED_A)), HostKeyStatus::Trusted);
        assert_eq!(kh.check("other", 22, &key(ED_A)), HostKeyStatus::Unknown { other_keys: vec![] });
        // Same host on another port is a different host to ssh.
        assert_eq!(kh.check("web.example.com", 2222, &key(ED_A)), HostKeyStatus::Unknown { other_keys: vec![] });
        assert!(matches!(kh.check("web.example.com", 22, &key(ED_B)), HostKeyStatus::Changed { line: 2, .. }));
    }

    #[test]
    fn bad_lines_do_not_poison_the_file() {
        let (_d, kh) = setup(&format!("garbage\nhost ssh-ed25519 notbase64!!\n@weird host {ED_B}\nweb {ED_A}\n"));
        assert_eq!(kh.check("web", 22, &key(ED_A)), HostKeyStatus::Trusted);
    }

    #[test]
    fn revoked_and_cert_authority() {
        let (_d, kh) = setup(&format!("@revoked * {ED_A}\n@cert-authority *.corp {ED_B}\n"));
        assert!(matches!(kh.check("x", 22, &key(ED_A)), HostKeyStatus::Revoked { .. }));
        // A CA line is not a host key.
        assert_eq!(kh.check("a.corp", 22, &key(ED_B)), HostKeyStatus::Unknown { other_keys: vec![] });
    }

    #[test]
    fn hashed_hosts() {
        // The same construction `ssh-keygen -H` uses: HMAC-SHA1(salt, hostname).
        let salt = B64.encode(b"0123456789abcdefghij");
        let mut mac = Hmac::<Sha1>::new_from_slice(b"0123456789abcdefghij").unwrap();
        mac.update(b"web.example.com");
        let hash = B64.encode(mac.finalize().into_bytes());
        let (_d, kh) = setup(&format!("|1|{salt}|{hash} {ED_A}\n"));
        assert_eq!(kh.check("web.example.com", 22, &key(ED_A)), HostKeyStatus::Trusted);
        assert_eq!(kh.check("web2.example.com", 22, &key(ED_A)), HostKeyStatus::Unknown { other_keys: vec![] });
    }

    #[test]
    fn learn_then_forget() {
        let (_d, kh) = setup("");
        kh.learn("box", 2222, &key(ED_A)).unwrap();
        assert_eq!(kh.check("box", 2222, &key(ED_A)), HostKeyStatus::Trusted);
        let text = std::fs::read_to_string(kh.app_file()).unwrap();
        assert!(text.starts_with("[box]:2222 ssh-ed25519 "), "{text}");
        assert_eq!(kh.known_algorithms("box", 2222), vec![key(ED_A).algorithm()]);
        assert_eq!(kh.forget("box", 2222).unwrap(), 1);
        assert_eq!(kh.check("box", 2222, &key(ED_A)), HostKeyStatus::Unknown { other_keys: vec![] });
    }
}
