//! Connecting: TCP or jump-host tunnels, host key verification, authentication,
//! and the channels a session needs (shell, SFTP, one-off commands).

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, ChannelWriteHalf, Disconnect, MethodKind, MethodSet, Preferred};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::forward::RemoteForwards;
use crate::known_hosts::{HostKeyStatus, KnownHosts, fingerprint};
use crate::paths::{expand_tilde, user_ssh_dir};
use crate::profile::{AuthMethod, Profile};
use crate::secrets::{SecretKey, SecretStore};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PASSWORD_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Questions the connection asks the user

#[derive(Debug, Clone)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    /// Key types already trusted for this host, if any (a server that added a
    /// new key type looks like this).
    pub other_keys: Vec<String>,
    /// Something the user should know, e.g. why a host certificate was refused.
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SecretPrompt {
    Password { user: String, host: String, retry: bool },
    Passphrase { key_path: String, retry: bool },
}

#[derive(Debug, Clone)]
pub struct SecretAnswer {
    pub secret: String,
    /// Save to the OS keychain once it is known to work.
    pub remember: bool,
}

#[derive(Debug, Clone)]
pub struct KbdPrompt {
    pub host: String,
    pub name: String,
    pub instructions: String,
    /// (prompt text, echo the answer?)
    pub prompts: Vec<(String, bool)>,
}

/// Implemented by the UI. Every method may take as long as the user needs;
/// returning `None`/`false` cancels the connection.
pub trait Prompter: Send + Sync + 'static {
    fn confirm_host_key(&self, prompt: HostKeyPrompt) -> impl Future<Output = bool> + Send;
    fn ask_secret(&self, prompt: SecretPrompt) -> impl Future<Output = Option<SecretAnswer>> + Send;
    fn keyboard_interactive(&self, prompt: KbdPrompt) -> impl Future<Output = Option<Vec<String>>> + Send;
    /// Progress text ("Authenticating…"). Default: ignored.
    fn status(&self, _message: &str) {}
}

/// Everything a connection attempt needs from the app.
pub struct ConnectContext<P> {
    pub known_hosts: KnownHosts,
    pub secrets: Arc<dyn SecretStore>,
    pub prompter: Arc<P>,
    /// Private keys tried by [`AuthMethod::Auto`]. Defaults to the usual
    /// `~/.ssh/id_*` files.
    pub default_keys: Vec<PathBuf>,
}

impl<P> ConnectContext<P> {
    pub fn new(known_hosts: KnownHosts, secrets: Arc<dyn SecretStore>, prompter: Arc<P>) -> Self {
        let default_keys = user_ssh_dir()
            .map(|d| ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| d.join(n)).collect())
            .unwrap_or_default();
        Self { known_hosts, secrets, prompter, default_keys }
    }
}

// ---------------------------------------------------------------------------
// Errors the UI handles specially

#[derive(Debug, Error)]
pub enum HostKeyError {
    #[error(
        "the host key for {host} has CHANGED since you last connected.\n\
         Recorded: {recorded}\nPresented: {presented}\n\
         This can mean someone is intercepting the connection. If the server was \
         reinstalled or its keys rotated, remove the old key ({file}, line {line}) and try again."
    )]
    Changed { host: String, port: u16, recorded: String, presented: String, file: PathBuf, line: usize },
    #[error("the host key for {host} is marked @revoked in {file} (line {line}); refusing to connect")]
    Revoked { host: String, file: PathBuf, line: usize },
    #[error("connection cancelled: the host key for {host} was not accepted")]
    Declined { host: String },
}

#[derive(Debug, Error)]
#[error("authentication failed for {user}@{host}{}", if .methods.is_empty() { String::new() } else { format!(" (the server accepts: {})", .methods) })]
pub struct AuthError {
    pub user: String,
    pub host: String,
    pub methods: String,
}

#[derive(Debug, Error)]
#[error("cancelled")]
pub struct Cancelled;

// ---------------------------------------------------------------------------
// The russh handler

enum Verdict {
    Ok,
    Failed(HostKeyError),
}

type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send>>;
type ConfirmHostKey = Arc<dyn Fn(HostKeyPrompt) -> BoxFuture<bool> + Send + Sync>;

pub struct Client {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    confirm: ConfirmHostKey,
    verdict: Arc<Mutex<Option<Verdict>>>,
    remote_forwards: RemoteForwards,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Only connections for forwards we asked for are accepted.
        if self.remote_forwards.lock().unwrap().contains_key(&connected_port) {
            reply.accept().await;
            crate::forward::accept_forwarded(&self.remote_forwards, connected_port, channel);
        } else {
            reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        }
        Ok(())
    }

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let mut warning = None;
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                // A valid certificate from a trusted CA is enough on its own;
                // otherwise judge the key inside it like any other.
                match self.known_hosts.check_certificate(&self.host, self.port, cert) {
                    Ok(()) => {
                        *self.verdict.lock().unwrap() = Some(Verdict::Ok);
                        return Ok(true);
                    }
                    Err(why) => warning = Some(format!("The server's host certificate was not accepted: {why}.")),
                }
                PublicKey::new(cert.public_key().clone(), "")
            }
        };
        let presented = fingerprint(&key);
        let verdict = match self.known_hosts.check(&self.host, self.port, &key) {
            HostKeyStatus::Trusted => Verdict::Ok,
            HostKeyStatus::Unknown { other_keys } => {
                let prompt = HostKeyPrompt {
                    host: self.host.clone(),
                    port: self.port,
                    algorithm: key.algorithm().to_string(),
                    fingerprint: presented,
                    other_keys,
                    warning,
                };
                if (self.confirm)(prompt).await {
                    if let Err(e) = self.known_hosts.learn(&self.host, self.port, &key) {
                        tracing::warn!("could not record host key: {e:#}");
                    }
                    Verdict::Ok
                } else {
                    Verdict::Failed(HostKeyError::Declined { host: self.host.clone() })
                }
            }
            HostKeyStatus::Changed { recorded_in, line, recorded_fingerprint } => {
                Verdict::Failed(HostKeyError::Changed {
                    host: self.host.clone(),
                    port: self.port,
                    recorded: recorded_fingerprint,
                    presented,
                    file: recorded_in,
                    line,
                })
            }
            HostKeyStatus::Revoked { recorded_in, line } => {
                Verdict::Failed(HostKeyError::Revoked { host: self.host.clone(), file: recorded_in, line })
            }
        };
        let ok = matches!(verdict, Verdict::Ok);
        *self.verdict.lock().unwrap() = Some(verdict);
        Ok(ok)
    }
}

fn same_family(a: &Algorithm, b: &Algorithm) -> bool {
    match (a, b) {
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
        _ => a == b,
    }
}

fn config_for(known: &[Algorithm], want_certificates: bool) -> Arc<client::Config> {
    // Offer key types we already trust for this host first, as OpenSSH does,
    // so a server with several host keys presents the one we can verify.
    let mut keys: Vec<Algorithm> = Preferred::DEFAULT.key.to_vec();
    keys.sort_by_key(|a| !known.iter().any(|k| same_family(k, a)));
    Arc::new(client::Config {
        // Ask for host certificates only when a CA is trusted for this host;
        // otherwise a certificate would just be judged as its plain key.
        preferred: Preferred {
            host_key_certificates: if want_certificates { Cow::Owned(keys.clone()) } else { Cow::Borrowed(&[]) },
            key: Cow::Owned(keys),
            ..Preferred::DEFAULT
        },
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Connecting

/// A live session to the last host in a jump chain. The intermediate hops are
/// kept alive for as long as this is.
pub struct Connection {
    handle: Handle<Client>,
    hops: Vec<Handle<Client>>,
    pub profile: Profile,
    remote_forwards: RemoteForwards,
}

/// Connect through `chain` (outermost hop first, target last), as produced by
/// [`crate::ProfileStore::chain`].
pub async fn connect<P: Prompter>(chain: Vec<Profile>, ctx: &ConnectContext<P>) -> Result<Connection> {
    let target = chain.last().cloned().context("nothing to connect to")?;
    let prompter = ctx.prompter.clone();
    let confirm: ConfirmHostKey = Arc::new(move |prompt| {
        let prompter = prompter.clone();
        Box::pin(async move { prompter.confirm_host_key(prompt).await })
    });
    let mut hops: Vec<Handle<Client>> = Vec::new();
    let mut remote_forwards = RemoteForwards::default();
    for hop in &chain {
        let via =
            if hops.is_empty() { String::new() } else { format!(" via {}", chain[hops.len() - 1].display_name()) };
        ctx.prompter.status(&format!("Connecting to {}{via}…", hop.destination()));
        let verdict = Arc::new(Mutex::new(None));
        // Each hop gets its own table; only the target's is ever filled.
        remote_forwards = RemoteForwards::default();
        let handler = Client {
            host: hop.host.clone(),
            port: hop.port,
            known_hosts: ctx.known_hosts.clone(),
            confirm: confirm.clone(),
            verdict: verdict.clone(),
            remote_forwards: remote_forwards.clone(),
        };
        let config = config_for(
            &ctx.known_hosts.known_algorithms(&hop.host, hop.port),
            !ctx.known_hosts.trusted_cas(&hop.host, hop.port).is_empty(),
        );
        let result = match hops.last() {
            None => {
                let tcp = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    tokio::net::TcpStream::connect((hop.host.as_str(), hop.port)),
                )
                .await
                .map_err(|_| anyhow!("timed out connecting to {}:{}", hop.host, hop.port))?
                .with_context(|| format!("could not reach {}:{}", hop.host, hop.port))?;
                tcp.set_nodelay(true).ok();
                client::connect_stream(config, tcp, handler).await
            }
            Some(prev) => {
                let ch = prev
                    .channel_open_direct_tcpip(hop.host.clone(), hop.port as u32, "127.0.0.1", 0)
                    .await
                    .with_context(|| format!("the jump host could not reach {}:{}", hop.host, hop.port))?;
                client::connect_stream(config, ch.into_stream(), handler).await
            }
        };
        let mut handle = match result {
            Ok(h) => h,
            Err(e) => {
                if let Some(Verdict::Failed(hk)) = verdict.lock().unwrap().take() {
                    return Err(hk.into());
                }
                return Err(anyhow::Error::from(e).context(format!("SSH handshake with {} failed", hop.host)));
            }
        };
        ctx.prompter.status(&format!("Authenticating as {}…", hop.destination()));
        authenticate(&mut handle, hop, ctx).await?;
        hops.push(handle);
    }
    let handle = hops.pop().expect("chain is not empty");
    Ok(Connection { handle, hops, profile: target, remote_forwards })
}

async fn secret_get(store: &Arc<dyn SecretStore>, key: SecretKey) -> Option<String> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.get(&key)).await.ok().flatten()
}

async fn secret_set(store: &Arc<dyn SecretStore>, key: SecretKey, value: String) {
    let store = store.clone();
    let res = tokio::task::spawn_blocking(move || store.set(&key, &value)).await;
    if let Ok(Err(e)) = res {
        tracing::warn!("{e}");
    }
}

async fn secret_delete(store: &Arc<dyn SecretStore>, key: SecretKey) {
    let store = store.clone();
    let _ = tokio::task::spawn_blocking(move || store.delete(&key)).await;
}

struct AuthState {
    /// What the server says it still accepts; `None` until it tells us.
    remaining: Option<MethodSet>,
    partial: bool,
}

impl AuthState {
    fn allows(&self, m: MethodKind) -> bool {
        self.remaining.as_ref().is_none_or(|r| r.contains(&m))
    }

    /// Record a failed attempt; returns true on success.
    fn update(&mut self, r: AuthResult) -> bool {
        match r {
            AuthResult::Success => true,
            AuthResult::Failure { remaining_methods, partial_success } => {
                self.remaining = Some(remaining_methods);
                self.partial |= partial_success;
                false
            }
        }
    }
}

async fn authenticate<H, P>(h: &mut Handle<H>, p: &Profile, ctx: &ConnectContext<P>) -> Result<()>
where
    H: client::Handler<Error = russh::Error>,
    P: Prompter,
{
    let user = p.user.clone();
    let mut st = AuthState { remaining: None, partial: false };
    // Asking for "none" first tells us which methods the server accepts, so we
    // do not prompt for a password the server would never take.
    if st.update(h.authenticate_none(&user).await?) {
        return Ok(());
    }

    if matches!(p.auth, AuthMethod::Auto | AuthMethod::Agent) && st.allows(MethodKind::PublicKey) {
        match try_agent(h, &user, &mut st).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) if p.auth == AuthMethod::Agent => return Err(e.context("SSH agent authentication failed")),
            Err(e) => tracing::debug!("agent: {e:#}"),
        }
    }

    let keys: Vec<PathBuf> = match &p.auth {
        AuthMethod::Key { path } => vec![expand_tilde(path)],
        AuthMethod::Auto => ctx.default_keys.iter().filter(|k| k.exists()).cloned().collect(),
        _ => vec![],
    };
    for key in keys {
        if !st.allows(MethodKind::PublicKey) {
            break;
        }
        match try_key(h, &user, &key, ctx, &mut st).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) if e.is::<Cancelled>() => return Err(e),
            Err(e) if matches!(p.auth, AuthMethod::Key { .. }) => return Err(e),
            Err(e) => tracing::debug!("key {}: {e:#}", key.display()),
        }
    }

    // A server that wants a second factor after a key (partial success) gets a
    // password even if the profile is key-only.
    if matches!(p.auth, AuthMethod::Auto | AuthMethod::Password) || st.partial {
        if st.allows(MethodKind::Password) && try_password(h, p, ctx, &mut st).await? {
            return Ok(());
        }
        if st.allows(MethodKind::KeyboardInteractive) && try_keyboard_interactive(h, p, ctx, &mut st).await? {
            return Ok(());
        }
    }

    let methods = st.remaining.map(|m| m.iter().map(<&str>::from).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    Err(AuthError { user, host: p.host.clone(), methods }.into())
}

async fn try_agent<H>(h: &mut Handle<H>, user: &str, st: &mut AuthState) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
{
    #[cfg(unix)]
    {
        let agent = AgentClient::connect_env().await.context("no SSH agent (SSH_AUTH_SOCK)")?;
        agent_auth(h, user, agent, st).await
    }
    #[cfg(windows)]
    {
        // Windows' built-in OpenSSH agent first, then Pageant (PuTTY).
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await
            && agent_auth(h, user, agent, st).await?
        {
            return Ok(true);
        }
        match AgentClient::connect_pageant().await {
            Ok(agent) => agent_auth(h, user, agent, st).await,
            Err(_) => Ok(false),
        }
    }
}

async fn agent_auth<H, S>(h: &mut Handle<H>, user: &str, mut agent: AgentClient<S>, st: &mut AuthState) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
    S: russh::keys::agent::client::AgentStream + tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let identities = agent.request_identities().await?;
    for id in identities {
        let rsa = match &id {
            AgentIdentity::PublicKey { key, .. } => matches!(key.algorithm(), Algorithm::Rsa { .. }),
            AgentIdentity::Certificate { certificate, .. } => matches!(certificate.algorithm(), Algorithm::Rsa { .. }),
        };
        let hash = if rsa { h.best_supported_rsa_hash().await?.flatten() } else { None };
        let res = match id {
            AgentIdentity::PublicKey { key, .. } => h.authenticate_publickey_with(user, key, hash, &mut agent).await?,
            AgentIdentity::Certificate { certificate, .. } => {
                h.authenticate_certificate_with(user, certificate, hash, &mut agent).await?
            }
        };
        if st.update(res) {
            return Ok(true);
        }
        if !st.allows(MethodKind::PublicKey) {
            break;
        }
    }
    Ok(false)
}

async fn try_key<H, P>(
    h: &mut Handle<H>,
    user: &str,
    path: &PathBuf,
    ctx: &ConnectContext<P>,
    st: &mut AuthState,
) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
    P: Prompter,
{
    let text = std::fs::read_to_string(path).with_context(|| format!("reading key {}", path.display()))?;
    let path_str = path.display().to_string();
    let secret_key = SecretKey::Passphrase(path_str.clone());
    let key = match russh::keys::decode_secret_key(&text, None) {
        Ok(k) => k,
        Err(russh::keys::Error::KeyIsEncrypted) => {
            let mut stored = secret_get(&ctx.secrets, secret_key.clone()).await;
            let mut retry = false;
            loop {
                let (pass, remember, from_store) = match stored.take() {
                    Some(s) => (s, false, true),
                    None => {
                        let a = ctx
                            .prompter
                            .ask_secret(SecretPrompt::Passphrase { key_path: path_str.clone(), retry })
                            .await
                            .ok_or(Cancelled)?;
                        (a.secret, a.remember, false)
                    }
                };
                match russh::keys::decode_secret_key(&text, Some(&pass)) {
                    Ok(k) => {
                        if remember {
                            secret_set(&ctx.secrets, secret_key.clone(), pass).await;
                        }
                        break k;
                    }
                    Err(_) => {
                        if from_store {
                            secret_delete(&ctx.secrets, secret_key.clone()).await;
                        }
                        retry = true;
                    }
                }
            }
        }
        Err(e) => return Err(anyhow::Error::from(e).context(format!("could not load key {}", path.display()))),
    };
    let key = Arc::new(key);
    // OpenSSH convention: the certificate for `id_ed25519` is `id_ed25519-cert.pub`.
    let cert_path = PathBuf::from(format!("{}-cert.pub", path.display()));
    if cert_path.exists() {
        match russh::keys::load_openssh_certificate(&cert_path) {
            Ok(cert) => {
                if st.update(h.authenticate_openssh_cert(user, key.clone(), cert).await?) {
                    return Ok(true);
                }
            }
            Err(e) => tracing::warn!("ignoring certificate {}: {e}", cert_path.display()),
        }
    }
    let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
        h.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    let res = h.authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, hash)).await?;
    Ok(st.update(res))
}

async fn try_password<H, P>(h: &mut Handle<H>, p: &Profile, ctx: &ConnectContext<P>, st: &mut AuthState) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
    P: Prompter,
{
    let key = SecretKey::Password(p.id);
    let mut stored = secret_get(&ctx.secrets, key.clone()).await;
    for attempt in 0..PASSWORD_ATTEMPTS {
        let (password, remember) = match stored.take() {
            Some(s) => (s, false),
            None => {
                let a = ctx
                    .prompter
                    .ask_secret(SecretPrompt::Password {
                        user: p.user.clone(),
                        host: p.host.clone(),
                        retry: attempt > 0,
                    })
                    .await
                    .ok_or(Cancelled)?;
                (a.secret, a.remember)
            }
        };
        let res = h.authenticate_password(&p.user, password.clone()).await?;
        if st.update(res) {
            if remember {
                secret_set(&ctx.secrets, key, password).await;
            }
            return Ok(true);
        }
        if !st.allows(MethodKind::Password) {
            break;
        }
    }
    Ok(false)
}

async fn try_keyboard_interactive<H, P>(
    h: &mut Handle<H>,
    p: &Profile,
    ctx: &ConnectContext<P>,
    st: &mut AuthState,
) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
    P: Prompter,
{
    // PAM usually asks a single "Password:" here; answer it from the keychain
    // when we can, and show anything else (OTP codes, etc.) to the user.
    let mut stored = secret_get(&ctx.secrets, SecretKey::Password(p.id)).await;
    let mut resp = h.authenticate_keyboard_interactive_start(&p.user, None).await?;
    for _ in 0..10 {
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { remaining_methods, partial_success } => {
                st.remaining = Some(remaining_methods);
                st.partial |= partial_success;
                return Ok(false);
            }
            KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                let answers = if prompts.is_empty() {
                    vec![]
                } else if prompts.len() == 1
                    && !prompts[0].echo
                    && prompts[0].prompt.to_lowercase().contains("password")
                    && stored.is_some()
                {
                    vec![stored.take().unwrap()]
                } else {
                    let prompt = KbdPrompt {
                        host: p.host.clone(),
                        name,
                        instructions,
                        prompts: prompts.iter().map(|q| (q.prompt.clone(), q.echo)).collect(),
                    };
                    ctx.prompter.keyboard_interactive(prompt).await.ok_or(Cancelled)?
                };
                resp = h.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
    bail!("the server kept asking keyboard-interactive questions")
}

// ---------------------------------------------------------------------------
// Using a connection

/// Called from background tasks when there is something new to show.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
pub enum ShellEvent {
    Output(Vec<u8>),
    /// The remote shell exited (with its status, if the server sent one).
    Exited(Option<u32>),
}

/// An interactive shell with a PTY.
pub struct Shell {
    writer: ChannelWriteHalf<client::Msg>,
    pub events: mpsc::UnboundedReceiver<ShellEvent>,
}

impl Shell {
    pub async fn send(&self, bytes: &[u8]) -> Result<()> {
        self.writer.data_bytes(bytes.to_vec()).await?;
        Ok(())
    }

    pub async fn resize(&self, cols: u32, rows: u32) -> Result<()> {
        self.writer.window_change(cols, rows, 0, 0).await?;
        Ok(())
    }

    pub fn writer(&self) -> &ChannelWriteHalf<client::Msg> {
        &self.writer
    }
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub status: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

impl Connection {
    pub(crate) fn handle(&self) -> &Handle<Client> {
        &self.handle
    }

    pub(crate) fn remote_forwards(&self) -> &RemoteForwards {
        &self.remote_forwards
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Hops between us and the target, for display ("via bastion").
    pub fn hop_count(&self) -> usize {
        self.hops.len()
    }

    pub async fn open_shell(&self, cols: u32, rows: u32, notify: Notify) -> Result<Shell> {
        let ch = self.handle.channel_open_session().await?;
        ch.request_pty(true, "xterm-256color", cols, rows, 0, 0, &[]).await?;
        ch.request_shell(true).await?;
        let (mut read, writer) = ch.split();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut status = None;
            while let Some(msg) = read.wait().await {
                match msg {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        if tx.send(ShellEvent::Output(data.to_vec())).is_err() {
                            return;
                        }
                        notify();
                    }
                    ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            let _ = tx.send(ShellEvent::Exited(status));
            notify();
        });
        Ok(Shell { writer, events: rx })
    }

    pub async fn open_sftp(&self) -> Result<crate::sftp::RemoteFs> {
        let ch = self.handle.channel_open_session().await?;
        ch.request_subsystem(true, "sftp").await?;
        let session = russh_sftp::client::SftpSession::new(ch.into_stream()).await.context("starting SFTP")?;
        Ok(crate::sftp::RemoteFs::new(session))
    }

    /// Run a command without a PTY and collect its output.
    pub async fn exec(&self, command: &str, stdin: Option<&[u8]>) -> Result<ExecOutput> {
        let mut ch = self.handle.channel_open_session().await?;
        ch.exec(true, command).await?;
        if let Some(input) = stdin {
            ch.data(input).await?;
        }
        ch.eof().await?;
        let mut out = ExecOutput { status: None, stdout: Vec::new(), stderr: Vec::new() };
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => out.status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(out)
    }

    pub async fn disconnect(&self) {
        let _ = self.handle.disconnect(Disconnect::ByApplication, "", "en").await;
        for hop in self.hops.iter().rev() {
            let _ = hop.disconnect(Disconnect::ByApplication, "", "en").await;
        }
    }
}

/// Quote for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/etc/it's here"), r"'/etc/it'\''s here'");
    }

    #[test]
    fn known_key_types_go_first() {
        let cfg = config_for(&[Algorithm::Rsa { hash: None }], false);
        assert!(matches!(cfg.preferred.key[0], Algorithm::Rsa { .. }));
        let cfg = config_for(&[], true);
        assert!(!cfg.preferred.host_key_certificates.is_empty());
        assert_eq!(cfg.preferred.key[0], Algorithm::Ed25519);
    }
}
