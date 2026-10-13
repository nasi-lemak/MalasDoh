//! Teleport: log in with a password and an authenticator code, get an SSH
//! certificate, and reach the cluster's servers through its proxy, without
//! `tsh` or a browser.
//!
//! - Login goes to the proxy's web API (`/webapi/mfa/login/begin` and
//!   `…/finish`), the same calls `tsh login` makes. The key is made here and
//!   never leaves this computer; Teleport signs its public half.
//! - The certificate and key are kept as `key` / `key-cert.pub` in the
//!   cluster's folder, so connections use them like any key and certificate.
//! - Servers are reached with the proxy as a jump host: on its SSH port, or,
//!   when the cluster serves everything on one port ("TLS routing"), through
//!   TLS to the web port with the `teleport-proxy-ssh` protocol.
//! - Their host certificates are checked against the cluster's host CA,
//!   written as `@cert-authority` lines for just its proxy and servers.
//! - Listing servers needs a web session, which needs its own code (Teleport
//!   accepts each code once), so it is a separate step from logging in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::profile::{AuthMethod, Profile, ProfileId};
use crate::proxy::Proxy;

pub type ClusterId = uuid::Uuid;

/// How long to ask the certificate to last; the cluster may give less.
const TTL_NANOS: u64 = 12 * 3600 * 1_000_000_000;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A Teleport cluster this computer logs in to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cluster {
    pub id: ClusterId,
    /// The proxy's web address as typed: `teleport.example.com` or `host:3080`.
    pub address: String,
    /// The Teleport user name.
    pub user: String,
    /// The cluster's name, from the proxy.
    pub name: String,
    /// Everything on the web port (ALPN routing), or a separate SSH port.
    pub tls_routing: bool,
    /// The proxy's SSH address when it has a separate SSH port.
    pub ssh_addr: String,
    /// SHA-256 of the proxy's TLS certificate, when the user chose to trust
    /// one the system does not (a self-signed proxy). Hex.
    pub pinned_cert: Option<String>,
    /// The cluster's host CA keys (OpenSSH public key lines).
    pub host_cas: Vec<String>,
    /// The proxy connection that servers jump through.
    pub proxy_profile: Option<ProfileId>,
    /// `[host]:port` names of the proxy and servers, for the CA lines.
    pub hosts: Vec<String>,
}

impl Default for Cluster {
    fn default() -> Self {
        Self {
            id: ClusterId::new_v4(),
            address: String::new(),
            user: String::new(),
            name: String::new(),
            tls_routing: false,
            ssh_addr: String::new(),
            pinned_cert: None,
            host_cas: Vec::new(),
            proxy_profile: None,
            hosts: Vec::new(),
        }
    }
}

impl Cluster {
    pub fn label(&self) -> String {
        if self.name.is_empty() { self.address.clone() } else { self.name.clone() }
    }

    fn target(&self) -> Result<Target> {
        let (host, port) = parse_address(&self.address)?;
        Ok(Target { host, port, pin: self.pinned_cert.clone() })
    }

    /// The private key connections to this cluster use (its certificate is
    /// `key-cert.pub` next to it).
    pub fn key_path(&self, teleport_dir: &Path) -> PathBuf {
        teleport_dir.join(self.id.to_string()).join("key")
    }
}

/// The proxy's address and how to trust it.
#[derive(Debug, Clone)]
struct Target {
    host: String,
    port: u16,
    pin: Option<String>,
}

/// `https://teleport.example.com/web` → (`teleport.example.com`, 443).
pub fn parse_address(addr: &str) -> Result<(String, u16)> {
    let a = addr.trim();
    let a = a.strip_prefix("https://").unwrap_or(a);
    let a = a.split('/').next().unwrap_or("");
    if a.is_empty() {
        bail!("enter the Teleport address, e.g. teleport.example.com");
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.starts_with('[') => {
            let port = p.parse().with_context(|| format!("bad port in {addr}"))?;
            Ok((h.trim_matches(['[', ']']).to_string(), port))
        }
        _ => Ok((a.to_string(), 443)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TeleportError {
    /// The proxy's TLS certificate is not trusted by this computer.
    #[error("the Teleport proxy's certificate is not trusted ({reason})")]
    UntrustedCert { fingerprint: String, reason: String },
    /// What the cluster said.
    #[error("Teleport: {0}")]
    Api(String),
    #[error("{0}")]
    Unsupported(String),
}

/// A connection needs a (new) Teleport login first.
#[derive(Debug, Clone, thiserror::Error)]
#[error("your Teleport login for {server} has expired or is missing; log in again (Connection menu, Teleport…)")]
pub struct LoginNeeded {
    pub cluster: ClusterId,
    pub server: String,
}

// ---------------------------------------------------------------------------
// TLS

/// Checks certificates the usual way (system and Mozilla roots), or accepts
/// the one pinned certificate; remembers what was presented either way.
#[derive(Debug)]
struct Verifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    pin: Option<String>,
    seen: Arc<Mutex<Option<Vec<u8>>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(end_entity.to_vec());
        if self.pin.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&hex::encode(Sha256::digest(end_entity)))) {
            return Ok(ServerCertVerified::assertion());
        }
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn system_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // The system's store too: company CAs are often only there.
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    roots
}

/// `ab:cd:…` for showing a certificate's SHA-256.
pub fn format_fingerprint(hex_digest: &str) -> String {
    hex_digest.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect::<Vec<_>>().join(":")
}

pub type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

/// TLS to the proxy, offering `alpn` protocols; its certificate checked
/// against `roots` (the system's, unless given).
async fn tls_connect(t: &Target, alpn: &[&[u8]], roots: Option<RootCertStore>) -> Result<TlsStream> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = roots.unwrap_or_else(system_roots);
    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .context("setting up certificate checks")?;
    let seen = Arc::new(Mutex::new(None));
    let verifier = Arc::new(Verifier { inner, pin: t.pin.clone(), seen: seen.clone(), provider: provider.clone() });
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let name = ServerName::try_from(t.host.clone()).with_context(|| format!("bad host name {}", t.host))?;
    let tcp = tokio::time::timeout(TIMEOUT, TcpStream::connect((t.host.as_str(), t.port)))
        .await
        .map_err(|_| anyhow::anyhow!("{}:{} did not answer", t.host, t.port))?
        .with_context(|| format!("could not reach {}:{}", t.host, t.port))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    match tokio::time::timeout(TIMEOUT, connector.connect(name, tcp)).await {
        Err(_) => bail!("the TLS handshake with {} timed out", t.host),
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => {
            let bad_cert = e
                .get_ref()
                .and_then(|i| i.downcast_ref::<rustls::Error>())
                .is_some_and(|r| matches!(r, rustls::Error::InvalidCertificate(_)));
            match seen.lock().unwrap().take() {
                Some(der) if bad_cert => Err(TeleportError::UntrustedCert {
                    fingerprint: hex::encode(Sha256::digest(&der)),
                    reason: e.to_string(),
                }
                .into()),
                _ => Err(anyhow::Error::from(e).context(format!("TLS with {} failed", t.host))),
            }
        }
    }
}

/// The SSH stream to a proxy that routes by TLS protocol (one port for all).
/// For SSH the proxy shows a certificate from the cluster's own CA, not
/// its web certificate, so that CA (`ca_file`, written at login) is what
/// it is checked against.
pub async fn ssh_over_tls(host: &str, port: u16, ca_file: &Path) -> Result<TlsStream> {
    let pem = std::fs::read(ca_file).with_context(|| format!("reading {}", ca_file.display()))?;
    let mut roots = RootCertStore::empty();
    use rustls::pki_types::pem::PemObject as _;
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
        roots.add(cert.context("bad certificate in the cluster CA file")?).context("bad cluster CA")?;
    }
    if roots.is_empty() {
        bail!("no cluster CA in {}; log in to Teleport again", ca_file.display());
    }
    let t = Target { host: host.to_string(), port, pin: None };
    tls_connect(&t, &[b"teleport-proxy-ssh"], Some(roots)).await
}

// ---------------------------------------------------------------------------
// HTTP, just enough for the web API.

struct Response {
    status: u16,
    body: Vec<u8>,
    cookies: Vec<String>,
}

async fn request(
    t: &Target,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    headers: &[(&str, String)],
) -> Result<Response> {
    let mut s = tls_connect(t, &[b"http/1.1"], None).await?;
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let host = if t.port == 443 { t.host.clone() } else { format!("{}:{}", t.host, t.port) };
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: MalasDoh/{}\r\nAccept: application/json\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if method != "GET" {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    let work = async {
        s.write_all(req.as_bytes()).await?;
        let mut raw = Vec::new();
        // Servers often skip TLS close_notify; what arrived is still the answer.
        match s.read_to_end(&mut raw).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
            Err(e) => return Err(e),
        }
        Ok(raw)
    };
    let raw = tokio::time::timeout(TIMEOUT, work).await.map_err(|_| anyhow::anyhow!("Teleport did not answer"))??;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").context("bad HTTP answer from Teleport")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .context("bad HTTP status from Teleport")?;
    let mut chunked = false;
    let mut cookies = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        } else if k == "set-cookie"
            && let Some(c) = v.split(';').next()
        {
            cookies.push(c.trim().to_string());
        }
    }
    let rest = &raw[split + 4..];
    let body = if chunked { dechunk(rest)? } else { rest.to_vec() };
    Ok(Response { status, body, cookies })
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").context("bad chunked answer")?;
        let size_text = String::from_utf8_lossy(&b[..eol]);
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16).context("bad chunk size")?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            bail!("the answer from Teleport was cut short");
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or_default();
    }
}

/// The JSON answer, or the cluster's error message.
fn json(r: Response) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null);
    if r.status >= 400 {
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {}", r.status));
        return Err(TeleportError::Api(friendly(&msg)).into());
    }
    Ok(v)
}

fn friendly(msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("invalid credentials")
        || m.contains("invalid username, password")
        || m == "access denied"
        || m.contains("invalid totp")
    {
        return "the user name, password or code was not accepted (each code works only once; wait for the next \
                one). Repeated failures lock the account for a while."
            .into();
    }
    msg.to_string()
}

// ---------------------------------------------------------------------------
// The API

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInfo {
    pub name: String,
    pub version: String,
    /// `local`, or an SSO type (`github`, `oidc`, `saml`).
    pub auth_type: String,
    /// `otp`, `on`, `webauthn`, `optional`, `off`.
    pub second_factor: String,
    pub tls_routing: bool,
    /// `host:port` of the proxy's own SSH port, when it has one.
    pub ssh_addr: String,
    pub session_ttl: String,
}

impl ClusterInfo {
    /// Why this way of logging in cannot work here, if it cannot.
    pub fn unsupported(&self) -> Option<String> {
        if self.auth_type != "local" {
            return Some(format!(
                "this cluster logs in through {} (single sign-on), which needs a browser; use tsh or the web page",
                self.auth_type
            ));
        }
        if matches!(self.second_factor.as_str(), "webauthn") {
            return Some("this cluster needs a security key (WebAuthn), which needs a browser or tsh".into());
        }
        None
    }

    pub fn needs_code(&self) -> bool {
        matches!(self.second_factor.as_str(), "otp" | "on" | "optional")
    }
}

/// What the proxy says about the cluster (no login needed).
pub async fn ping(address: &str, pinned_cert: Option<&str>) -> Result<ClusterInfo> {
    let (host, port) = parse_address(address)?;
    let t = Target { host: host.clone(), port, pin: pinned_cert.map(str::to_string) };
    let v = json(request(&t, "GET", "/webapi/ping", None, &[]).await?)?;
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let tls_routing = v.pointer("/proxy/tls_routing_enabled").and_then(|x| x.as_bool()).unwrap_or(false);
    let ssh_addr = if tls_routing {
        String::new()
    } else {
        let public = s("/proxy/ssh/ssh_public_addr");
        if public.is_empty() {
            // Teleport's default proxy SSH port, on the proxy's own name.
            format!("{host}:3023")
        } else if public.starts_with(':') {
            format!("{host}{public}")
        } else {
            public
        }
    };
    Ok(ClusterInfo {
        name: s("/cluster_name"),
        version: s("/server_version"),
        auth_type: s("/auth/type"),
        second_factor: s("/auth/second_factor"),
        tls_routing,
        ssh_addr,
        session_ttl: s("/auth/default_session_ttl"),
    })
}

/// A signed login.
#[derive(Debug, Clone)]
pub struct Login {
    /// The OpenSSH certificate line.
    pub cert: String,
    /// Host CA keys, as OpenSSH public key lines.
    pub host_cas: Vec<String>,
    /// The cluster's TLS host CA certificates, PEM.
    pub tls_cas: String,
    /// The user's TLS certificate, PEM, for Teleport's own API (listing
    /// servers). Older clusters may not send one.
    pub tls_cert: Option<String>,
}

fn b64_text(s: &str) -> Result<String> {
    let bytes = B64.decode(s.trim()).context("bad base64 from Teleport")?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

async fn sign(cluster: &Cluster, password: &str, code: &str, public_key: &str, tls_public: &str) -> Result<Login> {
    let t = cluster.target()?;
    let user = cluster.user.trim();
    // Starting the login tells Teleport which second factor to expect.
    let begin = serde_json::json!({ "user": user, "pass": password });
    let challenge = json(request(&t, "POST", "/webapi/mfa/login/begin", Some(&begin), &[]).await?)?;
    let wants_code = challenge.get("totp_challenge").and_then(|x| x.as_bool()).unwrap_or(false);
    let webauthn_only = !wants_code && !challenge.get("webauthn_challenge").is_none_or(|x| x.is_null());
    if webauthn_only {
        bail!(TeleportError::Unsupported(
            "this account signs in with a security key, which needs a browser or tsh".into()
        ));
    }
    let finish = serde_json::json!({
        "user": user,
        "password": password,
        "totp_code": code.trim(),
        "ssh_pub_key": B64.encode(public_key.trim().as_bytes()),
        "tls_pub_key": B64.encode(tls_public.as_bytes()),
        "ttl": TTL_NANOS,
    });
    let v = json(request(&t, "POST", "/webapi/mfa/login/finish", Some(&finish), &[]).await?)?;
    let cert = b64_text(v.get("cert").and_then(|c| c.as_str()).context("Teleport sent no certificate")?)?;
    let (mut host_cas, mut tls_cas) = (Vec::new(), String::new());
    for signer in v.get("host_signers").and_then(|h| h.as_array()).into_iter().flatten() {
        for k in signer.get("checking_keys").and_then(|k| k.as_array()).into_iter().flatten() {
            if let Some(k) = k.as_str() {
                host_cas.push(b64_text(k)?);
            }
        }
        for c in signer.get("tls_certs").and_then(|k| k.as_array()).into_iter().flatten() {
            if let Some(c) = c.as_str() {
                tls_cas.push_str(&b64_text(c)?);
                tls_cas.push('\n');
            }
        }
    }
    let tls_cert = v.get("tls_cert").and_then(|c| c.as_str()).filter(|c| !c.is_empty()).map(b64_text).transpose()?;
    Ok(Login { cert, host_cas, tls_cas, tls_cert })
}

/// Log in: make a new key, have Teleport sign it, and store both where
/// this cluster's connections look for them. Returns the certificate's
/// state.
pub async fn log_in(cluster: &mut Cluster, teleport_dir: &Path, password: &str, code: &str) -> Result<CertState> {
    use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("making a key")?;
    let public = key.public_key().to_openssh().context("encoding the key")?;
    let (tls_key, tls_public) = crate::teleport_api::new_tls_key()?;
    let login = sign(cluster, password, code, &public, &tls_public).await?;
    let path = cluster.key_path(teleport_dir);
    if let Some(dir) = path.parent() {
        crate::paths::create_private_dir(dir)?;
    }
    let pem = key.to_openssh(LineEnding::LF).context("encoding the key")?;
    // Replacing the previous login's files (owner-only, never half-written).
    crate::paths::write_private_atomic(&path, pem.as_bytes())?;
    crate::paths::write_private_atomic(&cert_path(&path), format!("{}\n", login.cert).as_bytes())?;
    crate::paths::write_private_atomic(&tls_ca_path(&path), login.tls_cas.as_bytes())?;
    let (tls_key_file, tls_cert_file) =
        (crate::teleport_api::tls_key_path(&path), crate::teleport_api::tls_cert_path(&path));
    match &login.tls_cert {
        Some(c) => {
            crate::paths::write_private_atomic(&tls_key_file, tls_key.as_bytes())?;
            crate::paths::write_private_atomic(&tls_cert_file, format!("{}\n", c.trim()).as_bytes())?;
        }
        None => {
            let _ = std::fs::remove_file(&tls_key_file);
            let _ = std::fs::remove_file(&tls_cert_file);
        }
    }
    cluster.host_cas = login.host_cas;
    cert_state(&path).context("Teleport's certificate could not be read")
}

/// Forget the login: the key and certificate go.
pub fn log_out(cluster: &Cluster, teleport_dir: &Path) {
    let path = cluster.key_path(teleport_dir);
    let _ = std::fs::remove_file(cert_path(&path));
    let _ = std::fs::remove_file(tls_ca_path(&path));
    let _ = std::fs::remove_file(crate::teleport_api::tls_key_path(&path));
    let _ = std::fs::remove_file(crate::teleport_api::tls_cert_path(&path));
    let _ = std::fs::remove_file(&path);
}

/// The cluster's TLS CA, next to the key.
pub fn tls_ca_path(key: &Path) -> PathBuf {
    key.with_file_name("tls-ca.pem")
}

pub fn cert_path(key: &Path) -> PathBuf {
    PathBuf::from(format!("{}-cert.pub", key.display()))
}

/// A login's certificate: until when, and as which server logins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertState {
    /// Unix seconds.
    pub valid_before: i64,
    pub logins: Vec<String>,
}

impl CertState {
    pub fn is_valid(&self) -> bool {
        // A minute's margin, so a connection does not start just as it ends.
        self.valid_before > crate::journal::now() + 60
    }
}

pub fn cert_state(key: &Path) -> Option<CertState> {
    let text = std::fs::read_to_string(cert_path(key)).ok()?;
    let cert = russh::keys::ssh_key::Certificate::from_openssh(text.trim()).ok()?;
    let logins = cert.valid_principals().iter().filter(|p| !p.starts_with('-')).cloned().collect();
    Some(CertState { valid_before: cert.valid_before() as i64, logins })
}

/// A server in the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub hostname: String,
    /// `host:port` the node listens on; empty for nodes behind a tunnel.
    pub addr: String,
    pub labels: Vec<(String, String)>,
    pub logins: Vec<String>,
}

impl Node {
    /// The SSH port to ask the proxy for.
    pub fn port(&self) -> u16 {
        self.addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).filter(|p| *p != 0).unwrap_or(3022)
    }
}

/// The servers this user can reach. Needs a web session, so a password and
/// a fresh code; the session is ended again straight after.
pub async fn list_nodes(cluster: &Cluster, password: &str, code: &str) -> Result<Vec<Node>> {
    let t = cluster.target()?;
    let login =
        serde_json::json!({ "user": cluster.user.trim(), "pass": password, "second_factor_token": code.trim() });
    let r = request(&t, "POST", "/v1/webapi/sessions/web", Some(&login), &[]).await?;
    let cookies = r.cookies.join("; ");
    let session = json(r)?;
    let token = session.get("token").and_then(|t| t.as_str()).context("Teleport sent no session")?.to_string();
    let auth = vec![("Authorization", format!("Bearer {token}")), ("Cookie", cookies)];
    let site = if cluster.name.is_empty() { "-current-".to_string() } else { cluster.name.clone() };
    let mut nodes = Vec::new();
    let mut start = String::new();
    let listed = async {
        for _ in 0..50 {
            let mut path = format!("/v1/webapi/sites/{site}/resources?kinds=node&limit=500");
            if !start.is_empty() {
                path.push_str(&format!("&startKey={}", url_escape(&start)));
            }
            let v = json(request(&t, "GET", &path, None, &auth).await?)?;
            for item in v.get("items").and_then(|i| i.as_array()).into_iter().flatten() {
                let s = |k: &str| item.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let labels = item
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|t| Some((t.get("name")?.as_str()?.to_string(), t.get("value")?.as_str()?.to_string())))
                    .collect();
                let logins = item
                    .get("sshLogins")
                    .and_then(|l| l.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect();
                nodes.push(Node { id: s("id"), hostname: s("hostname"), addr: s("addr"), labels, logins });
            }
            start = v.get("startKey").and_then(|k| k.as_str()).unwrap_or("").to_string();
            if start.is_empty() {
                break;
            }
        }
        anyhow::Ok(())
    }
    .await;
    // End the web session whatever happened.
    let _ = request(&t, "DELETE", "/v1/webapi/sessions/web", None, &auth).await;
    listed?;
    nodes.sort_by_key(|n| n.hostname.to_lowercase());
    Ok(nodes)
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connections

/// The connection servers jump through: the proxy's SSH port, or its web
/// port through TLS when the cluster routes everything over one port.
/// `existing` keeps its id and anything the user changed but these fields.
pub fn proxy_profile(
    cluster: &Cluster,
    teleport_dir: &Path,
    login: &str,
    existing: Option<Profile>,
) -> Result<Profile> {
    let (web_host, web_port) = parse_address(&cluster.address)?;
    let (host, port, proxy) = if cluster.tls_routing {
        let ca_file = tls_ca_path(&cluster.key_path(teleport_dir)).display().to_string();
        let tls = Proxy::TeleportTls { host: web_host.clone(), port: web_port, ca_file };
        (web_host, web_port, tls)
    } else {
        let (h, p) = parse_address(&cluster.ssh_addr).unwrap_or((web_host, 3023));
        (h, p, Proxy::None)
    };
    let mut p = existing.unwrap_or_else(|| Profile {
        name: format!("Teleport proxy ({})", cluster.label()),
        group: group_name(cluster),
        ..Default::default()
    });
    p.host = host;
    p.port = port;
    p.proxy = proxy;
    p.user = login.to_string();
    p.auth = AuthMethod::Key { path: cluster.key_path(teleport_dir).display().to_string() };
    p.teleport = Some(cluster.id);
    p.jump = None;
    Ok(p)
}

/// A server's connection: by its id (host names need not be unique), as
/// `login`, through the proxy connection `via`.
pub fn node_profile(
    cluster: &Cluster,
    teleport_dir: &Path,
    node: &Node,
    login: &str,
    via: ProfileId,
    existing: Option<Profile>,
) -> Profile {
    let mut p = existing.unwrap_or_else(|| Profile { group: group_name(cluster), ..Default::default() });
    p.name = if node.hostname.is_empty() { node.id.clone() } else { node.hostname.clone() };
    p.host = node.id.clone();
    p.port = node.port();
    p.user = login.to_string();
    p.auth = AuthMethod::Key { path: cluster.key_path(teleport_dir).display().to_string() };
    p.jump = Some(via);
    p.teleport = Some(cluster.id);
    p.proxy = Proxy::None;
    let labels: Vec<String> = node.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if p.notes.is_empty() || p.notes.starts_with("Teleport:") {
        p.notes = format!(
            "Teleport: {}{}",
            cluster.label(),
            if labels.is_empty() { String::new() } else { format!(" · {}", labels.join(", ")) }
        );
    }
    p
}

fn group_name(cluster: &Cluster) -> String {
    format!("Teleport · {}", cluster.label())
}

// ---------------------------------------------------------------------------
// Storage

/// The clusters, in `teleport.toml`.
#[derive(Debug, Clone, Default)]
pub struct TeleportStore {
    path: PathBuf,
    pub clusters: Vec<Cluster>,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default, rename = "cluster")]
    clusters: Vec<Cluster>,
}

impl TeleportStore {
    pub fn load(path: PathBuf) -> Self {
        let clusters = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<File>(&t).ok())
            .map(|f| f.clusters)
            .unwrap_or_default();
        Self { path, clusters }
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&File { clusters: self.clusters.clone() })?;
        crate::paths::write_atomic(&self.path, text.as_bytes())
    }

    pub fn get(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.iter().find(|c| c.id == id)
    }

    /// The `@cert-authority` lines: each cluster's host CA, for just its
    /// proxy and servers.
    pub fn write_known_hosts(&self, path: &Path) -> Result<()> {
        let mut out = String::from("# Written by MalasDoh for its Teleport clusters; rewritten on each login.\n");
        for c in &self.clusters {
            if c.hosts.is_empty() {
                continue;
            }
            let patterns = c.hosts.join(",");
            for ca in &c.host_cas {
                out.push_str(&format!("@cert-authority {patterns} {ca}\n"));
            }
        }
        if let Some(dir) = path.parent() {
            crate::paths::create_private_dir(dir)?;
        }
        crate::paths::write_atomic(path, out.as_bytes())
    }
}

/// How `host`/`port` is written in known_hosts.
pub fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(parse_address("teleport.example.com").unwrap(), ("teleport.example.com".into(), 443));
        assert_eq!(parse_address("https://tp.example.com:3080/web/login").unwrap(), ("tp.example.com".into(), 3080));
        assert!(parse_address("  ").is_err());
    }

    #[test]
    fn http_answers() {
        let raw = b"HTTP/1.1 200 OK\r\nSet-Cookie: __Host-session=abc; Path=/; Secure\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(
            (r.status, r.body.as_slice(), r.cookies),
            (200, &b"{\"a\":1}"[..], vec!["__Host-session=abc".into()])
        );
        let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 40\r\n\r\n{\"error\":{\"message\":\"access denied\"}}";
        let e = json(parse_response(raw).unwrap()).unwrap_err().to_string();
        assert!(e.contains("each code works only once"), "{e}");
    }

    #[test]
    fn node_ports() {
        let n = |addr: &str| Node { addr: addr.into(), ..Default::default() }.port();
        assert_eq!((n("10.0.0.5:3022"), n(""), n("127.0.0.1:2222")), (3022, 3022, 2222));
    }

    #[test]
    fn fingerprints_read_like_browsers_show_them() {
        assert_eq!(format_fingerprint("ab01ff"), "AB:01:FF");
    }
}
