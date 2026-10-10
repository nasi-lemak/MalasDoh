//! End-to-end tests against a real OpenSSH server.
//!
//! Each test starts its own `sshd` on a free localhost port with a throwaway
//! host key and authorized key, so nothing on the machine is touched. If
//! `sshd` is not installed the tests print a note and pass; CI installs it.

#![cfg(unix)]

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
    let Some(sshd) = find_sshd() else {
        eprintln!("sshd not installed; skipping end-to-end test");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    keygen(&d.join("host_ed25519"), "");
    keygen(&d.join("client"), PASSPHRASE);
    std::fs::copy(d.join("client.pub"), d.join("authorized_keys")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::fs::write(
        d.join("sshd_config"),
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_ed25519\nPidFile {d}/sshd.pid\n\
             AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nAllowTcpForwarding yes\n\
             Subsystem sftp {}\nLogLevel ERROR\n",
            sftp_server(),
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
        let sshd = start_sshd()?;
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
