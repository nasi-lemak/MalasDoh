//! Port forwarding: local (`ssh -L`), remote (`ssh -R`) and dynamic SOCKS5
//! (`ssh -D`), saved per profile and started on demand or with the connection.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::ssh::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardKind {
    /// Listen here, connect from the server: reach a service behind it.
    #[default]
    Local,
    /// Listen on the server, connect from here: expose a local service to it.
    Remote,
    /// A SOCKS5 proxy here; every connection goes out through the server.
    Dynamic,
}

impl ForwardKind {
    pub fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "Local (-L)",
            ForwardKind::Remote => "Remote (-R)",
            ForwardKind::Dynamic => "SOCKS (-D)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForwardSpec {
    pub name: String,
    pub kind: ForwardKind,
    /// Where to listen: this computer for Local/Dynamic, the server for Remote.
    pub bind_host: String,
    /// 0 picks a free port.
    pub bind_port: u16,
    /// Where connections go: as seen from the server for Local, from this
    /// computer for Remote. Unused for Dynamic.
    pub target_host: String,
    pub target_port: u16,
    /// Start when the connection opens.
    pub auto_start: bool,
}

impl Default for ForwardSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            target_host: "127.0.0.1".into(),
            target_port: 0,
            auto_start: false,
        }
    }
}

impl ForwardSpec {
    /// `8080 → db:5432` style summary.
    pub fn describe(&self) -> String {
        let bind = format!("{}:{}", self.bind_host, self.bind_port);
        match self.kind {
            ForwardKind::Local => format!("local {bind} -> {}:{} (via server)", self.target_host, self.target_port),
            ForwardKind::Remote => format!("server {bind} -> {}:{} (here)", self.target_host, self.target_port),
            ForwardKind::Dynamic => format!("SOCKS5 proxy on {bind}"),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.bind_host.trim().is_empty() {
            bail!("listen address is required (127.0.0.1 keeps it private to this machine)");
        }
        if self.kind != ForwardKind::Dynamic && (self.target_host.trim().is_empty() || self.target_port == 0) {
            bail!("target host and port are required");
        }
        Ok(())
    }
}

/// Counters for the UI.
#[derive(Debug, Default)]
pub struct ForwardStats {
    pub active: AtomicU64,
    pub total: AtomicU64,
    pub bytes: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

impl ForwardStats {
    fn error(&self, e: impl std::fmt::Display) {
        *self.last_error.lock().unwrap() = Some(e.to_string());
    }
}

/// A running forward. Dropping it stops a local or dynamic listener; remote
/// forwards also need [`Connection::stop_forward`] to tell the server.
pub struct ForwardHandle {
    pub spec: ForwardSpec,
    /// The port actually listened on (useful when 0 was asked for).
    pub port: u16,
    pub stats: Arc<ForwardStats>,
    task: Option<JoinHandle<()>>,
}

impl Drop for ForwardHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Server-side listeners we asked for, by port, mapped to local targets. The
/// SSH handler consults it when the server opens a `forwarded-tcpip` channel.
pub type RemoteForwards = Arc<Mutex<HashMap<u32, (String, u16, Arc<ForwardStats>)>>>;

/// Handle a connection the server forwarded to us.
pub(crate) fn accept_forwarded(map: &RemoteForwards, port: u32, channel: russh::Channel<russh::client::Msg>) -> bool {
    let Some((host, tport, stats)) = map.lock().unwrap().get(&port).cloned() else { return false };
    tokio::spawn(async move {
        stats.total.fetch_add(1, Ordering::Relaxed);
        stats.active.fetch_add(1, Ordering::Relaxed);
        match TcpStream::connect((host.as_str(), tport)).await {
            Ok(mut tcp) => {
                let mut ch = channel.into_stream();
                if let Ok((a, b)) = tokio::io::copy_bidirectional(&mut tcp, &mut ch).await {
                    stats.bytes.fetch_add(a + b, Ordering::Relaxed);
                }
            }
            Err(e) => {
                stats.error(format!("could not reach {host}:{tport}: {e}"));
                let _ = channel.close().await;
            }
        }
        stats.active.fetch_sub(1, Ordering::Relaxed);
    });
    true
}

impl Connection {
    pub async fn start_forward(self: &Arc<Self>, spec: &ForwardSpec) -> Result<ForwardHandle> {
        spec.validate()?;
        let stats = Arc::new(ForwardStats::default());
        match spec.kind {
            ForwardKind::Remote => {
                let port =
                    self.handle().tcpip_forward(spec.bind_host.clone(), spec.bind_port as u32).await.with_context(
                        || format!("the server refused to listen on {}:{}", spec.bind_host, spec.bind_port),
                    )?;
                // The server answers 0 when it used the port we asked for.
                let port = if port == 0 { spec.bind_port as u32 } else { port };
                self.remote_forwards()
                    .lock()
                    .unwrap()
                    .insert(port, (spec.target_host.clone(), spec.target_port, stats.clone()));
                Ok(ForwardHandle { spec: spec.clone(), port: port as u16, stats, task: None })
            }
            ForwardKind::Local | ForwardKind::Dynamic => {
                let listener = TcpListener::bind((spec.bind_host.as_str(), spec.bind_port))
                    .await
                    .with_context(|| format!("could not listen on {}:{}", spec.bind_host, spec.bind_port))?;
                let port = listener.local_addr()?.port();
                let conn = self.clone();
                let (spec2, stats2) = (spec.clone(), stats.clone());
                let task = tokio::spawn(async move {
                    loop {
                        let Ok((tcp, peer)) = listener.accept().await else { continue };
                        let (conn, spec, stats) = (conn.clone(), spec2.clone(), stats2.clone());
                        tokio::spawn(async move {
                            stats.total.fetch_add(1, Ordering::Relaxed);
                            stats.active.fetch_add(1, Ordering::Relaxed);
                            let r = serve(&conn, &spec, tcp, peer, &stats).await;
                            if let Err(e) = r {
                                stats.error(format!("{e:#}"));
                            }
                            stats.active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                });
                Ok(ForwardHandle { spec: spec.clone(), port, stats, task: Some(task) })
            }
        }
    }

    /// Stop a forward, telling the server for remote ones.
    pub async fn stop_forward(&self, h: ForwardHandle) {
        if h.spec.kind == ForwardKind::Remote {
            self.remote_forwards().lock().unwrap().remove(&(h.port as u32));
            let _ = self.handle().cancel_tcpip_forward(h.spec.bind_host.clone(), h.port as u32).await;
        }
        drop(h);
    }
}

async fn serve(
    conn: &Connection,
    spec: &ForwardSpec,
    mut tcp: TcpStream,
    peer: std::net::SocketAddr,
    stats: &ForwardStats,
) -> Result<()> {
    tcp.set_nodelay(true).ok();
    let (host, port) = match spec.kind {
        ForwardKind::Dynamic => socks5_handshake(&mut tcp).await?,
        _ => (spec.target_host.clone(), spec.target_port),
    };
    let ch = conn
        .handle()
        .channel_open_direct_tcpip(host.clone(), port as u32, peer.ip().to_string(), peer.port() as u32)
        .await;
    let ch = match ch {
        Ok(ch) => {
            if spec.kind == ForwardKind::Dynamic {
                tcp.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            }
            ch
        }
        Err(e) => {
            if spec.kind == ForwardKind::Dynamic {
                // 0x05: connection refused.
                let _ = tcp.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            }
            bail!("the server could not reach {host}:{port}: {e}");
        }
    };
    let mut stream = ch.into_stream();
    let (a, b) = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await.unwrap_or((0, 0));
    stats.bytes.fetch_add(a + b, Ordering::Relaxed);
    Ok(())
}

/// The server side of a SOCKS5 CONNECT (RFC 1928), no authentication.
/// Returns the requested destination.
async fn socks5_handshake(s: &mut TcpStream) -> Result<(String, u16)> {
    let mut hdr = [0u8; 2];
    s.read_exact(&mut hdr).await?;
    if hdr[0] != 5 {
        bail!("not a SOCKS5 client (version {})", hdr[0]);
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).await?;
        bail!("SOCKS client wants authentication; only 'no authentication' is offered");
    }
    s.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[1] != 1 {
        // 0x07: command not supported (only CONNECT).
        s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        bail!("unsupported SOCKS command {}", req[1]);
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            IpAddr::V4(Ipv4Addr::from(a)).to_string()
        }
        3 => {
            let len = s.read_u8().await? as usize;
            let mut name = vec![0u8; len];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).context("SOCKS host name is not UTF-8")?
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            IpAddr::V6(Ipv6Addr::from(a)).to_string()
        }
        t => bail!("unknown SOCKS address type {t}"),
    };
    let port = s.read_u16().await?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socks5_parses_domain_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&[5, 1, 0]).await.unwrap();
            let mut r = [0u8; 2];
            c.read_exact(&mut r).await.unwrap();
            assert_eq!(r, [5, 0]);
            let mut req = vec![5, 1, 0, 3, 11];
            req.extend_from_slice(b"example.com");
            req.extend_from_slice(&443u16.to_be_bytes());
            c.write_all(&req).await.unwrap();
        });
        let (mut s, _) = listener.accept().await.unwrap();
        assert_eq!(socks5_handshake(&mut s).await.unwrap(), ("example.com".into(), 443));
        client.await.unwrap();
    }

    #[test]
    fn specs_validate_and_describe() {
        let mut f = ForwardSpec { bind_port: 8080, target_host: "db".into(), target_port: 5432, ..Default::default() };
        assert!(f.validate().is_ok());
        assert_eq!(f.describe(), "local 127.0.0.1:8080 -> db:5432 (via server)");
        f.target_port = 0;
        assert!(f.validate().is_err());
        f.kind = ForwardKind::Dynamic;
        assert!(f.validate().is_ok());
    }
}
