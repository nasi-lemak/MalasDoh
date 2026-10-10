//! Reaching the first SSH server through a proxy: SOCKS5, HTTP `CONNECT`, or
//! a command whose input and output carry the connection (`ProxyCommand`).

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proxy {
    #[default]
    None,
    /// `user` empty: no authentication. The password is kept in the keychain.
    Socks5 {
        host: String,
        port: u16,
        #[serde(default)]
        user: String,
    },
    Http {
        host: String,
        port: u16,
        #[serde(default)]
        user: String,
    },
    /// Run locally; `%h`, `%p` and `%r` become the server's host, port and
    /// user, as in OpenSSH's ProxyCommand.
    Command { command: String },
}

impl Proxy {
    pub fn is_none(&self) -> bool {
        matches!(self, Proxy::None)
    }

    /// The proxy's user name, when it needs a password.
    pub fn auth_user(&self) -> Option<&str> {
        match self {
            Proxy::Socks5 { user, .. } | Proxy::Http { user, .. } if !user.is_empty() => Some(user),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Proxy::None => "direct".into(),
            Proxy::Socks5 { host, port, .. } => format!("SOCKS5 {host}:{port}"),
            Proxy::Http { host, port, .. } => format!("HTTP proxy {host}:{port}"),
            Proxy::Command { command } => format!("command `{command}`"),
        }
    }
}

/// A byte stream to the SSH server, whichever way it was reached.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Open a connection to `host:port` through `proxy`. `password` is the
/// proxy's, when it asked for one.
pub async fn connect(
    proxy: &Proxy,
    host: &str,
    port: u16,
    ssh_user: &str,
    password: Option<&str>,
) -> Result<Box<dyn Stream>> {
    match proxy {
        Proxy::None => Ok(Box::new(tcp(host, port).await?)),
        Proxy::Socks5 { host: ph, port: pp, user } => {
            let s = tcp(ph, *pp).await.context("reaching the SOCKS proxy")?;
            let auth = (!user.is_empty()).then(|| (user.as_str(), password.unwrap_or("")));
            Ok(Box::new(socks5(s, host, port, auth).await?))
        }
        Proxy::Http { host: ph, port: pp, user } => {
            let s = tcp(ph, *pp).await.context("reaching the HTTP proxy")?;
            let auth = (!user.is_empty()).then(|| (user.as_str(), password.unwrap_or("")));
            Ok(Box::new(http_connect(s, host, port, auth).await?))
        }
        Proxy::Command { command } => Ok(Box::new(spawn_command(command, host, port, ssh_user)?)),
    }
}

async fn tcp(host: &str, port: u16) -> Result<TcpStream> {
    let s = tokio::time::timeout(std::time::Duration::from_secs(20), TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to {host}:{port}"))?
        .with_context(|| format!("could not reach {host}:{port}"))?;
    s.set_nodelay(true).ok();
    Ok(s)
}

#[derive(Debug, thiserror::Error)]
#[error("the proxy refused the user name or password")]
pub struct ProxyAuthFailed;

/// RFC 1928 client, with RFC 1929 user/password when given. The server name
/// is sent as a name, so it is resolved by the proxy (like `socks5h`).
pub async fn socks5<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    host: &str,
    port: u16,
    auth: Option<(&str, &str)>,
) -> Result<S> {
    let method = if auth.is_some() { 2u8 } else { 0u8 };
    s.write_all(&[5, 1, method]).await?;
    let mut reply = [0u8; 2];
    s.read_exact(&mut reply).await.context("the SOCKS proxy closed the connection")?;
    if reply[0] != 5 {
        bail!("not a SOCKS5 proxy");
    }
    match (reply[1], auth) {
        (0, _) => {}
        (2, Some((user, pass))) => {
            if user.len() > 255 || pass.len() > 255 {
                bail!("SOCKS user name and password are limited to 255 bytes");
            }
            let mut m = vec![1, user.len() as u8];
            m.extend_from_slice(user.as_bytes());
            m.push(pass.len() as u8);
            m.extend_from_slice(pass.as_bytes());
            s.write_all(&m).await?;
            let mut r = [0u8; 2];
            s.read_exact(&mut r).await?;
            if r[1] != 0 {
                return Err(ProxyAuthFailed.into());
            }
        }
        (0xff, _) => bail!("the SOCKS proxy accepts none of our login methods"),
        (m, _) => bail!("the SOCKS proxy chose an unsupported login method ({m})"),
    }
    if host.len() > 255 {
        bail!("host name too long for SOCKS");
    }
    let mut req = vec![5, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0 {
        let why = match head[1] {
            1 => "general failure",
            2 => "not allowed by its rules",
            3 => "network unreachable",
            4 => "host unreachable",
            5 => "connection refused",
            6 => "TTL expired",
            7 => "command not supported",
            8 => "address type not supported",
            _ => "unknown error",
        };
        bail!("the SOCKS proxy could not reach {host}:{port}: {why}");
    }
    // Skip the bound address it reports.
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            n[0] as usize
        }
        t => bail!("the SOCKS proxy sent an unknown address type {t}"),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(s)
}

/// HTTP `CONNECT` tunnel. The reply is read byte by byte up to the blank
/// line, so no SSH bytes after it are swallowed.
pub async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    host: &str,
    port: u16,
    auth: Option<(&str, &str)>,
) -> Result<S> {
    use base64::Engine;
    let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some((u, p)) = auth {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await?;
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).await? == 0 {
            bail!("the HTTP proxy closed the connection");
        }
        head.push(b[0]);
        if head.len() > 16 * 1024 {
            bail!("the HTTP proxy's reply is too long");
        }
    }
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or("");
    let code = status.split_whitespace().nth(1).unwrap_or("");
    match code {
        "200" => Ok(s),
        "407" => Err(ProxyAuthFailed.into()),
        _ => bail!("the HTTP proxy refused: {status}"),
    }
}

/// Expand `%h`, `%p`, `%r` and `%%`.
pub fn expand_command(cmd: &str, host: &str, port: u16, user: &str) -> String {
    let mut out = String::new();
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(host),
            Some('p') => out.push_str(&port.to_string()),
            Some('r') => out.push_str(user),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

/// A ProxyCommand's stdin/stdout as one stream. The command is stopped when
/// the stream is dropped.
pub struct CommandStream {
    _child: tokio::process::Child,
    out: tokio::process::ChildStdout,
    inp: tokio::process::ChildStdin,
}

fn spawn_command(cmd: &str, host: &str, port: u16, user: &str) -> Result<CommandStream> {
    let line = expand_command(cmd, host, port, user);
    #[cfg(windows)]
    let mut c = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&line);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&line);
        c
    };
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = c.spawn().with_context(|| format!("starting the proxy command `{line}`"))?;
    let out = child.stdout.take().context("proxy command without stdout")?;
    let inp = child.stdin.take().context("proxy command without stdin")?;
    Ok(CommandStream { _child: child, out, inp })
}

impl AsyncRead for CommandStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.out).poll_read(cx, buf)
    }
}

impl AsyncWrite for CommandStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inp).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inp).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inp).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_placeholders() {
        assert_eq!(expand_command("nc -X 5 -x proxy:1080 %h %p", "web", 22, "me"), "nc -X 5 -x proxy:1080 web 22");
        assert_eq!(expand_command("echo %r@%h %% %x %", "h", 2, "u"), "echo u@h % %x %");
    }

    #[tokio::test]
    async fn socks5_handshake_with_password() {
        let (client, mut server) = tokio::io::duplex(1024);
        let srv = tokio::spawn(async move {
            let mut b = [0u8; 3];
            server.read_exact(&mut b).await.unwrap();
            assert_eq!(b, [5, 1, 2]);
            server.write_all(&[5, 2]).await.unwrap();
            let mut auth = [0u8; 1 + 1 + 2 + 1 + 3];
            server.read_exact(&mut auth).await.unwrap();
            assert_eq!(&auth, b"\x01\x02me\x03pwd");
            server.write_all(&[1, 0]).await.unwrap();
            let mut req = [0u8; 5 + 7 + 2];
            server.read_exact(&mut req).await.unwrap();
            assert_eq!(&req[..5], &[5, 1, 0, 3, 7]);
            assert_eq!(&req[5..12], b"web.lan");
            assert_eq!(&req[12..], &22u16.to_be_bytes());
            server.write_all(&[5, 0, 0, 1, 10, 0, 0, 1, 0, 22]).await.unwrap();
            server.write_all(b"SSH-2.0-test\r\n").await.unwrap();
        });
        let mut s = socks5(client, "web.lan", 22, Some(("me", "pwd"))).await.unwrap();
        let mut banner = [0u8; 14];
        s.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"SSH-2.0-test\r\n", "nothing after the reply is lost");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn http_connect_and_refusals() {
        let (client, mut server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let mut got = Vec::new();
            let mut b = [0u8; 1];
            while !got.ends_with(b"\r\n\r\n") {
                server.read_exact(&mut b).await.unwrap();
                got.push(b[0]);
            }
            let text = String::from_utf8(got).unwrap();
            assert!(text.starts_with("CONNECT db:2222 HTTP/1.1\r\n"), "{text}");
            assert!(text.contains("Proxy-Authorization: Basic dTpw\r\n"), "{text}");
            server.write_all(b"HTTP/1.1 200 Connection established\r\nVia: x\r\n\r\nSSH-2.0-x\r\n").await.unwrap();
        });
        let mut s = http_connect(client, "db", 2222, Some(("u", "p"))).await.unwrap();
        let mut banner = [0u8; 11];
        s.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"SSH-2.0-x\r\n");
        srv.await.unwrap();

        let (client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut b = [0u8; 64];
            let _ = server.read(&mut b).await;
            let _ = server.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await;
        });
        let err = http_connect(client, "db", 22, None).await.unwrap_err();
        assert!(err.downcast_ref::<ProxyAuthFailed>().is_some(), "{err:#}");
    }
}
