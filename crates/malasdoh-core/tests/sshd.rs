//! End-to-end tests against a real OpenSSH server.
//!
//! Each test starts its own `sshd` on a free localhost port with a throwaway
//! host key and authorized key, so nothing on the machine is touched. If
//! `sshd` is not installed the tests print a note and pass; CI installs it.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use malasdoh_core::edit::{Access, RemoteIo, RemoteState, SaveError, SaveOptions};
use malasdoh_core::known_hosts::KnownHosts;
use malasdoh_core::profile::{AuthMethod, Profile};
use malasdoh_core::secrets::{MemoryStore, SecretKey, SecretStore};
use malasdoh_core::sftp::{EntryKind, Progress};
use malasdoh_core::ssh::{
    self, ConnectContext, HostKeyError, HostKeyPrompt, KbdPrompt, Prompter, SecretAnswer, SecretPrompt, ShellEvent,
};
use malasdoh_core::validate::{CheckOutcome, CheckWhen, ValidationRule};

const PASSPHRASE: &str = "correct horse";

struct Sshd {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
    user: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_sshd() -> Option<PathBuf> {
    ["/usr/sbin/sshd", "/usr/local/sbin/sshd", "/opt/homebrew/sbin/sshd"].iter().map(PathBuf::from).find(|p| p.exists())
}

fn sftp_server() -> &'static str {
    ["/usr/lib/openssh/sftp-server", "/usr/libexec/openssh/sftp-server", "/usr/libexec/sftp-server"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("internal-sftp")
}

fn keygen(path: &Path, passphrase: &str) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", passphrase, "-C", "malasdoh-test", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok, "ssh-keygen failed");
}

fn start_sshd() -> Option<Sshd> {
    start_sshd_with(|_| String::new())
}

/// `setup` runs after the keys are made (it may sign certificates or change
/// authorized_keys) and returns extra sshd_config lines.
fn start_sshd_with(setup: impl FnOnce(&Path) -> String) -> Option<Sshd> {
    let Some(sshd) = find_sshd() else {
        eprintln!("sshd not installed; skipping end-to-end test");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    keygen(&d.join("host_ed25519"), "");
    keygen(&d.join("client"), PASSPHRASE);
    std::fs::copy(d.join("client.pub"), d.join("authorized_keys")).unwrap();
    let extra = setup(d);
    // "#nosftp" in the extra config: run without an SFTP subsystem.
    let subsystem = if extra.contains("#nosftp") { String::new() } else { format!("Subsystem sftp {}", sftp_server()) };
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::fs::write(
        d.join("sshd_config"),
        format!(
            "{extra}\nPort {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_ed25519\nPidFile {d}/sshd.pid\n\
             AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nAllowTcpForwarding yes\n\
             {subsystem}\nLogLevel ERROR\n",
            d = d.display()
        ),
    )
    .unwrap();
    // Running as root, sshd insists on its privilege-separation directory.
    let _ = std::fs::create_dir_all("/run/sshd");
    let mut child = Command::new(sshd)
        .args(["-D", "-e", "-f"])
        .arg(d.join("sshd_config"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("starting sshd");
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap().trim().to_string();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(Sshd { child, port, dir, user });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("sshd did not start");
}

#[derive(Default)]
struct TestPrompter {
    accept_host: bool,
    passphrase: Option<String>,
    remember: bool,
    asked: Mutex<Vec<String>>,
}

impl Prompter for TestPrompter {
    async fn confirm_host_key(&self, p: HostKeyPrompt) -> bool {
        assert!(p.fingerprint.starts_with("SHA256:"));
        self.asked.lock().unwrap().push("host".into());
        self.accept_host
    }

    async fn ask_secret(&self, p: SecretPrompt) -> Option<SecretAnswer> {
        self.asked.lock().unwrap().push(match p {
            SecretPrompt::Password { .. } => "password".into(),
            SecretPrompt::Passphrase { retry, .. } => format!("passphrase retry={retry}"),
            SecretPrompt::ProxyPassword { .. } => "proxy".into(),
        });
        self.passphrase.clone().map(|secret| SecretAnswer { secret, remember: self.remember })
    }

    async fn keyboard_interactive(&self, _: KbdPrompt) -> Option<Vec<String>> {
        None
    }
}

impl TestPrompter {
    fn accepting() -> Self {
        Self { accept_host: true, passphrase: Some(PASSPHRASE.into()), ..Default::default() }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

struct Env {
    sshd: Sshd,
    known_hosts: KnownHosts,
    secrets: Arc<MemoryStore>,
}

impl Env {
    fn new() -> Option<Self> {
        Self::with(start_sshd()?)
    }

    fn with(sshd: Sshd) -> Option<Self> {
        let known_hosts = KnownHosts::new(sshd.dir.path().join("app_known_hosts"), vec![]);
        Some(Self { sshd, known_hosts, secrets: Arc::new(MemoryStore::default()) })
    }

    fn profile(&self) -> Profile {
        Profile {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: self.sshd.port,
            user: self.sshd.user.clone(),
            auth: AuthMethod::Key { path: self.sshd.dir.path().join("client").display().to_string() },
            ..Default::default()
        }
    }

    fn ctx(&self, prompter: TestPrompter) -> ConnectContext<TestPrompter> {
        let mut ctx = ConnectContext::new(self.known_hosts.clone(), self.secrets.clone(), Arc::new(prompter));
        ctx.default_keys = vec![];
        ctx
    }

    async fn connect(&self) -> Arc<ssh::Connection> {
        Arc::new(ssh::connect(vec![self.profile()], &self.ctx(TestPrompter::accepting())).await.expect("connect"))
    }

    fn scratch(&self) -> String {
        let p = self.sshd.dir.path().join("work");
        std::fs::create_dir_all(&p).unwrap();
        p.display().to_string()
    }
}

#[tokio::test]
async fn host_key_is_asked_once_then_trusted() {
    let Some(env) = Env::new() else { return };

    let ctx = env.ctx(TestPrompter { accept_host: false, ..TestPrompter::accepting() });
    let err = ssh::connect(vec![env.profile()], &ctx).await.err().expect("declined host key must fail");
    assert!(matches!(err.downcast_ref::<HostKeyError>(), Some(HostKeyError::Declined { .. })), "{err:#}");

    let ctx = env.ctx(TestPrompter::accepting());
    ssh::connect(vec![env.profile()], &ctx).await.expect("accepted");
    assert_eq!(ctx.prompter.asked(), ["host", "passphrase retry=false"]);

    let ctx = env.ctx(TestPrompter::accepting());
    ssh::connect(vec![env.profile()], &ctx).await.expect("trusted");
    assert_eq!(ctx.prompter.asked(), ["passphrase retry=false"], "known host must not be asked about again");
}

#[tokio::test]
async fn changed_host_key_is_refused() {
    let Some(env) = Env::new() else { return };
    // Record a different key for this host, as if the server had been swapped.
    let other = env.sshd.dir.path().join("other");
    keygen(&other, "");
    let fake = std::fs::read_to_string(other.with_extension("pub")).unwrap();
    std::fs::write(env.known_hosts.app_file(), format!("[127.0.0.1]:{} {fake}", env.sshd.port)).unwrap();

    let ctx = env.ctx(TestPrompter::accepting());
    let err = ssh::connect(vec![env.profile()], &ctx).await.err().expect("must refuse");
    assert!(matches!(err.downcast_ref::<HostKeyError>(), Some(HostKeyError::Changed { .. })), "{err:#}");
    assert!(ctx.prompter.asked().is_empty(), "a changed key must never be offered for acceptance");
}

#[tokio::test]
async fn passphrase_is_retried_and_remembered() {
    let Some(env) = Env::new() else { return };
    let key_path = env.profile().auth;
    let AuthMethod::Key { path } = key_path else { unreachable!() };
    // A wrong stored passphrase is discarded, the user is asked, and the new one kept.
    env.secrets.set(&SecretKey::Passphrase(path.clone()), "wrong").unwrap();
    let ctx = env.ctx(TestPrompter { remember: true, ..TestPrompter::accepting() });
    ssh::connect(vec![env.profile()], &ctx).await.expect("connect");
    assert_eq!(ctx.prompter.asked(), ["host", "passphrase retry=true"]);
    assert_eq!(env.secrets.get(&SecretKey::Passphrase(path)).as_deref(), Some(PASSPHRASE));

    let ctx = env.ctx(TestPrompter::accepting());
    ssh::connect(vec![env.profile()], &ctx).await.expect("connect with remembered passphrase");
    assert!(ctx.prompter.asked().is_empty());
}

#[tokio::test]
async fn cancelling_the_passphrase_cancels_the_connection() {
    let Some(env) = Env::new() else { return };
    let ctx = env.ctx(TestPrompter { passphrase: None, ..TestPrompter::accepting() });
    let err = ssh::connect(vec![env.profile()], &ctx).await.err().expect("must fail");
    assert!(err.is::<ssh::Cancelled>(), "{err:#}");
}

#[tokio::test]
async fn exec_and_shell() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let out = conn.exec("echo hello; echo oops >&2; exit 7", None).await.unwrap();
    assert_eq!(out.stdout, b"hello\n");
    assert_eq!(out.stderr_text(), "oops");
    assert_eq!(out.status, Some(7));

    let out = conn.exec("cat", Some(b"piped")).await.unwrap();
    assert_eq!(out.stdout, b"piped");

    let mut shell = conn.open_shell(80, 24, Arc::new(|| {})).await.unwrap();
    shell.send(b"echo $((6*7))-marker; exit\n").await.unwrap();
    let mut seen = Vec::new();
    let exited = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = shell.events.recv().await {
            match ev {
                ShellEvent::Output(b) => seen.extend(b),
                ShellEvent::Exited(_) => return true,
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(exited);
    assert!(String::from_utf8_lossy(&seen).contains("42-marker"), "{}", String::from_utf8_lossy(&seen));
}

#[tokio::test]
async fn sftp_browse_and_transfer() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let work = env.scratch();

    // Upload a small tree, list it, download it back.
    let local = tempfile::tempdir().unwrap();
    let tree = local.path().join("site");
    std::fs::create_dir_all(tree.join("conf")).unwrap();
    std::fs::write(tree.join("index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(tree.join("conf/app.toml"), "port = 8080\n").unwrap();
    let progress = Progress::default();
    let remote = fs.upload(&tree, &work, &progress).await.unwrap();
    assert_eq!(progress.done_files.load(std::sync::atomic::Ordering::Relaxed), 2);

    let entries = fs.list(&remote).await.unwrap();
    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names, [("conf", EntryKind::Dir), ("index.html", EntryKind::File)], "dirs first");

    let back = tempfile::tempdir().unwrap();
    let progress = Progress::default();
    let dest = fs.download(&remote, back.path(), &progress).await.unwrap();
    assert_eq!(std::fs::read_to_string(dest.join("conf/app.toml")).unwrap(), "port = 8080\n");
    assert_eq!(
        progress.done_bytes.load(std::sync::atomic::Ordering::Relaxed),
        progress.total_bytes.load(std::sync::atomic::Ordering::Relaxed)
    );

    fs.rename(&format!("{remote}/index.html"), &format!("{remote}/home.html")).await.unwrap();
    fs.chmod(&format!("{remote}/home.html"), 0o600).await.unwrap();
    let e = fs.list(&remote).await.unwrap().into_iter().find(|e| e.name == "home.html").unwrap();
    assert_eq!(e.mode, Some(0o600));

    fs.remove(&remote).await.unwrap();
    assert!(!fs.exists(&remote).await.unwrap());
}

#[tokio::test]
async fn safe_edit_round_trip() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let work = env.scratch();
    let path = format!("{work}/app.conf");
    std::fs::write(&path, "listen 80\r\nworkers 2\r\n").unwrap();
    set_mode(&path, 0o640);

    let io = RemoteIo { fs: fs.clone(), conn: conn.clone(), access: Access::Sftp };
    let mut s = io.open(&path).await.unwrap();
    assert_eq!(s.base_text, "listen 80\nworkers 2\n", "CRLF hidden while editing");

    let backups = tempfile::tempdir().unwrap();
    let opts = SaveOptions { local_backup_dir: Some(backups.path().into()), remote_backup: true, ..Default::default() };
    let report = io.save(&mut s, "listen 8080\nworkers 2\n", &opts).await.unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"listen 8080\r\nworkers 2\r\n", "line endings preserved");
    assert_eq!(mode_of(&path), 0o640, "permissions preserved");
    assert_eq!(report.previous, "listen 80\nworkers 2\n", "what was replaced, for the journal");
    assert_eq!(std::fs::read(report.local_backup.unwrap()).unwrap(), b"listen 80\r\nworkers 2\r\n");
    let remote_backup = report.remote_backup.unwrap();
    assert!(remote_backup.starts_with(&format!("{path}.bak-")), "{remote_backup}");
    assert_eq!(std::fs::read(&remote_backup).unwrap(), b"listen 80\r\nworkers 2\r\n");
    assert!(!s.is_modified("listen 8080\nworkers 2\n"));

    // Someone else edits the file. Our next save must stop and show their version.
    std::fs::write(&path, "listen 9090\r\nworkers 2\r\n").unwrap();
    let opts = SaveOptions { remote_backup: false, ..opts };
    let err = io.save(&mut s, "listen 8080\nworkers 4\n", &opts).await.unwrap_err();
    match err.downcast_ref::<SaveError>() {
        Some(SaveError::Conflict { current }) => assert_eq!(current, b"listen 9090\r\nworkers 2\r\n"),
        other => panic!("expected conflict, got {other:?} / {err:#}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"listen 9090\r\nworkers 2\r\n", "conflicting save wrote nothing");

    // The user decides to overwrite anyway; the other person's version is backed up.
    let report =
        io.save(&mut s, "listen 8080\nworkers 4\n", &SaveOptions { force: true, ..opts.clone() }).await.unwrap();
    assert_eq!(std::fs::read(report.local_backup.unwrap()).unwrap(), b"listen 9090\r\nworkers 2\r\n");
    assert_eq!(report.previous, "listen 9090\nworkers 2\n", "a forced save records their version as replaced");
    assert_eq!(io.remote_state(&s).await.unwrap(), RemoteState::Unchanged);

    std::fs::remove_file(&path).unwrap();
    let err = io.save(&mut s, "x\n", &opts).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<SaveError>(), Some(SaveError::Deleted)), "{err:#}");
}

#[tokio::test]
async fn binary_files_are_not_opened_as_text() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let path = format!("{}/blob.bin", env.scratch());
    std::fs::write(&path, b"\x7fELF\0\0\0").unwrap();
    let io = RemoteIo { fs, conn, access: Access::Sftp };
    let err = io.open(&path).await.unwrap_err();
    assert!(format!("{err:#}").contains("binary"), "{err:#}");
}

#[tokio::test]
async fn sudo_edit() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    if RemoteIo::check_sudo(&conn, None).await.is_err() {
        eprintln!("passwordless sudo not available; skipping sudo test");
        return;
    }
    let fs = conn.open_sftp().await.unwrap();
    let path = format!("{}/root-owned.conf", env.scratch());
    std::fs::write(&path, "a=1\n").unwrap();
    set_mode(&path, 0o600);
    let io = RemoteIo { fs, conn, access: Access::Sudo { password: None } };
    let mut s = io.open(&path).await.unwrap();
    assert_eq!(s.base_text, "a=1\n");
    let opts = SaveOptions { remote_backup: true, ..Default::default() };
    let report = io.save(&mut s, "a=2\n", &opts).await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a=2\n");
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(std::fs::read_to_string(report.remote_backup.unwrap()).unwrap(), "a=1\n");
}

#[tokio::test]
async fn jump_host_chain() {
    let Some(env) = Env::new() else { return };
    // Jump through the same server to itself: exercises the direct-tcpip tunnel
    // and a second handshake over it.
    let bastion = Profile { name: "bastion".into(), ..env.profile() };
    let mut target = env.profile();
    target.jump = Some(bastion.id);
    let ctx = env.ctx(TestPrompter { remember: true, ..TestPrompter::accepting() });
    let conn = ssh::connect(vec![bastion, target], &ctx).await.expect("connect via jump host");
    assert_eq!(conn.hop_count(), 1);
    let out = conn.exec("echo through", None).await.unwrap();
    assert_eq!(out.stdout, b"through\n");
    // One host-key question (same host and port for both hops), one passphrase
    // (remembered after the first hop).
    assert_eq!(ctx.prompter.asked(), ["host", "passphrase retry=false"]);
}

/// A TCP echo server; returns its port.
async fn echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

async fn round_trip(mut s: tokio::net::TcpStream, msg: &[u8]) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    s.write_all(msg).await.unwrap();
    let mut buf = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut buf)).await.expect("timed out").unwrap();
    buf
}

#[tokio::test]
async fn port_forwards_local_dynamic_and_remote() {
    use malasdoh_core::forward::{ForwardKind, ForwardSpec};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let echo = echo_server().await;

    // -L: connect here, the server connects to the echo server.
    let local = ForwardSpec { kind: ForwardKind::Local, target_port: echo, ..Default::default() };
    let h = conn.start_forward(&local).await.unwrap();
    let s = tokio::net::TcpStream::connect(("127.0.0.1", h.port)).await.unwrap();
    assert_eq!(round_trip(s, b"through -L").await, b"through -L");

    // -D: a SOCKS5 CONNECT to the echo server by name.
    let socks = ForwardSpec { kind: ForwardKind::Dynamic, ..Default::default() };
    let d = conn.start_forward(&socks).await.unwrap();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", d.port)).await.unwrap();
    s.write_all(&[5, 1, 0]).await.unwrap();
    let mut r = [0u8; 2];
    s.read_exact(&mut r).await.unwrap();
    let mut req = vec![5, 1, 0, 3, 9];
    req.extend_from_slice(b"localhost");
    req.extend_from_slice(&echo.to_be_bytes());
    s.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    s.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[1], 0, "SOCKS reply: success");
    assert_eq!(round_trip(s, b"through -D").await, b"through -D");

    // -R: the server listens; connecting there reaches the echo server here.
    let remote = ForwardSpec { kind: ForwardKind::Remote, target_port: echo, ..Default::default() };
    let r = conn.start_forward(&remote).await.unwrap();
    assert_ne!(r.port, 0, "server picked a port");
    let s = tokio::net::TcpStream::connect(("127.0.0.1", r.port)).await.unwrap();
    assert_eq!(round_trip(s, b"through -R").await, b"through -R");
    assert!(r.stats.total.load(std::sync::atomic::Ordering::Relaxed) >= 1);

    // Stopping closes the listeners.
    let (lp, rp) = (h.port, r.port);
    conn.stop_forward(h).await;
    conn.stop_forward(r).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", lp)).await.is_err(), "-L listener stopped");
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", rp)).await.is_err(), "-R listener stopped on the server");
}

#[tokio::test]
async fn folder_sync_plans_then_mirrors() {
    use malasdoh_core::sync::{self, Action, Direction, SyncOptions};
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let checker = malasdoh_core::transfer::Checker::new(conn.clone(), None);
    let topts = malasdoh_core::transfer::TransferOptions::default();
    let remote = format!("{}/site", env.scratch());
    let local = tempfile::tempdir().unwrap();
    let l = local.path();

    std::fs::create_dir_all(l.join("css")).unwrap();
    std::fs::write(l.join("index.html"), "v2").unwrap();
    std::fs::write(l.join("css/app.css"), "body{}").unwrap();
    std::fs::write(l.join("same.txt"), "same").unwrap();
    std::fs::create_dir_all(l.join(".git")).unwrap();
    std::fs::write(l.join(".git/HEAD"), "ref").unwrap();
    std::fs::create_dir_all(format!("{remote}/old")).unwrap();
    std::fs::write(format!("{remote}/index.html"), "v1").unwrap();
    std::fs::write(format!("{remote}/old/gone.txt"), "x").unwrap();
    std::fs::write(format!("{remote}/same.txt"), "same").unwrap();
    // Same contents and time on both sides: unchanged.
    let t = std::fs::metadata(l.join("same.txt")).unwrap().modified().unwrap();
    std::fs::File::options().write(true).open(format!("{remote}/same.txt")).unwrap().set_modified(t).unwrap();
    // index.html: same size, different bytes, written within the same second.
    // By size and time it looks unchanged; comparing contents finds it.
    let quick = sync::plan(&fs, l, &remote, &SyncOptions { compare_contents: false, ..Default::default() }, None)
        .await
        .unwrap();
    assert!(!quick.items.iter().any(|i| i.rel == "index.html"));

    let opts =
        SyncOptions { direction: Direction::Upload, delete_extra: true, compare_contents: true, ..Default::default() };
    let plan = sync::plan(&fs, l, &remote, &opts, Some(&checker)).await.unwrap();
    let mut got: Vec<(String, Action)> = plan.items.iter().map(|i| (i.rel.clone(), i.action.clone())).collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("css".into(), Action::MkdirRemote),
            ("css/app.css".into(), Action::Upload { size: 6 }),
            ("index.html".into(), Action::Upload { size: 2 }),
            ("old".into(), Action::DeleteRemote { dir: true }),
        ],
        "only the top of a deleted folder is listed; .git is excluded"
    );
    assert_eq!(plan.unchanged, 1);
    // Planning changes nothing.
    assert_eq!(std::fs::read_to_string(format!("{remote}/index.html")).unwrap(), "v1");

    let report = sync::execute(&fs, l, &remote, &plan, &Progress::default(), &topts, Some(&checker)).await.unwrap();
    assert_eq!(report.verified, 2, "copies are checked by checksum: {:?}", report.unverified);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(std::fs::read_to_string(format!("{remote}/index.html")).unwrap(), "v2");
    assert_eq!(std::fs::read_to_string(format!("{remote}/css/app.css")).unwrap(), "body{}");
    assert!(!Path::new(&format!("{remote}/old")).exists());
    assert!(!Path::new(&format!("{remote}/.git")).exists());

    // Times were copied, so a second comparison finds nothing to do.
    let again = sync::plan(&fs, l, &remote, &opts, None).await.unwrap();
    assert!(again.items.is_empty(), "{:?}", again.items);

    // And back the other way into an empty folder, honouring unticked items.
    let back = tempfile::tempdir().unwrap();
    let down = SyncOptions { direction: Direction::Download, ..Default::default() };
    let mut plan = sync::plan(&fs, back.path(), &remote, &down, None).await.unwrap();
    plan.items.iter_mut().filter(|i| i.rel == "same.txt").for_each(|i| i.enabled = false);
    sync::execute(&fs, back.path(), &remote, &plan, &Progress::default(), &topts, None).await.unwrap();
    assert_eq!(std::fs::read_to_string(back.path().join("css/app.css")).unwrap(), "body{}");
    assert!(!back.path().join("same.txt").exists(), "unticked item was skipped");
}

/// `ssh-keygen -s ca -I id [-h] -n principals key.pub`
fn sign(dir: &Path, key_pub: &str, host: bool, principals: &str) {
    let mut c = Command::new("ssh-keygen");
    c.current_dir(dir).args(["-q", "-s", "ca", "-I", "test"]);
    if host {
        c.arg("-h");
    }
    let ok = c.args(["-n", principals, key_pub]).status().unwrap().success();
    assert!(ok, "ssh-keygen -s failed");
}

#[tokio::test]
async fn host_certificate_from_trusted_ca_needs_no_prompt() {
    let Some(sshd) = start_sshd_with(|d| {
        keygen(&d.join("ca"), "");
        sign(d, "host_ed25519.pub", true, "127.0.0.1");
        format!("HostCertificate {}/host_ed25519-cert.pub", d.display())
    }) else {
        return;
    };
    let ca = std::fs::read_to_string(sshd.dir.path().join("ca.pub")).unwrap();
    let env = Env::with(sshd).unwrap();
    std::fs::write(env.known_hosts.app_file(), format!("@cert-authority * {ca}")).unwrap();

    // Declining every prompt still connects: the certificate is enough.
    let ctx = env.ctx(TestPrompter { accept_host: false, ..TestPrompter::accepting() });
    ssh::connect(vec![env.profile()], &ctx).await.expect("trusted host certificate");
    assert!(!ctx.prompter.asked().contains(&"host".to_string()), "no host-key question: {:?}", ctx.prompter.asked());

    // A CA trusted only for other hosts does not count; we fall back to asking.
    std::fs::write(env.known_hosts.app_file(), format!("@cert-authority *.example.com {ca}")).unwrap();
    let ctx = env.ctx(TestPrompter { accept_host: false, ..TestPrompter::accepting() });
    let err = ssh::connect(vec![env.profile()], &ctx).await.err().expect("must ask, and we declined");
    assert!(matches!(err.downcast_ref::<HostKeyError>(), Some(HostKeyError::Declined { .. })), "{err:#}");
}

#[tokio::test]
async fn host_certificate_for_another_name_is_not_trusted() {
    let Some(sshd) = start_sshd_with(|d| {
        keygen(&d.join("ca"), "");
        sign(d, "host_ed25519.pub", true, "other.example.com");
        format!("HostCertificate {}/host_ed25519-cert.pub", d.display())
    }) else {
        return;
    };
    let ca = std::fs::read_to_string(sshd.dir.path().join("ca.pub")).unwrap();
    let env = Env::with(sshd).unwrap();
    std::fs::write(env.known_hosts.app_file(), format!("@cert-authority * {ca}")).unwrap();
    let warning = Arc::new(Mutex::new(None));
    struct P(Arc<Mutex<Option<String>>>);
    impl Prompter for P {
        async fn confirm_host_key(&self, p: HostKeyPrompt) -> bool {
            *self.0.lock().unwrap() = p.warning;
            false
        }
        async fn ask_secret(&self, _: SecretPrompt) -> Option<SecretAnswer> {
            Some(SecretAnswer { secret: PASSPHRASE.into(), remember: false })
        }
        async fn keyboard_interactive(&self, _: KbdPrompt) -> Option<Vec<String>> {
            None
        }
    }
    let ctx = ConnectContext::new(env.known_hosts.clone(), env.secrets.clone(), Arc::new(P(warning.clone())));
    assert!(ssh::connect(vec![env.profile()], &ctx).await.is_err());
    let w = warning.lock().unwrap().clone().expect("the prompt explains the refused certificate");
    assert!(w.contains("other.example.com"), "{w}");
}

#[tokio::test]
async fn user_certificate_logs_in_without_authorized_keys() {
    let Some(sshd) = start_sshd_with(|d| {
        keygen(&d.join("ca"), "");
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        sign(d, "client.pub", false, user.trim());
        // Only the certificate can work: the plain key is no longer authorized.
        std::fs::write(d.join("authorized_keys"), "").unwrap();
        format!("TrustedUserCAKeys {}/ca.pub", d.display())
    }) else {
        return;
    };
    let env = Env::with(sshd).unwrap();
    let conn = env.connect().await;
    assert_eq!(conn.exec("echo cert-ok", None).await.unwrap().stdout, b"cert-ok\n");
}

#[tokio::test]
async fn without_sftp_files_work_through_the_shell() {
    use malasdoh_core::sync::{self, SyncOptions};
    let Some(sshd) = start_sshd_with(|_| "#nosftp".into()) else { return };
    let env = Env::with(sshd).unwrap();
    let conn = env.connect().await;
    assert!(conn.open_sftp().await.is_err(), "the server really has no SFTP");
    let fs = conn.open_fs().await.unwrap();
    assert!(fs.is_shell());
    let work = env.scratch();

    // Awkward names survive the listing.
    for name in ["plain.txt", "with space", "it's", "new\nline", ".hidden", "-dash"] {
        std::fs::write(format!("{work}/{name}"), name).unwrap();
    }
    std::fs::create_dir(format!("{work}/sub")).unwrap();
    std::os::unix::fs::symlink(format!("{work}/sub"), format!("{work}/link-to-sub")).unwrap();
    let listed = fs.list(&work).await.unwrap();
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    for name in ["plain.txt", "with space", "it's", "new\nline", ".hidden", "-dash", "sub", "link-to-sub"] {
        assert!(names.contains(&name), "{name:?} missing from {names:?}");
    }
    assert_eq!(listed.iter().find(|e| e.name == "sub").unwrap().kind, EntryKind::Dir);
    assert_eq!(listed.iter().find(|e| e.name == "link-to-sub").unwrap().kind, EntryKind::Symlink { to_dir: true });
    assert_eq!(listed.iter().find(|e| e.name == "it's").unwrap().size, 4);

    assert_eq!(fs.canonicalize(&format!("{work}/sub/../plain.txt")).await.unwrap(), format!("{work}/plain.txt"));
    assert_eq!(fs.read(&format!("{work}/it's"), 1024).await.unwrap(), b"it's");

    // Binary-safe round trip, create-only, rename refusing to clobber, chmod, times.
    let blob: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    fs.create_new(&format!("{work}/blob"), &blob, Some(0o600)).await.unwrap();
    assert_eq!(std::fs::read(format!("{work}/blob")).unwrap(), blob);
    assert_eq!(mode_of(&format!("{work}/blob")), 0o600);
    assert!(fs.create_new(&format!("{work}/blob"), b"x", None).await.is_err(), "create_new must not overwrite");
    assert!(fs.rename(&format!("{work}/plain.txt"), &format!("{work}/blob")).await.is_err());
    fs.chmod(&format!("{work}/blob"), 0o640).await.unwrap();
    assert_eq!(mode_of(&format!("{work}/blob")), 0o640);
    fs.set_mtime(&format!("{work}/blob"), 1_600_000_000).await.unwrap();
    assert_eq!(fs.stat(&format!("{work}/blob")).await.unwrap().mtime, Some(1_600_000_000));

    // The safe editor works the same way.
    let io = RemoteIo { fs: fs.clone(), conn: conn.clone(), access: Access::Sftp };
    let path = format!("{work}/app.conf");
    std::fs::write(&path, "a=1\r\n").unwrap();
    set_mode(&path, 0o640);
    let mut s = io.open(&path).await.unwrap();
    io.save(&mut s, "a=2\n", &SaveOptions::default()).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a=2\r\n");
    assert_eq!(mode_of(&path), 0o640, "rewritten in place");
    std::fs::write(&path, "changed\r\n").unwrap();
    assert!(matches!(
        io.save(&mut s, "a=3\n", &SaveOptions::default()).await.unwrap_err().downcast_ref::<SaveError>(),
        Some(SaveError::Conflict { .. })
    ));

    // Transfers and sync.
    let local = tempfile::tempdir().unwrap();
    let tree = local.path().join("site");
    std::fs::create_dir_all(tree.join("css")).unwrap();
    std::fs::write(tree.join("css/a.css"), "x{}").unwrap();
    // The transfer queue's engine works here too (no continuing, no temp
    // names), and still checks copies by checksum.
    use malasdoh_core::transfer::{self, Checker, Ended, TransferOptions};
    let (topts, checker) = (TransferOptions::default(), Checker::new(conn.clone(), None));
    let mut job = transfer::plan_upload(std::slice::from_ref(&tree), &work, &topts).unwrap();
    let ended = transfer::run(&fs, &mut job, &topts, &Progress::default(), topts.limit(), Some(&checker)).await;
    assert_eq!(ended.unwrap(), Ended::Finished);
    assert!(job.is_finished() && job.verified() == 1, "{:?} {:?}", job.failures(), job.unverified);
    let remote_site = format!("{work}/site");
    assert_eq!(std::fs::read_to_string(format!("{remote_site}/css/a.css")).unwrap(), "x{}");
    let plan = sync::plan(&fs, &tree, &remote_site, &SyncOptions::default(), Some(&checker)).await.unwrap();
    assert!(plan.items.is_empty(), "{:?}", plan.items);
    let back = tempfile::tempdir().unwrap();
    let mut job = transfer::plan_download(&fs, std::slice::from_ref(&remote_site), back.path(), &topts).await.unwrap();
    transfer::run(&fs, &mut job, &topts, &Progress::default(), topts.limit(), Some(&checker)).await.unwrap();
    assert!(job.is_finished(), "{:?}", job.failures());
    let got = back.path().join("site");
    assert_eq!(std::fs::read_to_string(got.join("css/a.css")).unwrap(), "x{}");
    let old = fs.download(&remote_site, tempfile::tempdir().unwrap().path(), &Progress::default()).await;
    assert!(old.is_ok());

    fs.remove(&remote_site).await.unwrap();
    assert!(!fs.exists(&remote_site).await.unwrap());
    assert!(fs.remove("/").await.is_err());
}

fn rule(check: &str, when: CheckWhen, reload: &str) -> ValidationRule {
    ValidationRule {
        name: "test".into(),
        pattern: "*".into(),
        check: check.into(),
        when,
        reload: reload.into(),
        enabled: true,
    }
}

#[tokio::test]
async fn validation_blocks_or_rolls_back_bad_configs() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let work = env.scratch();
    let io = RemoteIo { fs: fs.clone(), conn: conn.clone(), access: Access::Sftp };
    let save = |rule: ValidationRule| SaveOptions { validation: Some(rule), ..Default::default() };

    // A local syntax check: broken JSON is never written.
    let json = format!("{work}/app.json");
    std::fs::write(&json, "{\"a\": 1}\n").unwrap();
    let mut s = io.open(&json).await.unwrap();
    let err =
        io.save(&mut s, "{\"a\": 1,}\n", &save(rule("builtin:json", CheckWhen::BeforeWrite, ""))).await.unwrap_err();
    assert!(
        matches!(err.downcast_ref::<SaveError>(), Some(SaveError::ValidationFailed { rolled_back: false, .. })),
        "{err:#}"
    );
    assert_eq!(std::fs::read_to_string(&json).unwrap(), "{\"a\": 1}\n");

    // A remote check on a temporary copy: same, and the copy is cleaned up.
    let conf = format!("{work}/svc.conf");
    std::fs::write(&conf, "good = 1\n").unwrap();
    let mut s = io.open(&conf).await.unwrap();
    let before = rule("grep -q good {file}", CheckWhen::BeforeWrite, "");
    let err = io.save(&mut s, "bad = 1\n", &save(before.clone())).await.unwrap_err();
    assert!(
        matches!(err.downcast_ref::<SaveError>(), Some(SaveError::ValidationFailed { rolled_back: false, .. })),
        "{err:#}"
    );
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "good = 1\n");
    let home = fs.home().await.unwrap();
    assert!(
        !fs.list(&home).await.unwrap().iter().any(|e| e.name.starts_with(".malasdoh-check-")),
        "temp copy left behind"
    );

    // After-write check: the bad version is written, fails, and the original comes back.
    let after = rule("grep -q good {file}", CheckWhen::AfterWrite, &format!("touch {work}/reloaded"));
    let err = io.save(&mut s, "bad = 2\n", &save(after.clone())).await.unwrap_err();
    assert!(
        matches!(err.downcast_ref::<SaveError>(), Some(SaveError::ValidationFailed { rolled_back: true, .. })),
        "{err:#}"
    );
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "good = 1\n");
    assert!(!Path::new(&format!("{work}/reloaded")).exists(), "no reload after a failed check");
    assert_eq!(io.remote_state(&s).await.unwrap(), RemoteState::Unchanged, "editor base still matches the server");

    // A passing check runs the reload command.
    let report = io.save(&mut s, "good = 2\n", &save(after)).await.unwrap();
    let check = report.check.unwrap();
    assert!(matches!(check.outcome, CheckOutcome::Passed { .. }));
    assert_eq!(check.reload.map(|(ok, _)| ok), Some(true));
    assert!(Path::new(&format!("{work}/reloaded")).exists());

    // A check tool that is not installed does not block the save.
    let report = io
        .save(&mut s, "good = 3\n", &save(rule("no-such-checker-xyz {file}", CheckWhen::AfterWrite, "")))
        .await
        .unwrap();
    assert!(matches!(report.check.unwrap().outcome, CheckOutcome::Unavailable { .. }));
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "good = 3\n");
}

#[tokio::test]
async fn real_visudo_rejects_a_broken_sudoers_file() {
    let Some(env) = Env::new() else { return };
    if Command::new("visudo").arg("--version").output().is_err() {
        eprintln!("visudo not installed; skipping");
        return;
    }
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let path = format!("{}/90-ops", env.scratch());
    std::fs::write(&path, "root ALL=(ALL) ALL\n").unwrap();
    let io = RemoteIo { fs, conn, access: Access::Sftp };
    let mut s = io.open(&path).await.unwrap();
    let sudoers = malasdoh_core::validate::builtin_rules().into_iter().find(|r| r.name == "sudoers").unwrap();
    let opts = SaveOptions { validation: Some(sudoers), ..Default::default() };
    let err = io.save(&mut s, "root ALL=(ALL ALL\n", &opts).await.unwrap_err();
    match err.downcast_ref::<SaveError>() {
        Some(SaveError::ValidationFailed { output, rolled_back: false, .. }) => {
            assert!(output.contains("90-ops"), "temp path is shown as the real path: {output}")
        }
        other => panic!("{other:?} / {err:#}"),
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "root ALL=(ALL) ALL\n");
    io.save(&mut s, "root ALL=(ALL) NOPASSWD: ALL\n", &opts).await.unwrap();
}

#[cfg(unix)]
fn set_mode(path: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode_of(path: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[tokio::test]
async fn generated_key_is_installed_then_logs_in() {
    use malasdoh_core::keys::{self, Installed};
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("id_new");
    let line = keys::generate(&key, "test@malasdoh", "").unwrap();

    // The test sshd reads keys from its own folder, never ~/.ssh.
    let file = env.sshd.dir.path().join("authorized_keys");
    // A file without a final newline must not get the key glued on.
    let mut existing = std::fs::read(&file).unwrap();
    while existing.last() == Some(&b'\n') {
        existing.pop();
    }
    std::fs::write(&file, &existing).unwrap();
    let target = file.display().to_string();
    assert_eq!(keys::install(&conn, &line, Some(&target)).await.unwrap(), Installed::Added);
    assert_eq!(keys::install(&conn, &line, Some(&target)).await.unwrap(), Installed::AlreadyThere);
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().filter(|l| l.contains("test@malasdoh")).count(), 1);
    assert!(text.ends_with(&format!("\n{line}\n")), "{text:?}");

    let profile = Profile { auth: AuthMethod::Key { path: key.display().to_string() }, ..env.profile() };
    let conn2 = ssh::connect(vec![profile], &env.ctx(TestPrompter::accepting())).await.unwrap();
    assert_eq!(conn2.exec("echo new-key-ok", None).await.unwrap().stdout, b"new-key-ok\n");
}

#[tokio::test]
async fn file_commands_links_owner_archives_find_and_follow() {
    use malasdoh_core::commands::{self, Archive};
    use malasdoh_core::ssh::StreamEvent;
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let work = format!("{}/ops", env.scratch());
    std::fs::create_dir_all(format!("{work}/sub")).unwrap();
    std::fs::write(format!("{work}/a.conf"), "listen 80;\n").unwrap();
    std::fs::write(format!("{work}/sub/b.conf"), "server_name shop;\n").unwrap();
    std::fs::write(format!("{work}/c.txt"), "plain\n").unwrap();
    let run = |cmd: String| {
        let conn = conn.clone();
        async move { conn.exec(&cmd, None).await.unwrap() }
    };

    // Symlinks: made once, never replacing anything.
    let link = format!("{work}/current.conf");
    assert!(run(commands::symlink("a.conf", &link).unwrap()).await.success());
    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("a.conf"));
    assert!(!run(commands::symlink("c.txt", &link).unwrap()).await.success());
    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("a.conf"));

    // Owner: to ourselves, which needs no privilege.
    let me = String::from_utf8(run("id -un".into()).await.stdout).unwrap().trim().to_string();
    let out = run(commands::chown(&[format!("{work}/c.txt")], &me, false).unwrap()).await;
    assert!(out.success(), "{}", out.stderr_text());

    // Find by name and by contents (before any archive is unpacked).
    let mut by_name: Vec<String> =
        String::from_utf8(run(commands::find(&work, "*.CONF", "", None, 50).unwrap()).await.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
    by_name.sort();
    assert_eq!(by_name, [format!("{work}/a.conf"), format!("{work}/current.conf"), format!("{work}/sub/b.conf")]);
    let by_text = run(commands::find(&work, "", "SERVER_NAME", None, 50).unwrap()).await;
    assert_eq!(String::from_utf8(by_text.stdout).unwrap().trim(), format!("{work}/sub/b.conf"));

    // Pack, refuse to replace the archive, unpack into a new folder only.
    let names = vec!["a.conf".to_string(), "sub".to_string()];
    let out = run(commands::create(&work, &names, "pack.tar.gz", Archive::TarGz).unwrap()).await;
    assert!(out.success(), "{}", out.stderr_text());
    let again = run(commands::create(&work, &names, "pack.tar.gz", Archive::TarGz).unwrap()).await;
    assert!(!again.success() && again.stderr_text().contains("already exists"));
    let (cmd, dest) = commands::extract(&format!("{work}/pack.tar.gz")).unwrap();
    assert!(run(cmd.clone()).await.success());
    assert_eq!(std::fs::read_to_string(format!("{dest}/sub/b.conf")).unwrap(), "server_name shop;\n");
    std::fs::write(format!("{dest}/a.conf"), "edited\n").unwrap();
    assert!(!run(cmd).await.success(), "the folder exists now");
    assert_eq!(std::fs::read_to_string(format!("{dest}/a.conf")).unwrap(), "edited\n", "nothing overwritten");

    // A single .gz: refused while the plain file exists, then restored.
    assert!(run(format!("cd '{work}' && gzip -k c.txt")).await.success());
    let (cmd, dest) = commands::extract(&format!("{work}/c.txt.gz")).unwrap();
    let out = run(cmd.clone()).await;
    assert!(!out.success() && out.stderr_text().contains("already exists"), "{}", out.stderr_text());
    std::fs::write(&dest, "changed\n").unwrap();
    assert!(!run(cmd.clone()).await.success());
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "changed\n");
    std::fs::remove_file(&dest).unwrap();
    assert!(run(cmd).await.success());
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "plain\n");

    if Command::new("zip").arg("-v").output().is_ok() && Command::new("unzip").arg("-v").output().is_ok() {
        let out = run(commands::create(&work, &names, "bundle.zip", Archive::Zip).unwrap()).await;
        assert!(out.success(), "{}", out.stderr_text());
        let (cmd, dest) = commands::extract(&format!("{work}/bundle.zip")).unwrap();
        assert!(run(cmd).await.success());
        assert!(Path::new(&format!("{dest}/sub/b.conf")).exists());
    }

    // Follow a log: the last lines, then new ones; dropping the stream stops tail.
    let log = format!("{work}/app.log");
    std::fs::write(&log, "old 1\nold 2\n").unwrap();
    let mut stream = conn.exec_stream(&commands::follow(&log, 1), None).await.unwrap();
    let mut seen = String::new();
    let mut wait_for = async |want: &str, seen: &mut String| {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !seen.contains(want) {
                match stream.events.recv().await {
                    Some(StreamEvent::Stdout(b)) => seen.push_str(&String::from_utf8_lossy(&b)),
                    Some(StreamEvent::Exited(s)) => panic!("tail ended: {s:?}"),
                    Some(_) => {}
                    None => panic!("stream closed"),
                }
            }
        })
        .await
        .expect("output in time");
    };
    wait_for("old 2", &mut seen).await;
    assert!(!seen.contains("old 1"), "only the last line: {seen:?}");
    std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"new 3\n").unwrap();
    wait_for("new 3", &mut seen).await;
    drop(stream);
    let gone = async {
        for _ in 0..50 {
            let out = Command::new("pgrep").args(["-f", &format!("tail -n 1 -F -- {log}")]).output().unwrap();
            if out.stdout.is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    assert!(gone.await, "tail was stopped on the server");
}

/// Run `script` in an interactive shell and return what it printed.
async fn shell_output(conn: &ssh::Connection, script: &str) -> String {
    let mut shell = conn.open_shell(200, 24, Arc::new(|| {})).await.unwrap();
    shell.send(format!("{script}; exit\n").as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = shell.events.recv().await {
            match ev {
                ShellEvent::Output(b) => seen.extend(b),
                ShellEvent::Exited(_) => break,
            }
        }
    })
    .await
    .unwrap();
    String::from_utf8_lossy(&seen).into_owned()
}

#[tokio::test]
async fn agent_forwarding_only_when_asked() {
    let Some(env) = Env::new() else { return };
    if Command::new("ssh-agent").arg("-h").output().is_err() {
        eprintln!("ssh-agent not installed; skipping");
        return;
    }
    // A private agent holding a fresh key, on a socket of our own. The guard
    // stops it even if an assertion fails.
    struct Agent(std::process::Child);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let _agent = Agent(
        Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let key = dir.path().join("id_agent");
    let line = malasdoh_core::keys::generate(&key, "agent-test", "").unwrap();
    let added = Command::new("ssh-add").arg(&key).env("SSH_AUTH_SOCK", &sock).stdin(Stdio::null()).output().unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let fp = line.split_whitespace().nth(1).unwrap().to_string();

    let mut ctx = env.ctx(TestPrompter::accepting());
    ctx.agent_socket = Some(sock.clone());
    let on = Profile { forward_agent: true, ..env.profile() };
    let conn = ssh::connect(vec![on], &ctx).await.unwrap();
    let out = shell_output(&conn, "ssh-add -L").await;
    assert!(out.contains(&fp), "the forwarded agent lists the key: {out}");

    let off = env.profile();
    let conn = ssh::connect(vec![off], &ctx).await.unwrap();
    let out = shell_output(&conn, "echo agent=[$SSH_AUTH_SOCK]").await;
    assert!(out.contains("agent=[]"), "no agent unless asked: {out}");
}

/// A minimal HTTP CONNECT proxy for the test; counts the tunnels it made.
async fn http_proxy() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let c = c.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if s.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let line = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string();
                let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let Ok(mut up) = tokio::net::TcpStream::connect(&target).await else {
                    let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
                let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
            });
        }
    });
    (port, count)
}

#[tokio::test]
async fn connects_through_socks_http_and_command_proxies() {
    use malasdoh_core::forward::{ForwardKind, ForwardSpec};
    use malasdoh_core::proxy::Proxy;
    let Some(env) = Env::new() else { return };
    let ctx = env.ctx(TestPrompter::accepting());
    let check = async |p: Profile| {
        let conn = ssh::connect(vec![p], &ctx).await.expect("connect through the proxy");
        assert_eq!(conn.exec("echo via-proxy", None).await.unwrap().stdout, b"via-proxy\n");
    };

    // SOCKS5: our own dynamic forward on another connection is the proxy.
    let first = env.connect().await;
    let d = first.start_forward(&ForwardSpec { kind: ForwardKind::Dynamic, ..Default::default() }).await.unwrap();
    let socks = Proxy::Socks5 { host: "127.0.0.1".into(), port: d.port, user: String::new() };
    check(Profile { proxy: socks, host: "localhost".into(), ..env.profile() }).await;

    // HTTP CONNECT.
    let (port, tunnels) = http_proxy().await;
    let http = Proxy::Http { host: "127.0.0.1".into(), port, user: String::new() };
    check(Profile { proxy: http, ..env.profile() }).await;
    assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 1, "the connection went through the proxy");

    // ProxyCommand, OpenSSH style.
    if Command::new("nc").arg("-h").output().is_ok() {
        let cmd = Proxy::Command { command: "nc %h %p".into() };
        check(Profile { proxy: cmd, ..env.profile() }).await;
    }

    // A proxy that is not there fails clearly.
    let dead = Proxy::Http { host: "127.0.0.1".into(), port: 1, user: String::new() };
    let err = ssh::connect(vec![Profile { proxy: dead, ..env.profile() }], &ctx).await.err().expect("fails");
    assert!(format!("{err:#}").contains("HTTP proxy"), "{err:#}");
}

#[tokio::test]
async fn sftp_through_sudo_reaches_root_only_files() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    // sudo must work without a password here (the test runs as root, or
    // with NOPASSWD); otherwise there is nothing to check.
    if !conn.exec("sudo -n true", None).await.map(|o| o.success()).unwrap_or(false) {
        eprintln!("passwordless sudo not available; skipping");
        return;
    }
    let work = env.scratch();
    let secret = format!("{work}/root-only");
    conn.exec(
        &format!("sudo mkdir -m 700 {secret} && echo top > {secret}/key && sudo chown -R root:root {secret}"),
        None,
    )
    .await
    .unwrap();
    let fs = conn.open_sftp_sudo(None).await.expect("sftp through sudo");
    let names: Vec<String> = fs.list(&secret).await.unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["key"]);
    assert_eq!(fs.read(&format!("{secret}/key"), 100).await.unwrap(), b"top\n");
    // The id the files are handled as is root's.
    fs.create_new(&format!("{secret}/new"), b"x", Some(0o600)).await.unwrap();
    let uid = std::fs::metadata(format!("{secret}/new")).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap();
    assert_eq!(uid, 0);
}

#[tokio::test]
async fn create_new_through_sudo_never_replaces() {
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    if !conn.exec("sudo -n true", None).await.map(|o| o.success()).unwrap_or(false) {
        eprintln!("passwordless sudo not available; skipping");
        return;
    }
    let fs = conn.open_sftp().await.unwrap();
    let io = RemoteIo { fs, conn: conn.clone(), access: Access::Sudo { password: None } };
    let path = format!("{}/motd", env.scratch());
    io.create_new(&path, b"Managed by ops\n", 0o644).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"Managed by ops\n");
    assert_eq!(mode_of(&path), 0o644);
    assert!(io.create_new(&path, b"other\n", 0o644).await.is_err(), "an existing file is not replaced");
    assert_eq!(std::fs::read(&path).unwrap(), b"Managed by ops\n");
}

/// Pseudo-random bytes (so nothing compresses or dedupes by accident).
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Stop `progress` once `after` bytes have gone through: paused (partial
/// files kept) or cancelled.
fn stop_after(progress: Arc<Progress>, after: u64, pause: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    tokio::spawn(async move {
        while progress.done_bytes.load(Relaxed) < after {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        progress.pause.store(pause, Relaxed);
        progress.cancel.store(true, Relaxed);
    });
}

fn list_tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(base).unwrap().display().to_string();
            if e.file_type().unwrap().is_dir() {
                walk(base, &e.path(), out);
            }
            out.push(rel);
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn transfer_queue_parallel_pause_resume_verify() {
    use malasdoh_core::transfer::{self, Checker, Ended, TransferOptions};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering::Relaxed;
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let fs = conn.open_sftp().await.unwrap();
    let checker = Checker::new(conn.clone(), None);
    let work = env.scratch();
    const BIG: usize = 48 << 20;

    let local = tempfile::tempdir().unwrap();
    let tree = local.path().join("proj");
    for d in ["a", "b", "node_modules/pkg"] {
        std::fs::create_dir_all(tree.join(d)).unwrap();
    }
    for i in 0..40 {
        std::fs::write(tree.join(if i % 2 == 0 { "a" } else { "b" }).join(format!("f{i}.txt")), format!("file {i}\n"))
            .unwrap();
    }
    std::fs::write(tree.join("node_modules/pkg/index.js"), "x").unwrap();
    std::fs::write(tree.join("notes.tmp"), "scratch").unwrap();
    std::fs::write(tree.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(tree.join("run.sh"), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::write(tree.join("big.bin"), noise(BIG, 7)).unwrap();
    std::fs::write(tree.join("a/keep.conf"), "new\n").unwrap();
    // Already on the server, with its own permissions: written in place.
    std::fs::create_dir_all(format!("{work}/proj/a")).unwrap();
    std::fs::write(format!("{work}/proj/a/keep.conf"), "old contents\n").unwrap();
    std::fs::set_permissions(format!("{work}/proj/a/keep.conf"), std::fs::Permissions::from_mode(0o640)).unwrap();

    let opts = TransferOptions { parallel: 4, verify: true, skip: vec!["node_modules".into(), "*.tmp".into()] };
    let mut job = transfer::plan_upload(std::slice::from_ref(&tree), &work, &opts).unwrap();
    assert_eq!(job.skipped, ["proj/node_modules", "proj/notes.tmp"]);
    assert_eq!(job.files.len(), 43);

    // Pause part-way through the big file.
    let progress = Arc::new(Progress::default());
    stop_after(progress.clone(), 8 << 20, true);
    let ended = transfer::run(&fs, &mut job, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert_eq!(ended, Ended::Paused);
    assert!(!Path::new(&format!("{work}/proj/big.bin")).exists(), "nobody sees half a new file");
    let part = format!("{work}/proj/.big.bin.malasdoh-part");
    let had = std::fs::metadata(&part).expect("partial kept for resuming").len();
    assert!(had > 0 && (had as usize) < BIG);

    // Resume: the partial is continued, everything arrives and is checked.
    let progress = Progress::default();
    let ended = transfer::run(&fs, &mut job, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert_eq!(ended, Ended::Finished);
    assert!(job.is_finished(), "{:?}", job.failures());
    assert!(progress.resumed_bytes.load(Relaxed) > 0, "the partial upload was continued");
    assert_eq!(job.verified(), 43, "{:?}", job.unverified);
    assert_eq!(std::fs::read(format!("{work}/proj/big.bin")).unwrap(), std::fs::read(tree.join("big.bin")).unwrap());
    assert!(!Path::new(&part).exists());
    let server = list_tree(Path::new(&format!("{work}/proj")));
    assert!(
        !server.iter().any(|p| p.contains("node_modules") || p.ends_with(".tmp") || p.contains("malasdoh-part")),
        "{server:?}"
    );
    // In place: new contents, its own permissions. New files: the local ones.
    assert_eq!(std::fs::read_to_string(format!("{work}/proj/a/keep.conf")).unwrap(), "new\n");
    let mode = |p: &str| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&format!("{work}/proj/a/keep.conf")), 0o640);
    assert_eq!(mode(&format!("{work}/proj/run.sh")), 0o750);
    // Times kept.
    // To the second: SFTP carries whole seconds.
    let mtime = |p: &Path| {
        std::fs::metadata(p).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    };
    assert_eq!(mtime(Path::new(&format!("{work}/proj/b/f1.txt"))), mtime(&tree.join("b/f1.txt")));

    // Download it back; a cancel (not a pause) removes the partial.
    let back = tempfile::tempdir().unwrap();
    let remote = format!("{work}/proj");
    let mut down = transfer::plan_download(&fs, std::slice::from_ref(&remote), back.path(), &opts).await.unwrap();
    let progress = Arc::new(Progress::default());
    stop_after(progress.clone(), 8 << 20, false);
    let ended = transfer::run(&fs, &mut down, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert_eq!(ended, Ended::Cancelled);
    assert!(!back.path().join("proj/big.bin.malasdoh-part").exists(), "cancel cleans up");
    assert!(!back.path().join("proj/big.bin").exists());

    // Pause, then change the file on the server: the partial no longer fits
    // and the file starts over rather than being spliced.
    let progress = Arc::new(Progress::default());
    stop_after(progress.clone(), 8 << 20, true);
    let ended = transfer::run(&fs, &mut down, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert_eq!(ended, Ended::Paused);
    assert!(back.path().join("proj/big.bin.malasdoh-part").exists());
    let changed = noise(BIG, 99);
    std::fs::write(format!("{remote}/big.bin"), &changed).unwrap();
    let progress = Progress::default();
    let ended = transfer::run(&fs, &mut down, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert_eq!(ended, Ended::Finished);
    assert!(down.is_finished(), "{:?}", down.failures());
    assert_eq!(std::fs::read(back.path().join("proj/big.bin")).unwrap(), changed);
    assert_eq!(down.verified(), 43);
    assert_eq!(mtime(&back.path().join("proj/b/f1.txt")), mtime(Path::new(&format!("{remote}/b/f1.txt"))));
    assert!(!list_tree(back.path()).iter().any(|p| p.contains("malasdoh-part")));

    // Pausing a download and resuming it continues the partial.
    std::fs::remove_dir_all(back.path().join("proj")).unwrap();
    let mut again = transfer::plan_download(&fs, std::slice::from_ref(&remote), back.path(), &opts).await.unwrap();
    let progress = Arc::new(Progress::default());
    stop_after(progress.clone(), 8 << 20, true);
    transfer::run(&fs, &mut again, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    let progress = Progress::default();
    transfer::run(&fs, &mut again, &opts, &progress, opts.limit(), Some(&checker)).await.unwrap();
    assert!(again.is_finished() && progress.resumed_bytes.load(Relaxed) > 0);
    assert_eq!(std::fs::read(back.path().join("proj/big.bin")).unwrap(), changed);
}

#[tokio::test]
async fn server_checksums() {
    use malasdoh_core::transfer::{Checker, local_sha256};
    let Some(env) = Env::new() else { return };
    let conn = env.connect().await;
    let work = env.scratch();
    let names = ["plain", "with space", "it's", "new\nline", "-dash"];
    let mut paths = Vec::new();
    for n in names {
        let p = format!("{work}/{n}");
        std::fs::write(&p, n).unwrap();
        paths.push(p);
    }
    paths.push(format!("{work}/missing"));
    let got = Checker::new(conn, None).sha256(&paths).await.unwrap();
    assert_eq!(got.len(), 6);
    for (p, h) in paths.iter().zip(&got).take(5) {
        assert_eq!(h.as_deref(), Some(local_sha256(Path::new(p)).await.unwrap().as_str()), "{p:?}");
    }
    assert_eq!(got[5], None, "a missing file is reported, not fatal");
}

/// Timing, not run by default: `MALASDOH_DELAY_PROXY=delay.py cargo test
/// --test sshd transfer_speed -- --ignored --nocapture`, where the script is
/// a ProxyCommand adding latency (`script host port`).
#[tokio::test]
#[ignore]
async fn transfer_speed_with_latency() {
    use malasdoh_core::transfer::{self, TransferOptions};
    let Ok(script) = std::env::var("MALASDOH_DELAY_PROXY") else { return };
    let Some(env) = Env::new() else { return };
    let mut p = env.profile();
    p.proxy = malasdoh_core::proxy::Proxy::Command { command: format!("python3 {script} %h %p") };
    let conn = Arc::new(ssh::connect(vec![p], &env.ctx(TestPrompter::accepting())).await.unwrap());
    let fs = conn.open_sftp().await.unwrap();
    let local = tempfile::tempdir().unwrap();
    let tree = local.path().join("many");
    std::fs::create_dir_all(&tree).unwrap();
    for i in 0..60 {
        std::fs::write(tree.join(format!("f{i}.txt")), noise(4096, i)).unwrap();
    }
    std::fs::write(local.path().join("one.bin"), noise(4 << 20, 3)).unwrap();
    for (name, src) in [("60 x 4 KiB", tree.clone()), ("1 x 4 MiB", local.path().join("one.bin"))] {
        let dest = format!("{}/old-{}", env.scratch(), name.len());
        std::fs::create_dir_all(&dest).unwrap();
        let t = std::time::Instant::now();
        fs.upload(&src, &dest, &Progress::default()).await.unwrap();
        eprintln!("{name}: one at a time (before): {:.1?}", t.elapsed());
        for parallel in [1, 4, 8] {
            let dest = format!("{}/p{parallel}-{}", env.scratch(), name.len());
            std::fs::create_dir_all(&dest).unwrap();
            let opts = TransferOptions { parallel, verify: false, ..Default::default() };
            let mut job = transfer::plan_upload(std::slice::from_ref(&src), &dest, &opts).unwrap();
            let t = std::time::Instant::now();
            transfer::run(&fs, &mut job, &opts, &Progress::default(), opts.limit(), None).await.unwrap();
            assert!(job.is_finished());
            eprintln!("{name}: queue, {parallel} at once: {:.1?}", t.elapsed());
        }
    }
}

#[tokio::test]
async fn compressed_connection_works() {
    use malasdoh_core::transfer::{self, TransferOptions};
    let Some(env) = Env::new() else { return };
    let mut p = env.profile();
    p.compression = true;
    let conn = Arc::new(ssh::connect(vec![p], &env.ctx(TestPrompter::accepting())).await.expect("compressed"));
    let out = conn.exec("echo squeezed", None).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "squeezed");
    // A compressible file goes through intact.
    let fs = conn.open_sftp().await.unwrap();
    let local = tempfile::tempdir().unwrap();
    let f = local.path().join("log.txt");
    std::fs::write(&f, "GET /index.html 200\n".repeat(50_000)).unwrap();
    let opts = TransferOptions::default();
    let mut job = transfer::plan_upload(std::slice::from_ref(&f), &env.scratch(), &opts).unwrap();
    let checker = transfer::Checker::new(conn.clone(), None);
    transfer::run(&fs, &mut job, &opts, &Progress::default(), opts.limit(), Some(&checker)).await.unwrap();
    assert!(job.is_finished() && job.verified() == 1, "{:?}", job.failures());
}

/// Timing, not run by default (see `transfer_speed_with_latency`): text
/// through a link capped at 1 MB/s, with and without compression.
#[tokio::test]
#[ignore]
async fn compression_speed() {
    use malasdoh_core::transfer::{self, TransferOptions};
    let Ok(script) = std::env::var("MALASDOH_DELAY_PROXY") else { return };
    let Some(env) = Env::new() else { return };
    let local = tempfile::tempdir().unwrap();
    let f = local.path().join("access.log");
    std::fs::write(&f, "203.0.113.7 - - [13/Oct/2026] \"GET /index.html HTTP/1.1\" 200 512\n".repeat(150_000)).unwrap();
    for compression in [false, true] {
        let mut p = env.profile();
        p.compression = compression;
        p.proxy = malasdoh_core::proxy::Proxy::Command { command: format!("python3 {script} %h %p 0.005 1000000") };
        let conn = Arc::new(ssh::connect(vec![p], &env.ctx(TestPrompter::accepting())).await.unwrap());
        let fs = conn.open_sftp().await.unwrap();
        let dest = format!("{}/c{compression}", env.scratch());
        std::fs::create_dir_all(&dest).unwrap();
        let opts = TransferOptions { verify: false, ..Default::default() };
        let mut job = transfer::plan_upload(std::slice::from_ref(&f), &dest, &opts).unwrap();
        let t = std::time::Instant::now();
        transfer::run(&fs, &mut job, &opts, &Progress::default(), opts.limit(), None).await.unwrap();
        eprintln!("10 MB of log text at 1 MB/s, compression {compression}: {:.1?}", t.elapsed());
    }
}

/// A name server for the test tunnel: every A query gets `answer`.
async fn tiny_dns(bind: &str, answer: [u8; 4]) -> tokio::task::JoinHandle<()> {
    let sock = tokio::net::UdpSocket::bind(bind).await.expect("bind DNS");
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = sock.recv_from(&mut buf).await {
            if n < 12 {
                continue;
            }
            let q = &buf[..n];
            // Header: same id, response, recursion available, 1 answer.
            let mut r = vec![q[0], q[1], 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
            r.extend_from_slice(&q[12..]); // the question as asked
            r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            r.extend_from_slice(&answer);
            let _ = sock.send_to(&r, from).await;
        }
    })
}

/// Through MalasDoh's own WireGuard tunnel, to a real WireGuard server.
/// Needs one at `10.9.9.1` (e.g. wireguard-go on wg0) whose client
/// configuration is in the file `MALASDOH_WIREGUARD` names; skipped
/// otherwise.
#[tokio::test]
async fn wireguard_tunnel_end_to_end() {
    use malasdoh_core::transfer::{self, Checker, TransferOptions};
    use malasdoh_core::wireguard::{self, WgConfig};
    let Ok(conf_path) = std::env::var("MALASDOH_WIREGUARD") else {
        eprintln!("skipped: set MALASDOH_WIREGUARD to a client .conf for a server at 10.9.9.1");
        return;
    };
    let text = std::fs::read_to_string(&conf_path).unwrap();
    let Some(sshd) = start_sshd_with(|_| "ListenAddress 10.9.9.1".into()) else { return };
    let env = Env::with(sshd).unwrap();
    let _dns = tiny_dns("10.9.9.1:53", [10, 9, 9, 1]).await;

    // The configuration as the app keeps it: key in the keychain.
    let id = uuid::Uuid::new_v4();
    let key = WgConfig::private_key_text(&text).unwrap();
    env.secrets.set(&SecretKey::WireGuardKey(id), &key).unwrap();
    let ctx = env.ctx(TestPrompter::accepting());
    ctx.wireguard.write().unwrap().insert(id, WgConfig::strip_private_key(&text));
    let mut p = env.profile();
    // A name only the tunnel's DNS server knows.
    p.host = "sshbox.wg.test".into();
    p.proxy = malasdoh_core::proxy::Proxy::WireGuard { tunnel: id, name: "test".into() };

    let conn = Arc::new(ssh::connect(vec![p.clone()], &ctx).await.unwrap_or_else(|e| panic!("{e:#}")));
    let out = conn.exec("echo through-wireguard; echo $SSH_CONNECTION", None).await.unwrap();
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.starts_with("through-wireguard"), "{out}");
    assert!(out.contains("10.9.9.2"), "the server sees the tunnel address: {out}");
    assert_eq!(wireguard::running().len(), 1);
    assert!(wireguard::running()[0].1.handshake);

    // A second connection shares the tunnel.
    let conn2 = Arc::new(ssh::connect(vec![p.clone()], &ctx).await.unwrap());
    assert_eq!(wireguard::running().len(), 1, "one tunnel for both");

    // A real transfer through it, checked by checksum on the server.
    let fs = conn.open_sftp().await.unwrap();
    let local = tempfile::tempdir().unwrap();
    let f = local.path().join("blob.bin");
    std::fs::write(&f, noise(24 << 20, 5)).unwrap();
    let opts = TransferOptions::default();
    let mut job = transfer::plan_upload(std::slice::from_ref(&f), &env.scratch(), &opts).unwrap();
    let t = std::time::Instant::now();
    transfer::run(&fs, &mut job, &opts, &Progress::default(), opts.limit(), Some(&Checker::new(conn.clone(), None)))
        .await
        .unwrap();
    assert!(job.is_finished() && job.verified() == 1, "{:?} {:?}", job.failures(), job.unverified);
    eprintln!("24 MiB through the tunnel in {:.1?}", t.elapsed());
    let out = conn2.exec("echo second", None).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "second");

    // When both are gone, so is the tunnel.
    drop(fs);
    conn.disconnect().await;
    conn2.disconnect().await;
    drop((conn, conn2));
    for _ in 0..50 {
        if wireguard::running().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(wireguard::running().is_empty(), "the tunnel ends with its last connection");

    // A wrong server key: no handshake, and a message that says so.
    let wrong = text
        .lines()
        .map(
            |l| if l.starts_with("PublicKey") { "PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=" } else { l },
        )
        .collect::<Vec<_>>()
        .join("\n");
    let id2 = uuid::Uuid::new_v4();
    env.secrets.set(&SecretKey::WireGuardKey(id2), &key).unwrap();
    ctx.wireguard.write().unwrap().insert(id2, WgConfig::strip_private_key(&wrong));
    p.host = "10.9.9.1".into();
    p.proxy = malasdoh_core::proxy::Proxy::WireGuard { tunnel: id2, name: "wrong".into() };
    let e = ssh::connect(vec![p], &ctx).await.err().expect("no handshake");
    assert!(format!("{e:#}").contains("no handshake"), "{e:#}");
}

/// Diagnostics, not run by default: raw TCP through the tunnel to a sink
/// at 10.9.9.1:9000, reporting every pause in progress over 1 s.
#[tokio::test]
#[ignore]
async fn wireguard_tcp_probe() {
    use malasdoh_core::wireguard::{self, WgConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(conf_path) = std::env::var("MALASDOH_WIREGUARD") else { return };
    let cfg = WgConfig::parse(&std::fs::read_to_string(conf_path).unwrap()).unwrap();
    let t = wireguard::tunnel(uuid::Uuid::new_v4(), &cfg).await.unwrap();
    let mut s = t.connect("10.9.9.1", 9000).await.unwrap();
    let chunk = vec![7u8; 64 * 1024];
    let total = 24usize << 20;
    let (start, mut last, mut sent) = (std::time::Instant::now(), std::time::Instant::now(), 0usize);
    while sent < total {
        s.write_all(&chunk).await.unwrap();
        sent += chunk.len();
        let gap = last.elapsed();
        if gap > std::time::Duration::from_secs(1) {
            eprintln!("pause of {gap:.1?} at {} MiB", sent >> 20);
        }
        last = std::time::Instant::now();
    }
    s.shutdown().await.unwrap();
    let mut back = String::new();
    s.read_to_string(&mut back).await.unwrap();
    eprintln!("sent {} MiB in {:.1?}; sink got {}", total >> 20, start.elapsed(), back.trim());
}
