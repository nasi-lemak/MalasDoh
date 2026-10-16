//! A WireGuard tunnel inside MalasDoh, like wireproxy: no network adapter,
//! no admin rights. Connections that use it reach their server through the
//! tunnel; nothing else on the computer does.
//!
//! WireGuard itself is `boringtun`. Inside the tunnel travel IP packets, so
//! a small TCP/IP stack (`smoltcp`) makes the TCP connections SSH needs,
//! and asks the tunnel's DNS server for names. One task runs it all: UDP
//! datagrams to and from each peer, the WireGuard timers, and the TCP
//! sockets, each joined to the connection using it by a pipe.
//!
//! Connections using the same configuration share one tunnel, which ends
//! when the last of them closes.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use bytes::Bytes;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dns, tcp, udp};
use smoltcp::wire::{DnsQueryType, HardwareAddress, IpAddress, IpCidr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc, oneshot};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// How long a connection may take to open, handshake included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Room in each direction of each TCP connection; enough for a fast link
/// with some latency.
const TCP_BUFFER: usize = 512 * 1024;
/// WireGuard adds 80 bytes at most (60 of headers when the outer packet is
/// IPv6, plus 32 of its own, rounded): the default of wg-quick.
const DEFAULT_MTU: usize = 1420;

// ---------------------------------------------------------------------------
// Configuration: the usual wg-quick `.conf`

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgConfig {
    pub private_key: [u8; 32],
    /// This side's addresses in the tunnel.
    pub addresses: Vec<(IpAddr, u8)>,
    /// Name servers inside the tunnel.
    pub dns: Vec<IpAddr>,
    pub mtu: usize,
    pub peers: Vec<WgPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgPeer {
    pub public_key: [u8; 32],
    pub preshared_key: Option<[u8; 32]>,
    /// `host:port`, resolved when the tunnel starts.
    pub endpoint: String,
    /// What is reached through this peer.
    pub allowed_ips: Vec<(IpAddr, u8)>,
    pub keepalive: Option<u16>,
}

fn key(s: &str, what: &str) -> Result<[u8; 32]> {
    let b = B64.decode(s.trim()).with_context(|| format!("{what} is not valid base64"))?;
    b.try_into().map_err(|_| anyhow::anyhow!("{what} is not a WireGuard key (32 bytes)"))
}

fn cidr(s: &str) -> Result<(IpAddr, u8)> {
    let s = s.trim();
    let (ip, len) = match s.split_once('/') {
        Some((ip, len)) => (ip, Some(len)),
        None => (s, None),
    };
    let ip: IpAddr = ip.trim().parse().with_context(|| format!("{s} is not an IP address"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let len = match len {
        Some(l) => l.trim().parse::<u8>().ok().filter(|l| *l <= max).with_context(|| format!("bad prefix in {s}"))?,
        None => max,
    };
    Ok((ip, len))
}

fn list(v: &str) -> impl Iterator<Item = &str> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty())
}

impl WgConfig {
    /// Read a wg-quick configuration. Lines this does not use (PostUp,
    /// Table, ListenPort...) are ignored; what it cannot do is refused.
    pub fn parse(text: &str) -> Result<Self> {
        let (mut private_key, mut addresses, mut dns, mut mtu, mut peers) =
            (None, Vec::new(), Vec::new(), None, Vec::new());
        let mut section = "";
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                section = match line.to_ascii_lowercase().as_str() {
                    "[interface]" => "interface",
                    "[peer]" => {
                        peers.push((None, None, None, Vec::new(), None));
                        "peer"
                    }
                    other => bail!("line {}: unknown section {other}", n + 1),
                };
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { bail!("line {}: expected `Name = value`", n + 1) };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            let at = |e: anyhow::Error| e.context(format!("line {}", n + 1));
            match (section, k.as_str()) {
                ("interface", "privatekey") => private_key = Some(key(v, "PrivateKey").map_err(at)?),
                ("interface", "address") => {
                    for a in list(v) {
                        addresses.push(cidr(a).map_err(at)?);
                    }
                }
                ("interface", "dns") => {
                    // wg-quick allows search domains here too; only servers matter.
                    dns.extend(list(v).filter_map(|d| d.parse::<IpAddr>().ok()));
                }
                ("interface", "mtu") => mtu = Some(v.parse::<usize>().map_err(|e| at(e.into()))?),
                ("peer", k) => {
                    let p = peers.last_mut().unwrap();
                    match k {
                        "publickey" => p.0 = Some(key(v, "PublicKey").map_err(at)?),
                        "presharedkey" => p.1 = Some(key(v, "PresharedKey").map_err(at)?),
                        "endpoint" => p.2 = Some(v.to_string()),
                        "allowedips" => {
                            for a in list(v) {
                                p.3.push(cidr(a).map_err(at)?);
                            }
                        }
                        "persistentkeepalive" => {
                            p.4 = v.parse::<u16>().ok().filter(|k| *k > 0);
                        }
                        _ => {}
                    }
                }
                ("", _) => bail!("line {}: `{k}` is outside [Interface] and [Peer]", n + 1),
                _ => {}
            }
        }
        let private_key = private_key.context("the configuration has no PrivateKey")?;
        if addresses.is_empty() {
            bail!("the configuration has no Address (this side's address in the tunnel)");
        }
        if peers.is_empty() {
            bail!("the configuration has no [Peer]");
        }
        let peers = peers
            .into_iter()
            .enumerate()
            .map(|(i, (pk, psk, ep, allowed, keepalive))| {
                let n = i + 1;
                Ok(WgPeer {
                    public_key: pk.with_context(|| format!("peer {n} has no PublicKey"))?,
                    preshared_key: psk,
                    endpoint: ep.with_context(|| {
                        format!("peer {n} has no Endpoint: MalasDoh connects out, so it needs the server's address")
                    })?,
                    allowed_ips: if allowed.is_empty() {
                        bail!("peer {n} has no AllowedIPs");
                    } else {
                        allowed
                    },
                    keepalive,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { private_key, addresses, dns, mtu: mtu.unwrap_or(DEFAULT_MTU).clamp(576, 9000), peers })
    }

    /// The configuration without its private key, for storing next to the
    /// key kept in the keychain; [`WgConfig::parse_with_key`] reads it back.
    pub fn strip_private_key(text: &str) -> String {
        text.lines()
            .filter(|l| !l.split('=').next().unwrap_or("").trim().eq_ignore_ascii_case("privatekey"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A stored configuration as it is shown for editing: its PrivateKey
    /// line (if any) replaced by one saying `placeholder`, so the key is
    /// never on screen.
    pub fn for_editing(text: &str, placeholder: &str) -> String {
        let stripped = Self::strip_private_key(text);
        match Self::with_private_key(&stripped, placeholder) {
            Ok(t) => t,
            Err(_) => stripped,
        }
    }

    /// The private key's text as written in `text`, if it has one.
    pub fn private_key_text(text: &str) -> Option<String> {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            k.trim().eq_ignore_ascii_case("privatekey").then(|| v.split('#').next().unwrap_or("").trim().to_string())
        })
    }

    /// A stored configuration with its key: in the text itself (computers
    /// without a keychain), or `key` from the keychain.
    pub fn from_stored(text: &str, key: Option<&str>) -> Result<Self> {
        if Self::private_key_text(text).is_some() {
            return Self::parse(text);
        }
        Self::parse_with_key(text, key.context("its private key is missing; import the configuration again")?)
    }

    pub fn parse_with_key(text: &str, private_key: &str) -> Result<Self> {
        Self::parse(&Self::with_private_key(text, private_key)?)
    }

    /// `text` (without a PrivateKey line) with `private_key` put back in
    /// its [Interface] section.
    pub fn with_private_key(text: &str, private_key: &str) -> Result<String> {
        let mut full = String::new();
        let mut put = false;
        for l in text.lines() {
            full.push_str(l);
            full.push('\n');
            if !put && l.trim().eq_ignore_ascii_case("[interface]") {
                full.push_str(&format!("PrivateKey = {}\n", private_key.trim()));
                put = true;
            }
        }
        if !put {
            bail!("the configuration has no [Interface] section");
        }
        Ok(full)
    }

    /// The peer whose AllowedIPs best match `ip` (longest prefix).
    fn peer_for(&self, ip: IpAddr) -> Option<usize> {
        self.peers
            .iter()
            .enumerate()
            .flat_map(|(i, p)| p.allowed_ips.iter().map(move |c| (i, *c)))
            .filter(|(_, c)| contains(*c, ip))
            .max_by_key(|(_, (_, len))| *len)
            .map(|(i, _)| i)
    }

    /// This side's address of the same family as `ip`.
    fn source_for(&self, ip: IpAddr) -> Option<IpAddr> {
        self.addresses.iter().map(|(a, _)| *a).find(|a| a.is_ipv4() == ip.is_ipv4())
    }
}

/// For a server that does not answer at the WireGuard server's own public
/// address: through the tunnel it is usually reached at another one.
fn own_endpoint_hint(c: &WgConfig) -> String {
    format!(
        "It is the WireGuard server's own public address (its Endpoint); inside the tunnel the server usually has \
         another one{}. Connect to that instead.",
        tunnel_side_guess(c).map(|g| format!(", often {g}")).unwrap_or_default()
    )
}

/// Where the WireGuard server usually is inside the tunnel: the first
/// address of this side's network (x.y.z.1 when this side has a /32).
fn tunnel_side_guess(c: &WgConfig) -> Option<IpAddr> {
    c.addresses.iter().find_map(|(a, len)| match a {
        IpAddr::V4(v4) => {
            let len = if *len > 30 { 24 } else { u32::from(*len) };
            let net = u32::from(*v4) & (u32::MAX << (32 - len));
            let guess = std::net::Ipv4Addr::from(net + 1);
            (guess != *v4).then_some(IpAddr::V4(guess))
        }
        IpAddr::V6(_) => None,
    })
}

fn contains((net, len): (IpAddr, u8), ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) => {
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len)) };
            u32::from(n) & mask == u32::from(i) & mask
        }
        (IpAddr::V6(n), IpAddr::V6(i)) => {
            let mask = if len == 0 { 0 } else { u128::MAX << (128 - u32::from(len)) };
            u128::from(n) & mask == u128::from(i) & mask
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Stored configurations

/// The PrivateKey line's value while a stored configuration is edited.
pub const KEY_NOT_SHOWN: &str = "(saved, not shown)";
/// The same, for a .conf file loaded into the editor: its key, not shown.
pub const KEY_FROM_FILE: &str = "(from the file, not shown)";

/// An edited configuration, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    /// The text to store, without a PrivateKey line.
    pub config: String,
    /// A new private key; `None` keeps the saved one.
    pub new_key: Option<String>,
}

/// Checks an edited configuration (see [`WgConfig::for_editing`]).
/// A PrivateKey line left as [`KEY_NOT_SHOWN`] or [`KEY_FROM_FILE`], or
/// taken out, means `hidden` (the key of a file loaded into the editor)
/// or, without one, the saved key; any other value is a new key.
pub fn check_edit(text: &str, hidden: Option<&str>) -> Result<Edited> {
    let new_key = match WgConfig::private_key_text(text) {
        Some(v) if v != KEY_NOT_SHOWN && v != KEY_FROM_FILE => Some(v),
        _ => hidden.map(str::to_string),
    };
    let config = WgConfig::strip_private_key(text);
    // A stand-in key when the saved one is kept: the rest is what is checked.
    let check_key = new_key.clone().unwrap_or_else(|| "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string());
    WgConfig::parse_with_key(&config, &check_key).map_err(|e| {
        if new_key.is_some() && format!("{e:#}").to_lowercase().contains("privatekey") {
            e.context("the new PrivateKey is not a WireGuard key (leave the line as it was to keep the saved one)")
        } else {
            e
        }
    })?;
    Ok(Edited { config, new_key })
}

/// An imported configuration. `config` normally has no PrivateKey line:
/// the key is in the keychain as [`crate::secrets::SecretKey::WireGuardKey`].
/// On a computer without a keychain it stays in `config` (the file is
/// owner-only), and `key_in_file` says so.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WgEntry {
    pub id: uuid::Uuid,
    pub name: String,
    pub config: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key_in_file: bool,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default, rename = "tunnel")]
    tunnels: Vec<WgEntry>,
}

/// The configurations in `wireguard.toml`.
#[derive(Debug, Default)]
pub struct WgStore {
    path: std::path::PathBuf,
    pub tunnels: Vec<WgEntry>,
}

impl WgStore {
    pub fn load(path: std::path::PathBuf) -> Self {
        let tunnels = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<StoreFile>(&t).ok())
            .map(|f| f.tunnels)
            .unwrap_or_default();
        Self { path, tunnels }
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string(&StoreFile { tunnels: self.tunnels.clone() })?;
        crate::paths::write_private_atomic(&self.path, text.as_bytes())
    }

    /// For [`crate::ssh::ConnectContext::wireguard`].
    pub fn configs(&self) -> HashMap<uuid::Uuid, String> {
        self.tunnels.iter().map(|t| (t.id, t.config.clone())).collect()
    }

    /// Check a full configuration (with its key) and add it. Returns the new
    /// entry and the private key, for the caller to put in the keychain.
    pub fn import(&mut self, name: &str, text: &str) -> Result<(WgEntry, String)> {
        WgConfig::parse(text)?;
        let key = WgConfig::private_key_text(text).context("the configuration has no PrivateKey")?;
        let entry = WgEntry {
            id: uuid::Uuid::new_v4(),
            name: name.trim().to_string(),
            config: WgConfig::strip_private_key(text),
            key_in_file: false,
        };
        self.tunnels.push(entry.clone());
        Ok((entry, key))
    }
}

// ---------------------------------------------------------------------------
// Shared tunnels

/// The app's WireGuard configurations, without private keys, by tunnel id.
pub type Configs = Arc<std::sync::RwLock<HashMap<uuid::Uuid, String>>>;

/// A running tunnel. Kept alive by the connections using it.
pub struct Tunnel {
    cmd: mpsc::Sender<Cmd>,
    notify: Arc<Notify>,
    /// Once the first handshake has happened.
    pub status: Arc<Mutex<TunnelStatus>>,
}

#[derive(Debug, Clone, Default)]
pub struct TunnelStatus {
    pub handshake: bool,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    /// Each peer, in configuration order.
    pub peers: Vec<PeerStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct PeerStatus {
    pub endpoint: String,
    pub handshake: bool,
    /// The system reported the endpoint refused a datagram (ICMP "port
    /// unreachable"): nothing listens there.
    pub refused: bool,
}

enum Cmd {
    Connect {
        ip: IpAddr,
        port: u16,
        reply: oneshot::Sender<Result<tokio::io::DuplexStream>>,
    },
    Resolve {
        name: String,
        reply: oneshot::Sender<Result<Vec<IpAddr>>>,
    },
    /// Start a handshake with every peer now (one otherwise waits for traffic).
    Handshake,
    /// Send one UDP datagram through the tunnel and wait for any answer.
    Probe {
        to: (IpAddr, u16),
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
}

type Registry = Mutex<HashMap<(uuid::Uuid, u64), Weak<Tunnel>>>;
static TUNNELS: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();

fn fingerprint(c: &WgConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{c:?}").hash(&mut h);
    h.finish()
}

/// The tunnel for this configuration: the running one, or a new one.
pub async fn tunnel(id: uuid::Uuid, config: &WgConfig) -> Result<Arc<Tunnel>> {
    let key = (id, fingerprint(config));
    let reg = TUNNELS.get_or_init(Default::default);
    if let Some(t) = reg.lock().unwrap().get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let t = Arc::new(start(config.clone()).await?);
    let mut map = reg.lock().unwrap();
    map.retain(|_, w| w.strong_count() > 0);
    // Two connections may have started one each; keep the first.
    if let Some(existing) = map.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    map.insert(key, Arc::downgrade(&t));
    Ok(t)
}

/// The tunnels running now, by configuration id.
pub fn running() -> Vec<(uuid::Uuid, TunnelStatus)> {
    let reg = TUNNELS.get_or_init(Default::default);
    let map = reg.lock().unwrap();
    map.iter().filter_map(|((id, _), w)| w.upgrade().map(|t| (*id, t.status.lock().unwrap().clone()))).collect()
}

impl Tunnel {
    /// A TCP connection to `host:port` through the tunnel. Names are looked
    /// up with the tunnel's DNS servers when it has some, otherwise on this
    /// computer.
    pub async fn connect(self: &Arc<Self>, host: &str, port: u16) -> Result<WgStream> {
        let ip = match host.trim_matches(['[', ']']).parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => self.resolve(host).await?,
        };
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::Connect { ip, port, reply }).await.map_err(|_| anyhow::anyhow!("the tunnel stopped"))?;
        self.notify.notify_one();
        let pipe = rx.await.map_err(|_| anyhow::anyhow!("the tunnel stopped"))??;
        Ok(WgStream { pipe, _tunnel: self.clone() })
    }

    async fn resolve(&self, name: &str) -> Result<IpAddr> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Resolve { name: name.to_string(), reply })
            .await
            .map_err(|_| anyhow::anyhow!("the tunnel stopped"))?;
        self.notify.notify_one();
        let ips = rx.await.map_err(|_| anyhow::anyhow!("the tunnel stopped"))??;
        ips.into_iter().next().with_context(|| format!("{name} has no address"))
    }
}

impl Tunnel {
    fn send(&self, c: Cmd) -> Result<()> {
        self.cmd.try_send(c).map_err(|_| anyhow::anyhow!("the tunnel stopped"))?;
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Testing a configuration

/// One step of [`test`]: `ok` is `None` when it was not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestStep {
    pub what: String,
    pub ok: Option<bool>,
    pub detail: String,
}

fn step(what: &str, ok: Option<bool>, detail: impl Into<String>) -> TestStep {
    TestStep { what: what.into(), ok, detail: detail.into() }
}

/// A DNS question for the root's name servers: any server answers it,
/// even one that refuses to recurse.
fn dns_probe() -> Vec<u8> {
    vec![0x4d, 0x44, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 1]
}

/// Check a configuration step by step, with a tunnel of its own (running
/// connections are not touched): the server's address, a handshake with
/// each peer, traffic both ways inside the tunnel (the tunnel's DNS
/// server), and optionally a TCP connection to `target`. Stops at the
/// first step that fails.
pub async fn test(config: &WgConfig, target: Option<(&str, u16)>) -> Vec<TestStep> {
    let mut out = Vec::new();
    let mut endpoints = Vec::new();
    // 1. The servers' addresses.
    for p in &config.peers {
        let found = tokio::net::lookup_host(p.endpoint.as_str()).await.map(|mut a| a.next());
        if let Ok(Some(a)) = &found {
            endpoints.push(a.ip());
        }
        match found {
            Ok(Some(a)) if a.to_string() == p.endpoint => {
                out.push(step("Server address", Some(true), p.endpoint.clone()))
            }
            Ok(Some(a)) => out.push(step("Server address", Some(true), format!("{} is {a}", p.endpoint))),
            _ => {
                out.push(step(
                    "Server address",
                    Some(false),
                    format!(
                        "{} could not be looked up: check the Endpoint line, and this computer's network",
                        p.endpoint
                    ),
                ));
                return out;
            }
        }
    }
    let t = match start(config.clone()).await {
        Ok(t) => t,
        Err(e) => {
            out.push(step("Handshake", Some(false), format!("{e:#}")));
            return out;
        }
    };
    // 2. A handshake with every peer.
    let started = std::time::Instant::now();
    let _ = t.send(Cmd::Handshake);
    let deadline = Duration::from_secs(6);
    let status = loop {
        let st = t.status.lock().unwrap().clone();
        let done = !st.peers.is_empty() && st.peers.iter().all(|p| p.handshake || p.refused);
        if done || started.elapsed() > deadline {
            break st;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut all = true;
    for (p, st) in config.peers.iter().zip(&status.peers) {
        if st.handshake {
            out.push(step(
                "Handshake",
                Some(true),
                format!("{} answered in {} ms", p.endpoint, started.elapsed().as_millis()),
            ));
        } else if st.refused {
            all = false;
            out.push(step(
                "Handshake",
                Some(false),
                format!(
                    "{}: its computer says nothing listens on that UDP port. The port in Endpoint is wrong, or \
                     WireGuard is not running there.",
                    p.endpoint
                ),
            ));
        } else {
            all = false;
            out.push(step(
                "Handshake",
                Some(false),
                format!(
                    "{}: no answer in {} s. Most likely UDP to it is blocked (some networks let only web traffic \
                     out), the server is down, or a key does not match (this side's key is not one the server \
                     knows, or PublicKey is not the server's). WireGuard stays silent to packets it does not \
                     accept, so these look the same from here.",
                    p.endpoint,
                    deadline.as_secs()
                ),
            ));
        }
    }
    if !all {
        return out;
    }
    // 3. Traffic both ways inside the tunnel: the tunnel's DNS server.
    match config.dns.first() {
        Some(dns) => {
            let (reply, rx) = oneshot::channel();
            let sent = t.send(Cmd::Probe { to: (*dns, 53), payload: dns_probe(), reply });
            let t0 = std::time::Instant::now();
            match (sent, rx.await) {
                (Ok(()), Ok(Ok(()))) => out.push(step(
                    "Traffic through the tunnel",
                    Some(true),
                    format!("the DNS server {dns} answered through the tunnel in {} ms", t0.elapsed().as_millis()),
                )),
                _ => {
                    out.push(step(
                        "Traffic through the tunnel",
                        Some(false),
                        format!(
                            "the handshake works, but the DNS server {dns} did not answer through the tunnel. The \
                             server may not pass traffic on (its AllowedIPs for this side, or IP forwarding), or \
                             the DNS address is wrong."
                        ),
                    ));
                    return out;
                }
            }
        }
        None if target.is_none() => out.push(step(
            "Traffic through the tunnel",
            None,
            "not checked: the configuration has no DNS server; give a server and port to try",
        )),
        None => {}
    }
    // 4. A server through it.
    if let Some((host, port)) = target {
        let t = Arc::new(t);
        let r = tokio::time::timeout(Duration::from_secs(10), t.connect(host, port)).await;
        out.push(match r {
            Ok(Ok(_)) => step(&format!("{host}:{port}"), Some(true), "reachable through the tunnel"),
            Ok(Err(e)) if format!("{e:#}").contains("refused") => step(
                &format!("{host}:{port}"),
                Some(true),
                "reachable through the tunnel, but nothing accepts connections on that port",
            ),
            Ok(Err(e)) => step(&format!("{host}:{port}"), Some(false), format!("{e:#}")),
            Err(_) if host.parse::<IpAddr>().is_ok_and(|ip| endpoints.contains(&ip)) => step(
                &format!("{host}:{port}"),
                Some(false),
                format!("no answer through the tunnel in 10 s. {}", own_endpoint_hint(config)),
            ),
            Err(_) => step(
                &format!("{host}:{port}"),
                Some(false),
                "no answer through the tunnel in 10 s: the address may be wrong, or the WireGuard server does not \
                 pass traffic on to it",
            ),
        });
    }
    out
}

/// A connection through the tunnel; it keeps the tunnel running.
pub struct WgStream {
    pipe: tokio::io::DuplexStream,
    _tunnel: Arc<Tunnel>,
}

impl AsyncRead for WgStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_read(cx, buf)
    }
}

impl AsyncWrite for WgStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// The packet device between smoltcp and WireGuard

#[derive(Default)]
struct Queues {
    /// Decrypted packets from the tunnel, for smoltcp.
    rx: VecDeque<Vec<u8>>,
    /// Packets from smoltcp, to encrypt and send.
    tx: VecDeque<Vec<u8>>,
    mtu: usize,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for Queues {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(Rx, Tx<'_>)> {
        let p = self.rx.pop_front()?;
        Some((Rx(p), Tx(&mut self.tx)))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = self.mtu;
        c
    }
}

// ---------------------------------------------------------------------------
// The tunnel task

struct Peer {
    tunn: Tunn,
    udp: Arc<UdpSocket>,
    endpoint: SocketAddr,
}

/// One TCP connection, joined to its user by two channels.
struct Conn {
    handle: SocketHandle,
    /// Until established: where to hand over the pipe, and since when.
    opening: Option<(oneshot::Sender<Result<tokio::io::DuplexStream>>, std::time::Instant, IpAddr)>,
    from_app: Option<mpsc::Receiver<Bytes>>,
    to_app: Option<mpsc::Sender<Bytes>>,
    /// Part of a chunk from the app that did not fit the send buffer yet.
    pending: Option<Bytes>,
}

struct Lookup {
    handles: Vec<smoltcp::socket::dns::QueryHandle>,
    found: Vec<IpAddr>,
    reply: oneshot::Sender<Result<Vec<IpAddr>>>,
    since: std::time::Instant,
}

fn smol_ip(ip: IpAddr) -> IpAddress {
    match ip {
        IpAddr::V4(a) => IpAddress::Ipv4(a),
        IpAddr::V6(a) => IpAddress::Ipv6(a),
    }
}

fn std_ip(ip: IpAddress) -> IpAddr {
    match ip {
        IpAddress::Ipv4(a) => IpAddr::V4(a),
        IpAddress::Ipv6(a) => IpAddr::V6(a),
    }
}

/// The destination of an IP packet.
fn destination(p: &[u8]) -> Option<IpAddr> {
    match p.first()? >> 4 {
        4 if p.len() >= 20 => Some(IpAddr::V4(Ipv4Addr::new(p[16], p[17], p[18], p[19]))),
        6 if p.len() >= 40 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&p[24..40]).ok()?))),
        _ => None,
    }
}

/// A UDP socket with room for bursts: the system default (64 KiB on
/// Windows, about 200 KiB on Linux) drops datagrams at speed, which TCP
/// inside the tunnel takes as congestion. wireguard-go asks for 7 MiB.
fn udp_socket(bind: SocketAddr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_recv_buffer_size(4 << 20);
    let _ = s.set_send_buffer_size(4 << 20);
    s.set_nonblocking(true)?;
    s.bind(&bind.into())?;
    Ok(UdpSocket::from_std(s.into())?)
}

async fn start(config: WgConfig) -> Result<Tunnel> {
    let secret = StaticSecret::from(config.private_key);
    let mut peers = Vec::new();
    let mut receivers = Vec::new();
    let refused_flags: Arc<Vec<std::sync::atomic::AtomicBool>> =
        Arc::new(config.peers.iter().map(|_| Default::default()).collect());
    let (dgram_tx, dgram_rx) = mpsc::channel::<(usize, Vec<u8>)>(1024);
    for (i, p) in config.peers.iter().enumerate() {
        let endpoint = tokio::net::lookup_host(p.endpoint.as_str())
            .await
            .with_context(|| format!("looking up the WireGuard server {}", p.endpoint))?
            .next()
            .with_context(|| format!("the WireGuard server {} has no address", p.endpoint))?;
        let bind: SocketAddr = if endpoint.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
        let udp = Arc::new(udp_socket(bind).context("opening a UDP socket")?);
        udp.connect(endpoint).await.with_context(|| format!("reaching {endpoint}"))?;
        let tunn = Tunn::new(
            secret.clone(),
            PublicKey::from(p.public_key),
            p.preshared_key,
            p.keepalive,
            rand::random::<u32>() >> 8,
            None,
        );
        // Each peer's datagrams come in on its own socket. An error is not
        // the end: Windows reports a refused earlier datagram (ICMP) as
        // "connection reset" on the next receive, e.g. while the server
        // restarts.
        let (udp2, tx, refused) = (udp.clone(), dgram_tx.clone(), refused_flags.clone());
        let receiver = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match udp2.recv(&mut buf).await {
                    Ok(n) => {
                        if tx.send((i, buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) if tx.is_closed() => break,
                    Err(e) => {
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                        ) {
                            refused[i].store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                }
            }
        });
        receivers.push(receiver);
        peers.push(Peer { tunn, udp, endpoint });
    }
    let (cmd, cmd_rx) = mpsc::channel(64);
    let notify = Arc::new(Notify::new());
    let status = Arc::new(Mutex::new(TunnelStatus::default()));
    let mut task = Task::new(config, peers, status.clone(), notify.clone());
    task.refused = refused_flags;
    tokio::spawn(async move {
        task.run(cmd_rx, dgram_rx).await;
        // The tunnel is over: stop listening for its datagrams too.
        for r in receivers {
            r.abort();
        }
    });
    Ok(Tunnel { cmd, notify, status })
}

struct Task {
    config: WgConfig,
    peers: Vec<Peer>,
    dev: Queues,
    iface: Interface,
    sockets: SocketSet<'static>,
    dns: Option<SocketHandle>,
    conns: Vec<Conn>,
    lookups: Vec<Lookup>,
    status: Arc<Mutex<TunnelStatus>>,
    notify: Arc<Notify>,
    started: std::time::Instant,
    next_port: u16,
    scratch: Vec<u8>,
    refused: Arc<Vec<std::sync::atomic::AtomicBool>>,
    probes: Vec<(SocketHandle, oneshot::Sender<Result<()>>, std::time::Instant)>,
}

impl Task {
    fn new(config: WgConfig, peers: Vec<Peer>, status: Arc<Mutex<TunnelStatus>>, notify: Arc<Notify>) -> Self {
        let mut dev = Queues { mtu: config.mtu, ..Default::default() };
        let started = std::time::Instant::now();
        let mut iface =
            Interface::new(Config::new(HardwareAddress::Ip), &mut dev, smoltcp::time::Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            for (ip, len) in &config.addresses {
                let _ = a.push(IpCidr::new(smol_ip(*ip), *len));
            }
        });
        // Everything not on the tunnel's own subnet goes "out", which here
        // is always the tunnel; the gateway is never used as an address.
        for (ip, _) in &config.addresses {
            match ip {
                IpAddr::V4(a) => {
                    let _ = iface.routes_mut().add_default_ipv4_route(*a);
                }
                IpAddr::V6(a) => {
                    let _ = iface.routes_mut().add_default_ipv6_route(*a);
                }
            }
        }
        let mut sockets = SocketSet::new(Vec::new());
        let dns = (!config.dns.is_empty()).then(|| {
            let servers: Vec<IpAddress> = config.dns.iter().map(|d| smol_ip(*d)).collect();
            sockets.add(dns::Socket::new(&servers, (0..8).map(|_| None).collect::<Vec<_>>()))
        });
        let next_port = 40000 + (rand::random::<u16>() % 20000);
        Self {
            config,
            peers,
            dev,
            iface,
            sockets,
            dns,
            conns: Vec::new(),
            lookups: Vec::new(),
            status,
            notify,
            started,
            next_port,
            scratch: vec![0u8; 65536 + 256],
            refused: Default::default(),
            probes: Vec::new(),
        }
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_millis(self.started.elapsed().as_millis() as i64)
    }

    async fn send(peer: &Peer, data: &[u8]) {
        let _ = peer.udp.send(data).await;
    }

    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Cmd>, mut dgram_rx: mpsc::Receiver<(usize, Vec<u8>)>) {
        let mut timers = tokio::time::interval(Duration::from_millis(250));
        timers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let delay = self
                .iface
                .poll_delay(self.now(), &self.sockets)
                .map(|d| Duration::from_micros(d.total_micros()))
                .unwrap_or(Duration::from_millis(250))
                .min(Duration::from_millis(250));
            let notify = self.notify.clone();
            tokio::select! {
                c = cmd_rx.recv() => match c {
                    // Every handle on the tunnel is gone.
                    None => break,
                    Some(c) => self.command(c),
                },
                Some((i, d)) = dgram_rx.recv() => {
                    self.datagram(i, &d).await;
                    // Take whatever else has arrived before polling.
                    while let Ok((i, d)) = dgram_rx.try_recv() {
                        self.datagram(i, &d).await;
                    }
                }
                _ = timers.tick() => self.timers().await,
                _ = notify.notified() => {}
                _ = tokio::time::sleep(delay) => {}
            }
            // Until nothing moves: smoltcp, the connections, the wire.
            for _ in 0..64 {
                let now = self.now();
                self.iface.poll(now, &mut self.dev, &mut self.sockets);
                let moved = self.service();
                let sent = self.flush().await;
                if !moved && !sent && self.dev.rx.is_empty() {
                    break;
                }
            }
            self.lookups_service();
            self.probes_service();
        }
        // Tell the far side these connections are over.
        for c in &self.conns {
            self.sockets.get_mut::<tcp::Socket>(c.handle).abort();
        }
        let now = self.now();
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
        self.flush().await;
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Handshake => {
                for p in &mut self.peers {
                    if let TunnResult::WriteToNetwork(d) = p.tunn.format_handshake_initiation(&mut self.scratch, false)
                    {
                        let _ = p.udp.try_send(d);
                    }
                }
            }
            Cmd::Probe { to, payload, reply } => {
                let mut s = udp::Socket::new(
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
                );
                self.next_port = if self.next_port >= 60999 { 40000 } else { self.next_port + 1 };
                if s.bind(self.next_port).is_err() || s.send_slice(&payload, (smol_ip(to.0), to.1)).is_err() {
                    let _ = reply.send(Err(anyhow::anyhow!("could not send through the tunnel")));
                    return;
                }
                let h = self.sockets.add(s);
                self.probes.push((h, reply, std::time::Instant::now()));
            }
            Cmd::Connect { ip, port, reply } => {
                if self.config.peer_for(ip).is_none() {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "{ip} is not in the tunnel's AllowedIPs, so it cannot be reached through it"
                    )));
                    return;
                }
                if self.config.source_for(ip).is_none() {
                    let _ = reply.send(Err(anyhow::anyhow!("the tunnel has no address of the same kind as {ip}")));
                    return;
                }
                let mut s = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
                    tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
                );
                s.set_nagle_enabled(false);
                // Without congestion control a connection sends its whole
                // window at once; queues on the way overflow, the losses
                // stack up and the retransmission timer climbs to its
                // ten-second maximum. Cubic, like the desktop systems.
                s.set_congestion_control(tcp::CongestionControl::Cubic);
                s.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
                // A far end that stops answering (keepalives included) ends it.
                s.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
                self.next_port = if self.next_port >= 60999 { 40000 } else { self.next_port + 1 };
                if let Err(e) = s.connect(self.iface.context(), (smol_ip(ip), port), self.next_port) {
                    let _ = reply.send(Err(anyhow::anyhow!("could not open a connection to {ip}: {e}")));
                    return;
                }
                let handle = self.sockets.add(s);
                self.conns.push(Conn {
                    handle,
                    opening: Some((reply, std::time::Instant::now(), ip)),
                    from_app: None,
                    to_app: None,
                    pending: None,
                });
            }
            Cmd::Resolve { name, reply } => {
                let Some(h) = self.dns else {
                    // No name servers in the tunnel: this computer's.
                    tokio::spawn(async move {
                        let r = tokio::net::lookup_host((name.as_str(), 0))
                            .await
                            .map(|a| a.map(|s| s.ip()).collect())
                            .with_context(|| format!("looking up {name} (the tunnel has no DNS server)"));
                        let _ = reply.send(r);
                    });
                    return;
                };
                let mut handles = Vec::new();
                let types: &[DnsQueryType] = if self.config.addresses.iter().any(|(a, _)| a.is_ipv6()) {
                    &[DnsQueryType::A, DnsQueryType::Aaaa]
                } else {
                    &[DnsQueryType::A]
                };
                for t in types {
                    let cx = self.iface.context();
                    match self.sockets.get_mut::<dns::Socket>(h).start_query(cx, &name, *t) {
                        Ok(q) => handles.push(q),
                        Err(e) => {
                            let _ = reply.send(Err(anyhow::anyhow!("cannot look up {name}: {e:?}")));
                            return;
                        }
                    }
                }
                self.lookups.push(Lookup { handles, found: Vec::new(), reply, since: std::time::Instant::now() });
            }
        }
    }

    fn probes_service(&mut self) {
        let mut i = 0;
        while i < self.probes.len() {
            let (h, _, since) = &self.probes[i];
            let got = self.sockets.get_mut::<udp::Socket>(*h).recv().is_ok();
            if got || since.elapsed() > Duration::from_secs(5) {
                let (h, reply, _) = self.probes.remove(i);
                self.sockets.remove(h);
                let _ = reply.send(if got { Ok(()) } else { Err(anyhow::anyhow!("no answer")) });
            } else {
                i += 1;
            }
        }
    }

    fn lookups_service(&mut self) {
        let Some(h) = self.dns else { return };
        let mut i = 0;
        while i < self.lookups.len() {
            let l = &mut self.lookups[i];
            let s = self.sockets.get_mut::<dns::Socket>(h);
            let mut j = 0;
            while j < l.handles.len() {
                match s.get_query_result(l.handles[j]) {
                    Err(dns::GetQueryResultError::Pending) => j += 1,
                    Ok(ips) => {
                        l.found.extend(ips.into_iter().map(std_ip));
                        l.handles.remove(j);
                    }
                    Err(dns::GetQueryResultError::Failed) => {
                        l.handles.remove(j);
                    }
                }
            }
            let timed_out = l.since.elapsed() > Duration::from_secs(10);
            if l.handles.is_empty() || timed_out {
                for q in l.handles.drain(..) {
                    s.cancel_query(q);
                }
                let l = self.lookups.remove(i);
                // IPv4 first: the usual case, and what Address always has.
                let mut found = l.found;
                found.sort_by_key(|a| !a.is_ipv4());
                let _ = l.reply.send(if found.is_empty() {
                    Err(anyhow::anyhow!("the tunnel's DNS server found no address for that name"))
                } else {
                    Ok(found)
                });
            } else {
                i += 1;
            }
        }
    }

    /// Move bytes between sockets and their users. True if anything moved.
    fn service(&mut self) -> bool {
        let mut moved = false;
        let mut i = 0;
        while i < self.conns.len() {
            let c = &mut self.conns[i];
            let s = self.sockets.get_mut::<tcp::Socket>(c.handle);
            if let Some((_, since, ip)) = &c.opening {
                let ip = *ip;
                match s.state() {
                    tcp::State::Established => {
                        let (reply, ..) = c.opening.take().unwrap();
                        let (ours, theirs) = tokio::io::duplex(256 * 1024);
                        let (to_tunnel, from_app) = mpsc::channel::<Bytes>(32);
                        let (to_app, from_tunnel) = mpsc::channel::<Bytes>(32);
                        pumps(theirs, to_tunnel, from_tunnel, self.notify.clone());
                        c.from_app = Some(from_app);
                        c.to_app = Some(to_app);
                        if reply.send(Ok(ours)).is_err() {
                            s.abort();
                        }
                        moved = true;
                    }
                    tcp::State::Closed | tcp::State::TimeWait => {
                        let (reply, ..) = c.opening.take().unwrap();
                        let _ = reply.send(Err(anyhow::anyhow!("{ip} refused the connection (through the tunnel)")));
                        self.sockets.remove(c.handle);
                        self.conns.remove(i);
                        continue;
                    }
                    _ if since.elapsed() > CONNECT_TIMEOUT => {
                        let (reply, ..) = c.opening.take().unwrap();
                        let handshake = self.status.lock().unwrap().handshake;
                        let own = self.peers.iter().any(|p| p.endpoint.ip() == ip);
                        let why = if handshake && own {
                            format!("{ip} did not answer through the tunnel. {}", own_endpoint_hint(&self.config))
                        } else if handshake {
                            format!("{ip} did not answer through the tunnel")
                        } else {
                            "the WireGuard server did not answer (no handshake): check its Endpoint, that UDP can get \
                             there, and the keys"
                                .to_string()
                        };
                        let _ = reply.send(Err(anyhow::anyhow!(why)));
                        s.abort();
                        self.sockets.remove(c.handle);
                        self.conns.remove(i);
                        continue;
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            // App → tunnel.
            loop {
                if !s.can_send() {
                    break;
                }
                let chunk = match c.pending.take() {
                    Some(b) => b,
                    None => match c.from_app.as_mut().map(|r| r.try_recv()) {
                        Some(Ok(b)) => b,
                        Some(Err(mpsc::error::TryRecvError::Disconnected)) => {
                            c.from_app = None;
                            s.close();
                            moved = true;
                            break;
                        }
                        _ => break,
                    },
                };
                match s.send_slice(&chunk) {
                    Ok(n) if n < chunk.len() => {
                        c.pending = Some(chunk.slice(n..));
                        moved = true;
                        break;
                    }
                    Ok(_) => moved = true,
                    Err(_) => break,
                }
            }
            // Tunnel → app, while the app keeps up.
            while s.can_recv() {
                let Some(tx) = &c.to_app else { break };
                let Ok(permit) = tx.try_reserve() else { break };
                let mut buf = vec![0u8; 64 * 1024];
                match s.recv_slice(&mut buf) {
                    Ok(n) if n > 0 => {
                        buf.truncate(n);
                        permit.send(Bytes::from(buf));
                        moved = true;
                    }
                    _ => break,
                }
            }
            // The far side finished sending: end of file for the app.
            if !s.may_recv() && !s.can_recv() && c.to_app.is_some() {
                c.to_app = None;
                moved = true;
            }
            let done = matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait)
                || (c.to_app.is_none() && c.from_app.is_none() && s.state() == tcp::State::Closed);
            if done {
                self.sockets.remove(c.handle);
                self.conns.remove(i);
                continue;
            }
            i += 1;
        }
        moved
    }

    /// Encrypt and send what smoltcp produced. True if anything went out.
    async fn flush(&mut self) -> bool {
        let mut sent = false;
        while let Some(p) = self.dev.tx.pop_front() {
            let Some(dst) = destination(&p) else { continue };
            let Some(i) = self.config.peer_for(dst) else { continue };
            let peer = &mut self.peers[i];
            match peer.tunn.encapsulate(&p, &mut self.scratch) {
                TunnResult::WriteToNetwork(d) => {
                    let d = d.to_vec();
                    Self::send(&self.peers[i], &d).await;
                    sent = true;
                }
                TunnResult::Err(e) => tracing::debug!("wireguard: {e:?}"),
                _ => {}
            }
        }
        sent
    }

    async fn datagram(&mut self, i: usize, d: &[u8]) {
        let Some(peer) = self.peers.get_mut(i) else { return };
        let mut first = true;
        loop {
            let src = if first { d } else { &[][..] };
            let from = Some(peer.endpoint.ip());
            match peer.tunn.decapsulate(from, src, &mut self.scratch) {
                TunnResult::WriteToNetwork(out) => {
                    let out = out.to_vec();
                    let _ = peer.udp.send(&out).await;
                    // Packets queued while there was no session go now.
                    first = false;
                    continue;
                }
                TunnResult::WriteToTunnelV4(p, _) | TunnResult::WriteToTunnelV6(p, _) => {
                    self.dev.rx.push_back(p.to_vec());
                }
                TunnResult::Err(e) => tracing::debug!("wireguard: {e:?}"),
                TunnResult::Done => {}
            }
            break;
        }
        self.update_status();
    }

    async fn timers(&mut self) {
        for i in 0..self.peers.len() {
            let peer = &mut self.peers[i];
            if let TunnResult::WriteToNetwork(d) = peer.tunn.update_timers(&mut self.scratch) {
                let d = d.to_vec();
                let _ = peer.udp.send(&d).await;
            }
        }
        self.update_status();
    }

    fn update_status(&self) {
        let mut st = self.status.lock().unwrap();
        st.handshake = self.peers.iter().any(|p| p.tunn.time_since_last_handshake().is_some());
        let (mut tx, mut rx) = (0, 0);
        for p in &self.peers {
            let (_, t, r, ..) = p.tunn.stats();
            tx += t as u64;
            rx += r as u64;
        }
        st.tx_bytes = tx;
        st.rx_bytes = rx;
        st.peers = self
            .peers
            .iter()
            .enumerate()
            .map(|(i, p)| PeerStatus {
                endpoint: p.endpoint.to_string(),
                handshake: p.tunn.time_since_last_handshake().is_some(),
                refused: self.refused.get(i).is_some_and(|r| r.load(std::sync::atomic::Ordering::Relaxed)),
            })
            .collect();
    }
}

/// Join one connection's pipe to the tunnel task: what the app writes goes
/// to `to_tunnel`, what `from_tunnel` brings is written for the app. Each
/// side wakes the task when it has moved something.
fn pumps(
    pipe: tokio::io::DuplexStream,
    to_tunnel: mpsc::Sender<Bytes>,
    mut from_tunnel: mpsc::Receiver<Bytes>,
    notify: Arc<Notify>,
) {
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let n1 = notify.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_tunnel.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                        break;
                    }
                    n1.notify_one();
                }
            }
        }
        drop(to_tunnel);
        n1.notify_one();
    });
    tokio::spawn(async move {
        while let Some(b) = from_tunnel.recv().await {
            if wr.write_all(&b).await.is_err() {
                break;
            }
            // Room again in the channel: the task may read more.
            notify.notify_one();
        }
        let _ = wr.shutdown().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "\
[Interface]
PrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=
Address = 10.0.0.2/32, fd00::2/128
DNS = 10.0.0.1, corp.example
MTU = 1380
ListenPort = 51820   # ignored

[Peer]
PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=
PresharedKey = /UwcSPg38hW/D9Y3tcS1FOV0K1wuURMbS0sesJEP5ak=
Endpoint = vpn.example.com:51820
AllowedIPs = 10.0.0.0/24, 192.168.10.0/24
PersistentKeepalive = 25

[Peer]
PublicKey = TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=
Endpoint = 203.0.113.9:51820
AllowedIPs = 192.168.10.128/25
";

    #[test]
    fn parses_wg_quick_configs() {
        let c = WgConfig::parse(CONF).unwrap();
        assert_eq!(c.addresses, [("10.0.0.2".parse().unwrap(), 32), ("fd00::2".parse().unwrap(), 128)]);
        assert_eq!(c.dns, ["10.0.0.1".parse::<IpAddr>().unwrap()], "search domains are not servers");
        assert_eq!(c.mtu, 1380);
        assert_eq!(c.peers.len(), 2);
        assert_eq!(c.peers[0].endpoint, "vpn.example.com:51820");
        assert_eq!(c.peers[0].keepalive, Some(25));
        assert!(c.peers[0].preshared_key.is_some());
        // Longest prefix wins.
        assert_eq!(c.peer_for("10.0.0.7".parse().unwrap()), Some(0));
        assert_eq!(c.peer_for("192.168.10.5".parse().unwrap()), Some(0));
        assert_eq!(c.peer_for("192.168.10.200".parse().unwrap()), Some(1));
        assert_eq!(c.peer_for("8.8.8.8".parse().unwrap()), None);
    }

    #[test]
    fn the_private_key_is_kept_apart() {
        let key = WgConfig::private_key_text(CONF).unwrap();
        assert_eq!(key, "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=");
        let stripped = WgConfig::strip_private_key(CONF);
        assert!(!stripped.contains(&key));
        assert!(WgConfig::parse(&stripped).is_err());
        assert_eq!(WgConfig::parse_with_key(&stripped, &key).unwrap(), WgConfig::parse(CONF).unwrap());
    }

    #[test]
    fn editing_never_shows_the_key() {
        let key = WgConfig::private_key_text(CONF).unwrap();
        let stored = WgConfig::strip_private_key(CONF);
        // Stored with or without the key, the editor shows neither.
        for text in [stored.as_str(), CONF] {
            let shown = WgConfig::for_editing(text, KEY_NOT_SHOWN);
            assert!(!shown.contains(&key), "{shown}");
            assert!(shown.starts_with("[Interface]\nPrivateKey = (saved, not shown)\n"), "{shown}");
        }
        let shown = WgConfig::for_editing(&stored, KEY_NOT_SHOWN);

        // Left alone: the saved key stays, the rest is stored as edited.
        let edit = shown.replace("AllowedIPs = 192.168.10.128/25", "AllowedIPs = 192.168.10.128/25, 31.7.87.10/32");
        let e = check_edit(&edit, None).unwrap();
        assert_eq!(e.new_key, None);
        assert!(e.config.contains("31.7.87.10/32"));
        assert!(!e.config.to_lowercase().contains("privatekey"));
        let c = WgConfig::parse_with_key(&e.config, &key).unwrap();
        assert_eq!(c.peer_for("31.7.87.10".parse().unwrap()), Some(1));

        // Taking the line out keeps it too.
        let e = check_edit(&WgConfig::strip_private_key(&shown), None).unwrap();
        assert_eq!(e.new_key, None);

        // A new key replaces it; a bad one is refused, saying so.
        let other = "cHJpdmF0ZWtleXByaXZhdGVrZXlwcml2YXRla2V5MDA=";
        let e = check_edit(&shown.replace(KEY_NOT_SHOWN, other), None).unwrap();
        assert_eq!(e.new_key.as_deref(), Some(other));
        let err = format!("{:#}", check_edit(&shown.replace(KEY_NOT_SHOWN, "nonsense"), None).unwrap_err());
        assert!(err.contains("not a WireGuard key"), "{err}");

        // A file loaded into the editor brings its key, unseen.
        let loaded = WgConfig::for_editing(CONF, KEY_FROM_FILE);
        assert!(!loaded.contains(&key));
        let e = check_edit(&loaded, Some(&key)).unwrap();
        assert_eq!(e.new_key.as_deref(), Some(key.as_str()));

        // Mistakes elsewhere are refused before anything is saved.
        assert!(check_edit(&shown.replace("Endpoint = 203.0.113.9:51820\n", ""), None).is_err());
        assert!(check_edit(&shown.replace("[Interface]", "[Interfac]"), None).is_err());
    }

    #[test]
    fn guesses_the_servers_tunnel_address() {
        let c = WgConfig::parse(CONF).unwrap();
        assert_eq!(tunnel_side_guess(&c), Some("10.0.0.1".parse().unwrap()), "a /32 guesses .1 of its /24");
        let c = WgConfig::parse(&CONF.replace("10.0.0.2/32", "10.8.4.7/22")).unwrap();
        assert_eq!(tunnel_side_guess(&c), Some("10.8.4.1".parse().unwrap()));
        let c = WgConfig::parse(&CONF.replace("10.0.0.2/32", "10.0.0.1/24")).unwrap();
        assert_eq!(tunnel_side_guess(&c), None, "not this side's own address");
    }

    #[test]
    fn refuses_what_it_cannot_use() {
        let no_endpoint = CONF.replace("Endpoint = 203.0.113.9:51820\n", "");
        assert!(format!("{:#}", WgConfig::parse(&no_endpoint).unwrap_err()).contains("Endpoint"));
        let bad_key = CONF.replace("xTIBA5", "short");
        assert!(WgConfig::parse(&bad_key).is_err());
        assert!(WgConfig::parse("[Interface]\nAddress = 10.0.0.2/32\n").is_err());
    }

    #[test]
    fn packet_destinations() {
        let mut v4 = vec![0x45u8; 20];
        v4[16..20].copy_from_slice(&[10, 9, 9, 1]);
        assert_eq!(destination(&v4), Some("10.9.9.1".parse().unwrap()));
        assert_eq!(destination(&[0x45, 0]), None);
        assert!(contains(("0.0.0.0".parse().unwrap(), 0), "1.2.3.4".parse().unwrap()));
    }
}
