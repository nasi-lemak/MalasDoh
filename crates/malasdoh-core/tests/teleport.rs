//! Against a real Teleport cluster, when `MALASDOH_TELEPORT` names one:
//! `address user password totp-secret` (a local password + authenticator
//! user; the secret in base32, as in the authenticator QR code). Skipped
//! otherwise. Logs in, lists servers, connects through the proxy, runs a
//! command and uses SFTP, with host keys trusted only through the CA.

use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::secrets::MemoryStore;
use malasdoh_core::ssh::{self, ConnectContext, HostKeyPrompt, KbdPrompt, Prompter, SecretAnswer, SecretPrompt};
use malasdoh_core::teleport::{self, Cluster, TeleportError, TeleportStore};

/// Refuses every question: a host key prompt means the CA was not trusted.
#[derive(Default)]
struct Refuse(Mutex<Vec<String>>);

impl Prompter for Refuse {
    async fn confirm_host_key(&self, p: HostKeyPrompt) -> bool {
        self.0.lock().unwrap().push(format!("host key for {} ({:?})", p.host, p.warning));
        false
    }
    async fn ask_secret(&self, _: SecretPrompt) -> Option<SecretAnswer> {
        self.0.lock().unwrap().push("secret".into());
        None
    }
    async fn keyboard_interactive(&self, _: KbdPrompt) -> Option<Vec<String>> {
        None
    }
}

fn base32(s: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut bits, mut n, mut out) = (0u64, 0, Vec::new());
    for c in s.trim_end_matches('=').bytes() {
        bits = (bits << 5) | alphabet.iter().position(|a| *a == c.to_ascii_uppercase()).unwrap() as u64;
        n += 5;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    out
}

fn totp(secret: &str, step: u64) -> String {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&base32(secret)).unwrap();
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 15) as usize;
    let v = u32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]) & 0x7fff_ffff;
    format!("{:06}", v % 1_000_000)
}

fn step() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / 30
}

/// A code from a time step not used yet (Teleport takes each code once).
async fn fresh_code(secret: &str, used: &mut Option<u64>) -> String {
    while *used == Some(step()) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    *used = Some(step());
    totp(secret, step())
}

#[tokio::test]
async fn log_in_list_and_connect() {
    let Ok(spec) = std::env::var("MALASDOH_TELEPORT") else {
        eprintln!("skipped: set MALASDOH_TELEPORT=\"address user password totp-secret\"");
        return;
    };
    let [address, user, password, secret]: [&str; 4] =
        spec.split_whitespace().collect::<Vec<_>>().try_into().expect("four fields");
    let dir = tempfile::tempdir().unwrap();
    let tdir = dir.path().join("teleport");

    // A self-signed proxy is refused until its certificate is pinned.
    let pin = match teleport::ping(address, None).await {
        Ok(_) => None,
        Err(e) => match e.downcast_ref::<TeleportError>() {
            Some(TeleportError::UntrustedCert { fingerprint, .. }) => Some(fingerprint.clone()),
            _ => panic!("{e:#}"),
        },
    };
    let info = teleport::ping(address, pin.as_deref()).await.unwrap();
    assert!(info.unsupported().is_none(), "{info:?}");
    let mut cluster = Cluster {
        address: address.into(),
        user: user.into(),
        name: info.name.clone(),
        tls_routing: info.tls_routing,
        ssh_addr: info.ssh_addr.clone(),
        pinned_cert: pin,
        ..Default::default()
    };

    // Wrong code: a clear refusal, nothing stored. (Start in a fresh time
    // step: a previous run may have used this one's code.)
    let mut used = Some(step());
    let wrong = teleport::log_in(&mut cluster, &tdir, password, "000000").await.unwrap_err();
    assert!(format!("{wrong:#}").contains("not accepted"), "{wrong:#}");
    let key = cluster.key_path(&tdir);
    assert!(teleport::cert_state(&key).is_none());

    let code = fresh_code(secret, &mut used).await;
    let state = teleport::log_in(&mut cluster, &tdir, password, &code).await.unwrap();
    assert!(state.is_valid() && !state.logins.is_empty(), "{state:?}");
    assert!(!cluster.host_cas.is_empty());

    // Logging in again replaces the first login's files.
    let code = fresh_code(secret, &mut used).await;
    let state = teleport::log_in(&mut cluster, &tdir, password, &code).await.unwrap();
    assert!(state.is_valid());

    // Listing with the login itself: no second code.
    let nodes = malasdoh_core::teleport_api::list_nodes_with_login(&cluster, &tdir).await;
    let nodes = nodes.unwrap_or_else(|e| panic!("{e:#}"));
    assert!(!nodes.is_empty());
    assert!(nodes.iter().all(|n| !n.id.is_empty() && !n.hostname.is_empty()), "{nodes:?}");
    assert!(nodes.iter().any(|n| !n.logins.is_empty()), "each server's logins come too: {nodes:?}");
    // The same servers as the web API lists (which needs its own code).
    let code = fresh_code(secret, &mut used).await;
    let web = teleport::list_nodes(&cluster, password, &code).await.unwrap();
    let ids = |v: &[teleport::Node]| {
        let mut i: Vec<(String, String, Vec<String>)> = v
            .iter()
            .map(|n| {
                (n.id.clone(), n.hostname.clone(), {
                    let mut l = n.logins.clone();
                    l.sort();
                    l
                })
            })
            .collect();
        i.sort();
        i
    };
    assert_eq!(ids(&nodes), ids(&web));
    let node = &nodes[0];
    let login = node.logins.iter().find(|l| state.logins.contains(l)).cloned().unwrap_or(state.logins[0].clone());

    let mut proxy = teleport::proxy_profile(&cluster, &tdir, &state.logins[0], None).unwrap();
    proxy.id = malasdoh_core::profile::ProfileId::new();
    let target = teleport::node_profile(&cluster, &tdir, node, &login, proxy.id, None);
    cluster.hosts =
        vec![teleport::host_pattern(&proxy.host, proxy.port), teleport::host_pattern(&target.host, target.port)];
    let mut store = TeleportStore::load(dir.path().join("teleport.toml"));
    store.clusters = vec![cluster.clone()];
    let kh_file = tdir.join("known_hosts");
    store.write_known_hosts(&kh_file).unwrap();

    let prompter = Arc::new(Refuse::default());
    let known = KnownHosts::new(dir.path().join("app_known_hosts"), vec![kh_file]);
    let ctx = ConnectContext::new(known, Arc::new(MemoryStore::default()), prompter.clone());
    let conn = ssh::connect(vec![proxy.clone(), target.clone()], &ctx).await;
    let conn = conn.unwrap_or_else(|e| panic!("{e:#}; asked: {:?}", prompter.0.lock().unwrap()));
    let out = conn.exec("echo hello-from-$(hostname)", None).await.unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("hello-from-"), "{out:?}");
    let fs = conn.open_sftp().await.unwrap();
    assert!(!fs.home().await.unwrap().is_empty());

    // Windows desktops, when the cluster has any (MALASDOH_TELEPORT_DESKTOP
    // names one reachable from the server above): listed with the login,
    // and Remote Desktop through a port forwarded by that server.
    let desktops = malasdoh_core::teleport_api::list_desktops_with_login(&cluster, &tdir).await.unwrap();
    if let Ok(name) = std::env::var("MALASDOH_TELEPORT_DESKTOP") {
        let d = desktops.iter().find(|d| d.name == name).unwrap_or_else(|| panic!("{name} not in {desktops:?}"));
        assert!(!d.logins.is_empty(), "the Windows users come too: {d:?}");
        let p = teleport::desktop_profile(&cluster, d, &d.logins[0], None);
        assert!(p.rdp.is_some() && p.teleport == Some(cluster.id));
        let (host, port) = d.host_port();
        assert_eq!((p.host.as_str(), p.port), (host.as_str(), port));
        let conn = Arc::new(conn);
        let within = std::time::Duration::from_secs(8);
        malasdoh_core::rdp::probe(&conn, &host, port, within).await.unwrap();
        let e = malasdoh_core::rdp::probe(&conn, "127.0.0.1", 1, within).await.unwrap_err();
        assert!(format!("{e:#}").contains("nothing answers"), "{e:#}");
        // Its SSH port answers, but not as Remote Desktop.
        let e = malasdoh_core::rdp::probe(&conn, "127.0.0.1", 3022, within).await.unwrap_err();
        assert!(format!("{e:#}").contains("not as Remote Desktop"), "{e:#}");
        let spec =
            malasdoh_core::forward::ForwardSpec { target_host: host.clone(), target_port: port, ..Default::default() };
        let fwd = conn.start_forward(&spec).await.unwrap();
        // An RDP hello (X.224 connection request); the desktop answers with
        // a connection confirm (code 0xD0).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", fwd.port)).await.unwrap();
        tcp.write_all(&malasdoh_core::rdp::hello()).await.unwrap();
        let mut reply = [0u8; 11];
        tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read_exact(&mut reply)).await.unwrap().unwrap();
        assert!(malasdoh_core::rdp::is_rdp_answer(&reply), "{reply:02x?}");
        drop(fwd);
        conn.disconnect().await;
    } else {
        conn.disconnect().await;
    }
    assert!(prompter.0.lock().unwrap().is_empty(), "asked: {:?}", prompter.0.lock().unwrap());

    // Logged out: connecting says so before trying.
    teleport::log_out(&cluster, &tdir);
    let e = ssh::connect(vec![proxy, target], &ctx).await.err().expect("refused");
    assert!(e.downcast_ref::<teleport::LoginNeeded>().is_some(), "{e:#}");
}
