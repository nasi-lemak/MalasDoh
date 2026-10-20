//! Remote Desktop through MalasDoh's connections: the system's own client
//! (`mstsc` on Windows, FreeRDP or Remmina on Linux, Windows App on macOS)
//! opened on a local port that MalasDoh forwards to the desktop.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};

/// The `.rdp` file for a desktop reached at `addr` (`127.0.0.1:port` when
/// forwarded).
pub fn rdp_file(addr: &str, user: &str) -> String {
    let mut s = format!("full address:s:{addr}\r\n");
    if !user.trim().is_empty() {
        s.push_str(&format!("username:s:{}\r\n", user.trim()));
    }
    s
}

/// Writes the `.rdp` file into `dir`, named after the desktop.
pub fn write_rdp_file(dir: &Path, name: &str, addr: &str, user: &str) -> Result<PathBuf> {
    crate::paths::create_private_dir(dir)?;
    let safe: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let path = dir.join(format!("{safe}.rdp"));
    crate::paths::write_private_atomic(&path, rdp_file(addr, user).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// An RDP hello: a TPKT with an X.224 connection request asking for TLS
/// or CredSSP, as clients send first.
pub fn hello() -> Vec<u8> {
    let neg = [0x01, 0x00, 0x08, 0x00, 0x03, 0x00, 0x00, 0x00];
    let mut x224 = vec![(6 + neg.len()) as u8, 0xe0, 0, 0, 0, 0, 0];
    x224.extend_from_slice(&neg);
    let mut pkt = vec![0x03, 0x00, 0, 0];
    pkt.extend_from_slice(&x224);
    let len = pkt.len() as u16;
    pkt[2..4].copy_from_slice(&len.to_be_bytes());
    pkt
}

/// Is the answer to [`hello`] from a Remote Desktop server (a TPKT
/// carrying an X.224 connection confirm)?
pub fn is_rdp_answer(b: &[u8]) -> bool {
    b.len() >= 6 && b[0] == 0x03 && b[5] == 0xd0
}

/// Whether the server of `conn` reaches a Remote Desktop server at
/// `host:port`: sends an RDP hello through a forwarded channel and waits
/// for its answer. (Some servers, Teleport's among them, accept the
/// channel before trying the address, so opening it proves nothing.)
pub async fn probe(conn: &crate::ssh::Connection, host: &str, port: u16, within: std::time::Duration) -> Result<()> {
    let work = async {
        let ch = conn
            .handle()
            .channel_open_direct_tcpip(host.to_string(), port as u32, "127.0.0.1".to_string(), 0)
            .await
            .with_context(|| format!("it refused to forward to {host}:{port}"))?;
        probe_stream(ch.into_stream(), &format!("{host}:{port}"), "from there").await
    };
    match tokio::time::timeout(within, work).await {
        Ok(r) => r,
        Err(_) => bail!("no answer from {host}:{port} through it in {} s", within.as_secs()),
    }
}

/// Sends an RDP hello on `s` (a connection to `what`) and checks the
/// answer is Remote Desktop's.
pub async fn probe_stream<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut s: S,
    what: &str,
    from: &str,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    s.write_all(&hello()).await?;
    let mut got = [0u8; 11];
    let mut n = 0;
    while n < got.len() {
        match s.read(&mut got[n..]).await? {
            0 => break,
            k => n += k,
        }
    }
    let _ = s.shutdown().await;
    match n {
        0 => bail!("nothing answers at {what} {from}"),
        _ if is_rdp_answer(&got[..n]) => Ok(()),
        _ => bail!("{what} answers {from}, but not as Remote Desktop"),
    }
}

/// A local port (on 127.0.0.1) whose connections go wherever `dial` takes
/// them: through a WireGuard tunnel or another proxy. Ends when the
/// handle is aborted.
pub async fn listen<F, Fut>(dial: F) -> Result<(u16, tokio::task::JoinHandle<()>)>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Box<dyn crate::proxy::Stream>>> + Send,
{
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let dial = std::sync::Arc::new(dial);
    let task = tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tcp.set_nodelay(true).ok();
            let dial = dial.clone();
            tokio::spawn(async move {
                match dial().await {
                    Ok(mut far) => {
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut far).await;
                    }
                    Err(e) => tracing::warn!("remote desktop: {e:#}"),
                }
            });
        }
    });
    Ok((port, task))
}

/// The Remote Desktop programs tried on this system, in order.
pub fn clients() -> &'static [&'static str] {
    if cfg!(windows) {
        &["mstsc"]
    } else if cfg!(target_os = "macos") {
        &["open"]
    } else {
        &["xfreerdp3", "xfreerdp", "remmina"]
    }
}

/// The Remote Desktop program to use here: the first of [`clients`] that
/// is installed (on Windows and macOS, the system's own).
pub fn client() -> Option<&'static str> {
    if cfg!(any(windows, target_os = "macos")) {
        return clients().first().copied();
    }
    let path = std::env::var_os("PATH")?;
    clients().iter().copied().find(|c| std::env::split_paths(&path).any(|d| d.join(c).is_file()))
}

/// FreeRDP has no login dialog: MalasDoh asks for the password and hands
/// it over on its standard input (never on the command line, where other
/// users could see it).
pub fn wants_password(client: &str) -> bool {
    client.starts_with("xfreerdp")
}

/// The command for `client` to open `file` (the desktop at `addr`).
fn command(client: &str, file: &Path, addr: &str, user: &str, password: bool) -> Command {
    let mut c = Command::new(client);
    match client {
        "xfreerdp3" | "xfreerdp" => {
            c.arg(format!("/v:{addr}"));
            let user = user.trim();
            if !user.is_empty() {
                c.arg(format!("/u:{user}"));
                // Without a domain it would ask for one too.
                if !user.contains('\\') && !user.contains('@') {
                    c.arg("/d:");
                }
            }
            // Trust the desktop's certificate the first time, then insist
            // on it; the window follows its size; the clipboard is shared.
            c.args(["/cert:tofu", "/dynamic-resolution", "+clipboard"]);
            if password {
                c.arg("/from-stdin:force");
            }
        }
        "remmina" => {
            c.arg("-c").arg(file);
        }
        _ => {
            c.arg(file);
        }
    }
    c
}

/// Opens the Remote Desktop program on the desktop at `addr`; `password`
/// goes to programs that want one ([`wants_password`]).
pub fn launch(file: &Path, addr: &str, user: &str, password: Option<&str>) -> Result<(Child, &'static str)> {
    let Some(client) = client() else {
        bail!("no Remote Desktop program found: install FreeRDP (xfreerdp3 or xfreerdp) or Remmina")
    };
    let pw = password.filter(|_| wants_password(client));
    let mut cmd = command(client, file, addr, user, pw.is_some());
    if pw.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().with_context(|| format!("could not start {client}"))?;
    if let (Some(pw), Some(mut stdin)) = (pw, child.stdin.take()) {
        use std::io::Write;
        let _ = writeln!(stdin, "{pw}");
    }
    Ok((child, client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdp_file_names_the_local_port_and_user() {
        let f = rdp_file("127.0.0.1:53211", " Administrator ");
        assert!(f.starts_with("full address:s:127.0.0.1:53211\r\n"), "{f}");
        assert!(f.contains("username:s:Administrator\r\n"));
        assert!(!rdp_file("winbox:3389", "").contains("username"));
    }

    #[test]
    fn the_rdp_file_is_rewritten_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_rdp_file(dir.path(), "win app/1", "127.0.0.1:1000", "ann").unwrap();
        let b = write_rdp_file(dir.path(), "win app/1", "127.0.0.1:2000", "ann").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.file_name().unwrap(), "win_app_1.rdp");
        assert!(std::fs::read_to_string(&b).unwrap().contains("127.0.0.1:2000"));
    }

    #[test]
    fn rdp_hello_is_a_connection_request() {
        let h = hello();
        assert_eq!(h.len(), 19);
        assert_eq!(&h[..4], &[0x03, 0x00, 0x00, 0x13]);
        assert_eq!((h[4], h[5]), (14, 0xe0));
        assert!(is_rdp_answer(&[0x03, 0, 0, 0x13, 0x0e, 0xd0, 0, 0, 0x12, 0x34, 0]));
        assert!(!is_rdp_answer(b"SSH-2.0-OpenSSH"));
    }

    #[test]
    fn freerdp_gets_the_address_and_user() {
        let args = |c: Command| c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let a = args(command("xfreerdp3", Path::new("/tmp/x.rdp"), "127.0.0.1:4000", "rdpuser", true));
        assert_eq!(a[..3], ["/v:127.0.0.1:4000", "/u:rdpuser", "/d:"]);
        assert!(a.contains(&"/from-stdin:force".to_string()));
        assert!(!a.iter().any(|x| x.contains("pass")), "the password is not an argument");
        let a = args(command("xfreerdp3", Path::new("/tmp/x.rdp"), "127.0.0.1:4000", "CORP\\ann", false));
        assert!(!a.contains(&"/d:".to_string()) && !a.contains(&"/from-stdin:force".to_string()));
        let c = command("mstsc", Path::new("C:/t/x.rdp"), "127.0.0.1:4000", "rdpuser", false);
        assert_eq!(c.get_args().count(), 1, "mstsc takes the file");
    }
}
