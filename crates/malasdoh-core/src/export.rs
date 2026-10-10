//! Export and import of saved connections (and optionally snippets and
//! trusted host keys) as one file, encrypted with a passphrase, for backup
//! or moving to another computer.
//!
//! The key comes from the passphrase with PBKDF2-HMAC-SHA256 and a random
//! salt; the contents are sealed with ChaCha20-Poly1305, so a wrong
//! passphrase or a changed file is detected rather than producing garbage.
//! Saved passwords stay in the OS keychain and are not exported.

use anyhow::{Context, Result, bail};
use base64::Engine;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

const MAGIC: &str = "MalasDoh export v1";
/// OWASP's current advice for PBKDF2-HMAC-SHA256.
pub const ITERATIONS: u32 = 600_000;

/// What is inside, each as the text of its usual file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// `profiles.toml`.
    pub profiles: String,
    #[serde(default)]
    pub snippets: Option<String>,
    /// MalasDoh's own `known_hosts` lines.
    #[serde(default)]
    pub known_hosts: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    kdf: String,
    iterations: u32,
    salt: String,
    nonce: String,
    data: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn derive(passphrase: &str, salt: &[u8], iterations: u32) -> Result<LessSafeKey> {
    let mut key = [0u8; 32];
    let n = std::num::NonZeroU32::new(iterations).context("iterations must not be 0")?;
    ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA256, n, salt, passphrase.as_bytes(), &mut key);
    let k = UnboundKey::new(&aead::CHACHA20_POLY1305, &key).map_err(|_| anyhow::anyhow!("bad key"))?;
    Ok(LessSafeKey::new(k))
}

pub fn seal(bundle: &Bundle, passphrase: &str) -> Result<String> {
    seal_with(bundle, passphrase, ITERATIONS)
}

/// [`seal`] with a chosen work factor (tests use a small one).
pub fn seal_with(bundle: &Bundle, passphrase: &str, iterations: u32) -> Result<String> {
    if passphrase.chars().count() < 8 {
        bail!("use a passphrase of at least 8 characters");
    }
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rng.fill(&mut salt).map_err(|_| anyhow::anyhow!("no random numbers"))?;
    rng.fill(&mut nonce).map_err(|_| anyhow::anyhow!("no random numbers"))?;
    let key = derive(passphrase, &salt, iterations)?;
    let mut data = serde_json::to_vec(bundle)?;
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC.as_bytes()), &mut data)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let sealed = Sealed {
        kdf: "pbkdf2-hmac-sha256".into(),
        iterations,
        salt: b64().encode(salt),
        nonce: b64().encode(nonce),
        data: b64().encode(data),
    };
    Ok(format!("{MAGIC}\n{}\n", serde_json::to_string(&sealed)?))
}

pub fn open(text: &str, passphrase: &str) -> Result<Bundle> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some(MAGIC) {
        bail!("this is not a MalasDoh export");
    }
    let sealed: Sealed = serde_json::from_str(lines.next().unwrap_or("")).context("the export is damaged")?;
    if sealed.kdf != "pbkdf2-hmac-sha256" || sealed.iterations == 0 || sealed.iterations > 50_000_000 {
        bail!("the export uses settings this version does not understand");
    }
    let salt = b64().decode(&sealed.salt).context("the export is damaged")?;
    let nonce: [u8; 12] =
        b64().decode(&sealed.nonce).ok().and_then(|n| n.try_into().ok()).context("the export is damaged")?;
    let mut data = b64().decode(&sealed.data).context("the export is damaged")?;
    let key = derive(passphrase, &salt, sealed.iterations)?;
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC.as_bytes()), &mut data)
        .map_err(|_| anyhow::anyhow!("wrong passphrase, or the file was changed"))?;
    serde_json::from_slice(plain).context("the export is damaged")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> Bundle {
        Bundle {
            profiles: "[[profile]]\nname = \"web\"\n".into(),
            snippets: Some("[[snippet]]\n".into()),
            known_hosts: None,
        }
    }

    #[test]
    fn round_trip_and_wrong_passphrase() {
        let text = seal_with(&bundle(), "correct horse", 1000).unwrap();
        assert!(text.starts_with(MAGIC));
        assert!(!text.contains("web"), "contents are not readable");
        assert_eq!(open(&text, "correct horse").unwrap(), bundle());
        let err = open(&text, "wrong horse!").unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"), "{err:#}");
        // Two exports of the same data differ (fresh salt and nonce).
        assert_ne!(text, seal_with(&bundle(), "correct horse", 1000).unwrap());
    }

    #[test]
    fn tampering_and_junk_are_refused() {
        let text = seal_with(&bundle(), "correct horse", 1000).unwrap();
        let mut sealed: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        let mut data = b64().decode(sealed["data"].as_str().unwrap()).unwrap();
        data[0] ^= 1;
        sealed["data"] = b64().encode(data).into();
        let changed = format!("{MAGIC}\n{sealed}\n");
        assert!(open(&changed, "correct horse").is_err());
        assert!(open("hello", "x").unwrap_err().to_string().contains("not a MalasDoh export"));
        assert!(seal_with(&bundle(), "short", 1000).is_err(), "short passphrases are refused");
    }
}
