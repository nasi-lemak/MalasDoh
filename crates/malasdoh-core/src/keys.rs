//! SSH keys: make a new one, and put a public key on a server (what
//! `ssh-copy-id` does).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::ssh::{Connection, shell_quote};

/// Where a new key goes by default: `~/.ssh/id_ed25519`, or a numbered name
/// if that is taken.
pub fn default_path() -> PathBuf {
    let dir = crate::paths::user_ssh_dir().unwrap_or_else(|| PathBuf::from("."));
    let first = dir.join("id_ed25519");
    if !first.exists() && !pub_path(&first).exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("id_ed25519_{n}")))
        .find(|p| !p.exists() && !pub_path(p).exists())
        .expect("a free name")
}

pub fn pub_path(private: &Path) -> PathBuf {
    let mut s = private.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

/// Make an Ed25519 key pair at `path` and `path.pub`, never replacing an
/// existing file. An empty passphrase leaves the private key unencrypted.
/// Returns the public key line.
pub fn generate(path: &Path, comment: &str, passphrase: &str) -> Result<String> {
    let public = pub_path(path);
    for p in [path, public.as_path()] {
        if p.exists() {
            bail!("{} already exists", p.display());
        }
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        crate::paths::create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut rng = rand::rng();
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519).context("generating the key")?;
    key.set_comment(comment);
    let line = key.public_key().to_openssh().context("encoding the public key")?;
    let stored =
        if passphrase.is_empty() { key } else { key.encrypt(&mut rng, passphrase).context("encrypting the key")? };
    let pem = stored.to_openssh(LineEnding::LF).context("encoding the private key")?;
    crate::paths::write_private(path, pem.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    std::fs::write(&public, format!("{line}\n")).with_context(|| format!("writing {}", public.display()))?;
    Ok(line)
}

/// The public key line for a key file: from `<key>.pub`, or worked out from
/// the private key when that is not encrypted. A `.pub` path is read as is.
pub fn public_line(path: &Path) -> Result<String> {
    let candidates =
        if path.extension().is_some_and(|e| e == "pub") { vec![path.to_path_buf()] } else { vec![pub_path(path)] };
    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'));
            if let Some(l) = line {
                check_line(l)?;
                return Ok(l.to_string());
            }
        }
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match russh::keys::decode_secret_key(&text, None) {
        Ok(k) => Ok(k.public_key().to_openssh()?),
        Err(russh::keys::Error::KeyIsEncrypted) => {
            bail!("{} is encrypted and has no .pub file next to it", path.display())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A public key line must be one line that looks like `type base64 [comment]`.
fn check_line(line: &str) -> Result<()> {
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(data)) = (parts.next(), parts.next()) else { bail!("not a public key: {line}") };
    let known = kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-");
    if !known || line.contains(['\n', '\r']) || data.len() < 16 {
        bail!("not a public key: {line}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    Added,
    AlreadyThere,
}

/// The shell script that adds `line` to `file` (relative to the home
/// directory unless absolute). Like `ssh-copy-id`: the folder and file are
/// created owner-only, a missing final newline is fixed first, and SELinux
/// labels are restored where that tool exists. A key that is already there
/// (same type and data, any comment or options) is not added again.
fn install_script(line: &str, file: &str) -> Result<String> {
    check_line(line)?;
    let mut parts = line.split_whitespace();
    let id = format!("{} {}", parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    Ok(format!(
        "cd || exit 1; umask 077; F={f}; K={k}; ID={id}; \
         mkdir -p \"$(dirname \"$F\")\" || exit 1; \
         if [ -f \"$F\" ] && grep -qF -- \"$ID\" \"$F\"; then echo MALASDOH-ALREADY; exit 0; fi; \
         {{ [ -z \"$(tail -c1 \"$F\" 2>/dev/null)\" ] || echo >> \"$F\"; }} && printf '%s\\n' \"$K\" >> \"$F\" || exit 1; \
         if type restorecon >/dev/null 2>&1; then restorecon -F \"$(dirname \"$F\")\" \"$F\" 2>/dev/null; fi; \
         echo MALASDOH-ADDED",
        f = shell_quote(file),
        k = shell_quote(line),
        id = shell_quote(&id),
    ))
}

/// Add a public key to the server's `authorized_keys` (`file`, by default
/// `.ssh/authorized_keys` in the home directory).
pub async fn install(conn: &Connection, line: &str, file: Option<&str>) -> Result<Installed> {
    let script = install_script(line.trim(), file.unwrap_or(".ssh/authorized_keys"))?;
    let out = conn.exec(&script, None).await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.success() {
        bail!("adding the key failed: {}", out.stderr_text().trim());
    }
    if stdout.contains("MALASDOH-ALREADY") {
        Ok(Installed::AlreadyThere)
    } else if stdout.contains("MALASDOH-ADDED") {
        Ok(Installed::Added)
    } else {
        bail!("unexpected reply from the server: {}", stdout.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_usable_key_pair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("id_test");
        let line = generate(&path, "me@laptop", "").unwrap();
        assert!(line.starts_with("ssh-ed25519 ") && line.ends_with(" me@laptop"), "{line}");
        assert_eq!(public_line(&path).unwrap(), line);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(russh::keys::decode_secret_key(&text, None).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(generate(&path, "again", "").is_err(), "never replaces a key");

        // With a passphrase the private key is encrypted.
        let enc = dir.path().join("id_enc");
        generate(&enc, "x", "secret").unwrap();
        let text = std::fs::read_to_string(&enc).unwrap();
        assert!(matches!(russh::keys::decode_secret_key(&text, None), Err(russh::keys::Error::KeyIsEncrypted)));
        assert!(russh::keys::decode_secret_key(&text, Some("secret")).is_ok());
        // Without the .pub, an encrypted key cannot give its public half.
        std::fs::remove_file(pub_path(&enc)).unwrap();
        assert!(public_line(&enc).is_err());
    }

    #[test]
    fn install_script_quotes_and_rejects_junk() {
        let s =
            install_script("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample me's key", ".ssh/authorized_keys").unwrap();
        assert!(s.contains(r#"K='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample me'\''s key'"#), "{s}");
        assert!(s.contains("ID='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample'"));
        assert!(install_script("rm -rf /", "f").is_err());
        assert!(install_script("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5\nevil", "f").is_err());
    }
}
