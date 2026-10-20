//! Listing a Teleport cluster's servers with the login's own certificate,
//! the way `tsh ls` does, so no second authenticator code is needed.
//!
//! The proxy speaks gRPC to holders of a cluster certificate, next to SSH:
//! on its SSH port (TLS with `h2`), or on the web port with TLS protocol
//! `teleport-proxy-ssh-grpc` when everything shares one port. Its transport
//! service opens a byte stream to the cluster's auth service
//! (`ProxyCluster`); inside that stream runs a second TLS session, to the
//! auth service itself, and over it one call: `AuthService/ListResources`.
//! The few protobuf messages involved are encoded by hand.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bytes::{Buf, Bytes, BytesMut};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls;

use crate::teleport::{Cluster, Node, cert_state, parse_address};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// What the auth service's certificate is issued for, whatever its address.
const AUTH_NAME: &str = "teleport.cluster.local";
const PAGE: i32 = 500;

// ---------------------------------------------------------------------------
// The login's TLS key

/// The TLS key and certificate, next to the SSH key.
pub fn tls_key_path(key: &Path) -> PathBuf {
    key.with_file_name("tls-key.pem")
}

pub fn tls_cert_path(key: &Path) -> PathBuf {
    key.with_file_name("tls-cert.pem")
}

/// A new ECDSA P-256 key (what Teleport uses for TLS): PKCS#8 PEM, and the
/// public key as PEM for the login request.
pub(crate) fn new_tls_key() -> Result<(String, String)> {
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|_| anyhow::anyhow!("could not make a TLS key"))?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
        .map_err(|_| anyhow::anyhow!("could not read the new TLS key"))?;
    // SubjectPublicKeyInfo: id-ecPublicKey, prime256v1, then the point.
    let mut spki = hex::decode("3059301306072a8648ce3d020106082a8648ce3d030107034200").unwrap();
    spki.extend_from_slice(pair.public_key().as_ref());
    Ok((pem("PRIVATE KEY", pkcs8.as_ref()), pem("PUBLIC KEY", &spki)))
}

fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

// ---------------------------------------------------------------------------
// Protobuf, just these messages

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn field_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    varint(out, (u64::from(field) << 3) | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn field_varint(out: &mut Vec<u8>, field: u32, v: u64) {
    varint(out, u64::from(field) << 3);
    varint(out, v);
}

enum Value<'a> {
    Varint(#[cfg_attr(not(test), allow(dead_code))] u64),
    Bytes(&'a [u8]),
}

/// The fields of one message, in order. Fixed-width fields are skipped.
fn fields(mut b: &[u8]) -> Result<Vec<(u32, Value<'_>)>> {
    fn read_varint(b: &mut &[u8]) -> Result<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let Some((&c, rest)) = b.split_first() else { bail!("truncated message from Teleport") };
            *b = rest;
            v |= u64::from(c & 0x7f) << shift;
            if c < 0x80 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                bail!("bad number in a message from Teleport");
            }
        }
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let tag = read_varint(&mut b)?;
        let (field, wire) = ((tag >> 3) as u32, tag & 7);
        match wire {
            0 => out.push((field, Value::Varint(read_varint(&mut b)?))),
            1 | 5 => {
                let n = if wire == 1 { 8 } else { 4 };
                if b.len() < n {
                    bail!("truncated message from Teleport");
                }
                b = &b[n..];
            }
            2 => {
                let n = read_varint(&mut b)? as usize;
                if b.len() < n {
                    bail!("truncated message from Teleport");
                }
                out.push((field, Value::Bytes(&b[..n])));
                b = &b[n..];
            }
            _ => bail!("unexpected data in a message from Teleport"),
        }
    }
    Ok(out)
}

fn text(v: &Value<'_>) -> String {
    match v {
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Varint(_) => String::new(),
    }
}

/// `proto.ListResourcesRequest` for `kind` ("node", "windows_desktop"),
/// with each one's allowed logins.
fn list_request(kind: &str, start_key: &str) -> Vec<u8> {
    let mut m = Vec::new();
    field_bytes(&mut m, 1, kind.as_bytes()); // ResourceType
    field_bytes(&mut m, 2, b"default"); // Namespace
    field_varint(&mut m, 3, PAGE as u64); // Limit
    if !start_key.is_empty() {
        field_bytes(&mut m, 4, start_key.as_bytes()); // StartKey
    }
    field_varint(&mut m, 13, 1); // IncludeLogins
    m
}

/// The next page's key of a `proto.ListResourcesResponse`.
fn next_key(b: &[u8]) -> Result<String> {
    Ok(fields(b)?.into_iter().find(|(f, _)| *f == 2).map(|(_, v)| text(&v)).unwrap_or_default())
}

/// `proto.ListResourcesResponse`: the nodes and the next page's key.
fn parse_list(b: &[u8]) -> Result<(Vec<Node>, String)> {
    let (mut nodes, mut next) = (Vec::new(), String::new());
    for (f, v) in fields(b)? {
        match (f, v) {
            (1, Value::Bytes(res)) => {
                // PaginatedResource: Node = 3, Logins = 13.
                let mut node = None;
                let mut logins = Vec::new();
                for (f, v) in fields(res)? {
                    match (f, v) {
                        (3, Value::Bytes(server)) => node = Some(parse_server(server)?),
                        (13, v) => logins.push(text(&v)),
                        _ => {}
                    }
                }
                if let Some(mut n) = node {
                    logins.sort();
                    n.logins = logins;
                    nodes.push(n);
                }
            }
            (2, v) => next = text(&v),
            _ => {}
        }
    }
    Ok((nodes, next))
}

/// `types.ServerV2`: Metadata = 4 (Name = 1, Labels = 5), Spec = 5 (Addr = 1,
/// Hostname = 3).
fn parse_server(b: &[u8]) -> Result<Node> {
    let mut n = Node::default();
    for (f, v) in fields(b)? {
        match (f, v) {
            (4, Value::Bytes(meta)) => (n.id, n.labels) = metadata(meta)?,
            (5, Value::Bytes(spec)) => {
                for (f, v) in fields(spec)? {
                    match f {
                        1 => n.addr = text(&v),
                        3 => n.hostname = text(&v),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    n.labels.sort();
    Ok(n)
}

// ---------------------------------------------------------------------------
// gRPC over HTTP/2

/// A gRPC message on the wire: uncompressed flag, length, bytes.
fn grpc_frame(msg: &[u8]) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + msg.len());
    b.extend_from_slice(&[0]);
    b.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    b.extend_from_slice(msg);
    b.freeze()
}

/// Pulls whole gRPC messages out of the bytes of a response body.
#[derive(Default)]
struct Deframer(BytesMut);

impl Deframer {
    fn push(&mut self, data: &[u8]) {
        self.0.extend_from_slice(data);
    }

    fn next(&mut self) -> Result<Option<Bytes>> {
        if self.0.len() < 5 {
            return Ok(None);
        }
        if self.0[0] != 0 {
            bail!("Teleport sent a compressed message");
        }
        let n = u32::from_be_bytes([self.0[1], self.0[2], self.0[3], self.0[4]]) as usize;
        if self.0.len() < 5 + n {
            return Ok(None);
        }
        self.0.advance(5);
        Ok(Some(self.0.split_to(n).freeze()))
    }
}

fn grpc_request(authority: &str, path: &str) -> Result<http::Request<()>> {
    http::Request::builder()
        .method("POST")
        .uri(format!("https://{authority}{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .context("building a request")
}

/// The call's outcome from its headers or trailers: `grpc-status` 0 is fine.
fn grpc_status(h: &http::HeaderMap) -> Result<()> {
    let Some(code) = h.get("grpc-status").and_then(|v| v.to_str().ok()) else { return Ok(()) };
    if code == "0" {
        return Ok(());
    }
    let msg = h.get("grpc-message").and_then(|v| v.to_str().ok()).map(percent_decode).unwrap_or_default();
    match code {
        "7" => bail!("Teleport refused: {msg}"),
        "16" => bail!(crate::teleport::TeleportError::Unsupported(format!("the login was not accepted: {msg}"))),
        "12" => bail!("this Teleport version cannot list servers this way ({msg})"),
        _ => bail!("Teleport answered with error {code}: {msg}"),
    }
}

fn percent_decode(s: &str) -> String {
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

/// One unary call over an HTTP/2 connection.
async fn unary(send: &mut h2::client::SendRequest<Bytes>, authority: &str, path: &str, msg: &[u8]) -> Result<Bytes> {
    let req = grpc_request(authority, path)?;
    let (resp, mut body) = send.clone().ready().await?.send_request(req, false)?;
    body.send_data(grpc_frame(msg), true)?;
    let resp = resp.await.context("no answer from Teleport's auth service")?;
    if resp.status() != 200 {
        bail!("Teleport's auth service answered HTTP {}", resp.status());
    }
    grpc_status(resp.headers())?;
    let mut recv = resp.into_body();
    let mut de = Deframer::default();
    let mut message = None;
    while let Some(chunk) = recv.data().await {
        let chunk = chunk?;
        let _ = recv.flow_control().release_capacity(chunk.len());
        de.push(&chunk);
        if let Some(m) = de.next()? {
            message = Some(m);
        }
    }
    if let Some(t) = recv.trailers().await? {
        grpc_status(&t)?;
    }
    message.context("Teleport's auth service sent no answer")
}

// ---------------------------------------------------------------------------
// The connection

fn tls_config(roots: RootCertStore, cert: &Path, key: &Path, alpn: &[u8]) -> Result<Arc<ClientConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .context("bad TLS certificate")?;
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("using the login's TLS certificate")?;
    config.alpn_protocols = vec![alpn.to_vec()];
    Ok(Arc::new(config))
}

fn cluster_roots(ca_file: &Path) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(ca_file).with_context(|| format!("reading {}", ca_file.display()))? {
        roots.add(c.context("bad cluster CA")?).context("bad cluster CA")?;
    }
    if roots.is_empty() {
        bail!("no cluster CA; log in to Teleport again");
    }
    Ok(roots)
}

async fn h2_connect<S>(io: S) -> Result<h2::client::SendRequest<Bytes>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (send, conn) = h2::client::handshake(io).await.context("starting HTTP/2")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(send)
}

/// A byte stream to the cluster's auth service, through the proxy's
/// `ProxyCluster` call: what is written goes out as request frames, what
/// arrives comes back as response frames. The proxy answers only once
/// bytes flow, so its reply is handled in the background; an error it
/// gives ends the stream and is left in the returned slot.
async fn auth_stream(
    proxy: &mut h2::client::SendRequest<Bytes>,
    authority: &str,
    cluster: &str,
) -> Result<(tokio::io::DuplexStream, Arc<std::sync::Mutex<Option<String>>>)> {
    let req = grpc_request(authority, "/teleport.transport.v1.TransportService/ProxyCluster")?;
    let (resp, mut up) = proxy.clone().ready().await?.send_request(req, false)?;
    let mut first = Vec::new();
    field_bytes(&mut first, 1, cluster.as_bytes()); // ProxyClusterRequest.cluster
    up.send_data(grpc_frame(&first), false)?;
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    let (mut rd, mut wr) = tokio::io::split(theirs);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut frame = Vec::with_capacity(n + 8);
            field_bytes(&mut frame, 1, &buf[..n]); // Frame.payload
            let mut msg = Vec::with_capacity(frame.len() + 8);
            field_bytes(&mut msg, 2, &frame); // ProxyClusterRequest.frame
            if up.send_data(grpc_frame(&msg), false).is_err() {
                break;
            }
        }
        let _ = up.send_data(Bytes::new(), true);
    });
    let error = Arc::new(std::sync::Mutex::new(None));
    let slot = error.clone();
    tokio::spawn(async move {
        let fail = |e: String| *slot.lock().unwrap() = Some(e);
        let resp = match resp.await {
            Ok(r) => r,
            Err(e) => return fail(format!("the proxy ended the stream: {e}")),
        };
        if resp.status() != 200 {
            return fail(format!("the proxy answered HTTP {}", resp.status()));
        }
        if let Err(e) = grpc_status(resp.headers()) {
            return fail(format!("{e:#}"));
        }
        let mut recv = resp.into_body();
        let mut de = Deframer::default();
        while let Some(Ok(chunk)) = recv.data().await {
            let _ = recv.flow_control().release_capacity(chunk.len());
            de.push(&chunk);
            while let Ok(Some(m)) = de.next() {
                // ProxyClusterResponse.frame (1) → Frame.payload (1).
                let Ok(outer) = fields(&m) else { return };
                for (f, v) in outer {
                    if let (1, Value::Bytes(frame)) = (f, v) {
                        for (f, v) in fields(frame).unwrap_or_default() {
                            if let (1, Value::Bytes(p)) = (f, v)
                                && wr.write_all(p).await.is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        }
        if let Ok(Some(t)) = recv.trailers().await
            && let Err(e) = grpc_status(&t)
        {
            fail(format!("{e:#}"));
        }
        let _ = wr.shutdown().await;
    });
    Ok((ours, error))
}

/// The servers this login can reach, with the logins allowed on each.
/// Needs a login made by this version (which also gets a TLS certificate).
pub async fn list_nodes_with_login(cluster: &Cluster, teleport_dir: &Path) -> Result<Vec<Node>> {
    let pages = list_with_login(cluster, teleport_dir, "node").await?;
    let mut nodes = Vec::new();
    for p in pages {
        nodes.extend(parse_list(&p)?.0);
    }
    nodes.sort_by_key(|n| n.hostname.to_lowercase());
    Ok(nodes)
}

/// A Windows desktop registered in the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desktop {
    pub name: String,
    /// Its RDP address as the cluster knows it (`10.0.0.20:3389`).
    pub addr: String,
    /// The Active Directory domain; empty for a desktop outside one.
    pub domain: String,
    pub labels: Vec<(String, String)>,
    /// The Windows users this login may use there.
    pub logins: Vec<String>,
}

impl Desktop {
    /// Host and port of its address (3389 when it has none).
    pub fn host_port(&self) -> (String, u16) {
        match self.addr.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() && !h.ends_with(':') => {
                (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap())
            }
            _ => (self.addr.trim_matches(['[', ']']).to_string(), 3389),
        }
    }
}

/// The Windows desktops this login can see, with the Windows users allowed
/// on each.
pub async fn list_desktops_with_login(cluster: &Cluster, teleport_dir: &Path) -> Result<Vec<Desktop>> {
    let mut out = Vec::new();
    for p in list_with_login(cluster, teleport_dir, "windows_desktop").await? {
        out.extend(parse_desktops(&p)?);
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    Ok(out)
}

/// The desktops of a ListResources reply: PaginatedResource WindowsDesktop
/// = 5 (header = 1 with Metadata = 4; Spec = 2: Addr = 1, Domain = 2),
/// Logins = 13.
fn parse_desktops(b: &[u8]) -> Result<Vec<Desktop>> {
    let mut out = Vec::new();
    for (f, v) in fields(b)? {
        let (1, Value::Bytes(res)) = (f, v) else { continue };
        let mut d = None::<Desktop>;
        let mut logins = Vec::new();
        for (f, v) in fields(res)? {
            match (f, v) {
                (5, Value::Bytes(desk)) => {
                    let mut x = Desktop::default();
                    for (f, v) in fields(desk)? {
                        match (f, v) {
                            (1, Value::Bytes(header)) => {
                                for (f, v) in fields(header)? {
                                    if let (4, Value::Bytes(meta)) = (f, v) {
                                        let (name, labels) = metadata(meta)?;
                                        x.name = name;
                                        x.labels = labels;
                                    }
                                }
                            }
                            (2, Value::Bytes(spec)) => {
                                for (f, v) in fields(spec)? {
                                    match f {
                                        1 => x.addr = text(&v),
                                        2 => x.domain = text(&v),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    d = Some(x);
                }
                (13, v) => logins.push(text(&v)),
                _ => {}
            }
        }
        if let Some(mut d) = d {
            logins.sort();
            d.logins = logins;
            // Teleport's own labels say nothing a person needs here.
            d.labels.retain(|(k, _)| !k.starts_with("teleport.dev/"));
            d.labels.sort();
            out.push(d);
        }
    }
    Ok(out)
}

/// `types.Metadata`: Name = 1, Labels = 5 (map entries: key 1, value 2).
fn metadata(b: &[u8]) -> Result<(String, Vec<(String, String)>)> {
    let (mut name, mut labels) = (String::new(), Vec::new());
    for (f, v) in fields(b)? {
        match (f, v) {
            (1, v) => name = text(&v),
            (5, Value::Bytes(entry)) => {
                let (mut k, mut val) = (String::new(), String::new());
                for (f, v) in fields(entry)? {
                    match f {
                        1 => k = text(&v),
                        2 => val = text(&v),
                        _ => {}
                    }
                }
                labels.push((k, val));
            }
            _ => {}
        }
    }
    Ok((name, labels))
}

/// Every page of a ListResources reply for `kind`, as received.
pub async fn list_with_login(cluster: &Cluster, teleport_dir: &Path, kind: &str) -> Result<Vec<Bytes>> {
    let key = cluster.key_path(teleport_dir);
    if !cert_state(&key).is_some_and(|c| c.is_valid()) {
        bail!(crate::teleport::LoginNeeded { cluster: cluster.id, server: cluster.label() });
    }
    let (cert, tkey) = (tls_cert_path(&key), tls_key_path(&key));
    if !cert.exists() || !tkey.exists() {
        bail!("this login has no TLS certificate (it was made by an older MalasDoh); log in again");
    }
    let roots = cluster_roots(&crate::teleport::tls_ca_path(&key))?;
    // The proxy's gRPC service: on the one port when everything is routed
    // by TLS protocol, otherwise next to SSH on the SSH port.
    let (web_host, web_port) = parse_address(&cluster.address)?;
    let (host, port, alpn): (String, u16, &[u8]) = if cluster.tls_routing {
        (web_host, web_port, b"teleport-proxy-ssh-grpc")
    } else {
        let (h, p) = parse_address(&cluster.ssh_addr).unwrap_or((web_host, 3023));
        (h, p, b"h2")
    };
    tokio::time::timeout(TIMEOUT * 2, async {
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("could not reach {host}:{port}"))?;
        let outer = tls_config(roots.clone(), &cert, &tkey, alpn)?;
        let name = ServerName::try_from(host.clone()).with_context(|| format!("bad host name {host}"))?;
        let tls = tokio_rustls::TlsConnector::from(outer)
            .connect(name, tcp)
            .await
            .context("TLS with the proxy's gRPC service failed")?;
        let authority = format!("{host}:{port}");
        let mut proxy = h2_connect(tls).await?;
        let site = if cluster.name.is_empty() { cluster.label() } else { cluster.name.clone() };
        let (stream, proxy_error) = auth_stream(&mut proxy, &authority, &site).await?;
        let inner = tls_config(roots, &cert, &tkey, b"h2")?;
        let tls = match tokio_rustls::TlsConnector::from(inner)
            .connect(ServerName::try_from(AUTH_NAME).unwrap(), stream)
            .await
        {
            Ok(t) => t,
            Err(e) => match proxy_error.lock().unwrap().take() {
                Some(why) => bail!("{why}"),
                None => return Err(anyhow::Error::from(e).context("TLS with the auth service failed")),
            },
        };
        let mut auth = h2_connect(tls).await?;
        let (mut pages, mut start) = (Vec::new(), String::new());
        for _ in 0..200 {
            let resp =
                unary(&mut auth, AUTH_NAME, "/proto.AuthService/ListResources", &list_request(kind, &start)).await?;
            let next = next_key(&resp)?;
            pages.push(resp);
            if next.is_empty() {
                break;
            }
            start = next;
        }
        anyhow::Ok(pages)
    })
    .await
    .map_err(|_| anyhow::anyhow!("listing the servers timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_request_and_response_round_trip() {
        let req = list_request("node", "abc");
        let got: Vec<(u32, String)> = fields(&req)
            .unwrap()
            .into_iter()
            .map(|(f, v)| {
                (
                    f,
                    match v {
                        Value::Varint(n) => n.to_string(),
                        b => text(&b),
                    },
                )
            })
            .collect();
        assert_eq!(
            got,
            [(1, "node".into()), (2, "default".into()), (3, "500".into()), (4, "abc".into()), (13, "1".into())]
        );

        // A response with one node, as Teleport would encode it.
        let mut label = Vec::new();
        field_bytes(&mut label, 1, b"env");
        field_bytes(&mut label, 2, b"prod");
        let mut meta = Vec::new();
        field_bytes(&mut meta, 1, b"6f1c-uuid");
        field_bytes(&mut meta, 5, &label);
        let mut spec = Vec::new();
        field_bytes(&mut spec, 1, b"10.0.0.5:3022");
        field_bytes(&mut spec, 3, b"web-1");
        let mut server = Vec::new();
        field_bytes(&mut server, 1, b"node");
        field_bytes(&mut server, 4, &meta);
        field_bytes(&mut server, 5, &spec);
        let mut res = Vec::new();
        field_bytes(&mut res, 3, &server);
        field_bytes(&mut res, 13, b"root");
        field_bytes(&mut res, 13, b"deploy");
        let mut resp = Vec::new();
        field_bytes(&mut resp, 1, &res);
        field_bytes(&mut resp, 2, b"next-page");
        let (nodes, next) = parse_list(&resp).unwrap();
        assert_eq!(next, "next-page");
        assert_eq!(
            nodes,
            [Node {
                id: "6f1c-uuid".into(),
                hostname: "web-1".into(),
                addr: "10.0.0.5:3022".into(),
                labels: vec![("env".into(), "prod".into())],
                logins: vec!["deploy".into(), "root".into()],
            }]
        );
    }

    #[test]
    fn desktops_from_a_list_reply() {
        let entry = |k: &str, v: &str| {
            let mut e = Vec::new();
            field_bytes(&mut e, 1, k.as_bytes());
            field_bytes(&mut e, 2, v.as_bytes());
            e
        };
        let mut meta = Vec::new();
        field_bytes(&mut meta, 1, b"win-app-1");
        field_bytes(&mut meta, 5, &entry("teleport.dev/origin", "config-file"));
        field_bytes(&mut meta, 5, &entry("os", "windows"));
        let mut header = Vec::new();
        field_bytes(&mut header, 1, b"windows_desktop");
        field_bytes(&mut header, 4, &meta);
        let mut spec = Vec::new();
        field_bytes(&mut spec, 1, b"10.0.0.20:3389");
        field_bytes(&mut spec, 2, b"corp.example");
        let mut desk = Vec::new();
        field_bytes(&mut desk, 1, &header);
        field_bytes(&mut desk, 2, &spec);
        let mut res = Vec::new();
        field_bytes(&mut res, 13, b"rdpuser");
        field_bytes(&mut res, 13, b"Administrator");
        field_bytes(&mut res, 5, &desk);
        let mut reply = Vec::new();
        field_bytes(&mut reply, 1, &res);
        let got = parse_desktops(&reply).unwrap();
        assert_eq!(
            got,
            [Desktop {
                name: "win-app-1".into(),
                addr: "10.0.0.20:3389".into(),
                domain: "corp.example".into(),
                labels: vec![("os".into(), "windows".into())],
                logins: vec!["Administrator".into(), "rdpuser".into()],
            }]
        );
        assert_eq!(got[0].host_port(), ("10.0.0.20".to_string(), 3389));
        let bare = Desktop { addr: "winbox".into(), ..Default::default() };
        assert_eq!(bare.host_port(), ("winbox".to_string(), 3389));
        let v6 = Desktop { addr: "[fd00::5]:3390".into(), ..Default::default() };
        assert_eq!(v6.host_port(), ("fd00::5".to_string(), 3390));
    }

    #[test]
    fn grpc_framing() {
        let mut de = Deframer::default();
        let f = grpc_frame(b"hello");
        de.push(&f[..3]);
        assert!(de.next().unwrap().is_none());
        de.push(&f[3..]);
        de.push(&grpc_frame(b"!"));
        assert_eq!(&de.next().unwrap().unwrap()[..], b"hello");
        assert_eq!(&de.next().unwrap().unwrap()[..], b"!");
        assert!(de.next().unwrap().is_none());
        assert_eq!(percent_decode("access%20denied"), "access denied");
    }

    #[test]
    fn tls_key_pem() {
        let (key, public) = new_tls_key().unwrap();
        assert!(key.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(public.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        assert!(PrivateKeyDer::from_pem_slice(key.as_bytes()).is_ok());
    }
}
