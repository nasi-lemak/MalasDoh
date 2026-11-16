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
    ProxyPassword { user: String, proxy: String, retry: bool },
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
    /// The agent socket to forward instead of `SSH_AUTH_SOCK` (Unix; tests).
    pub agent_socket: Option<PathBuf>,
    /// WireGuard configurations (without private keys), by tunnel id.
    pub wireguard: crate::wireguard::Configs,
    /// Keys pasted into MalasDoh, for [`AuthMethod::Stored`].
    pub stored_keys: crate::stored_keys::StoredKeys,
}

impl<P> ConnectContext<P> {
    pub fn new(known_hosts: KnownHosts, secrets: Arc<dyn SecretStore>, prompter: Arc<P>) -> Self {
        let default_keys = user_ssh_dir()
            .map(|d| ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|n| d.join(n)).collect())
            .unwrap_or_default();
        Self {
            known_hosts,
            secrets,
            prompter,
            default_keys,
            agent_socket: None,
            wireguard: Default::default(),
            stored_keys: Default::default(),
        }
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
    /// Agent forwarding is on for this hop (the target only, when asked).
    /// Otherwise the server's agent channels are refused.
    forward_agent: Option<Option<PathBuf>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some(socket) = self.forward_agent.clone() else {
            reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        };
        match local_agent(socket.as_deref()).await {
            Ok(mut agent) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut ch = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut agent, &mut ch).await;
                });
            }
            Err(e) => {
                tracing::warn!("agent forwarding: {e:#}");
                reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            }
        }
        Ok(())
    }

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

/// Compression wanted: the delayed kind OpenSSH prefers, then plain zlib,
/// then none for servers that allow no compression.
const COMPRESS: &[russh::compression::Name] =
    &[russh::compression::ZLIB_LEGACY, russh::compression::ZLIB, russh::compression::NONE];

fn config_for(
    known: &[Algorithm],
    want_certificates: bool,
    keepalive_secs: u32,
    compress: bool,
) -> Arc<client::Config> {
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
            compression: if compress { Cow::Borrowed(COMPRESS) } else { Preferred::DEFAULT.compression },
            ..Preferred::DEFAULT
        },
        keepalive_interval: (keepalive_secs > 0).then(|| Duration::from_secs(keepalive_secs as u64)),
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
    // A Teleport login that has run out would only fail at authentication,
    // after a round trip and with a vaguer message.
    for hop in &chain {
        if let (Some(cluster), AuthMethod::Key { path }) = (hop.teleport, &hop.auth) {
            let path = crate::paths::expand_tilde(path);
            if !crate::teleport::cert_state(&path).is_some_and(|c| c.is_valid()) {
                return Err(crate::teleport::LoginNeeded { cluster, server: target.display_name() }.into());
            }
        }
    }
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
            // Never for jump hosts: only the server you are working on.
            forward_agent: (hops.len() + 1 == chain.len() && target.forward_agent).then(|| ctx.agent_socket.clone()),
        };
        let config = config_for(
            &ctx.known_hosts.known_algorithms(&hop.host, hop.port),
            !ctx.known_hosts.trusted_cas(&hop.host, hop.port).is_empty(),
            hop.keepalive_secs,
            hop.compression,
        );
        let result = match hops.last() {
            None if !hop.proxy.is_none() => {
                ctx.prompter.status(&format!("Connecting to {} through {}…", hop.destination(), hop.proxy.label()));
                let stream = through_proxy(hop, ctx).await?;
                client::connect_stream(config, stream, handler).await
            }
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

/// A connection to `p.host:p.port` the way `p` is reached as a first hop:
/// through its Proxy (WireGuard, SOCKS, HTTP, a command), or directly.
pub async fn dial<P: Prompter>(p: &Profile, ctx: &ConnectContext<P>) -> Result<Box<dyn crate::proxy::Stream>> {
    if p.proxy.is_none() {
        let tcp =
            tokio::time::timeout(Duration::from_secs(15), tokio::net::TcpStream::connect((p.host.as_str(), p.port)))
                .await
                .map_err(|_| anyhow::anyhow!("{}:{} did not answer in 15 s", p.host, p.port))?
                .with_context(|| format!("could not reach {}:{}", p.host, p.port))?;
        tcp.set_nodelay(true).ok();
        return Ok(Box::new(tcp));
    }
    through_proxy(p, ctx).await
}

/// Reach the first hop through its proxy, asking for (and remembering, if
/// told) the proxy password.
async fn through_proxy<P: Prompter>(hop: &Profile, ctx: &ConnectContext<P>) -> Result<Box<dyn crate::proxy::Stream>> {
    if let crate::proxy::Proxy::WireGuard { tunnel, name } = &hop.proxy {
        let conf = ctx.wireguard.read().unwrap().get(tunnel).cloned().with_context(|| {
            format!("the WireGuard tunnel \"{name}\" is not set up on this computer (Connection menu, WireGuard…)")
        })?;
        // The key is in the keychain, or (computers without one) still in
        // the stored configuration.
        let config = if crate::wireguard::WgConfig::private_key_text(&conf).is_some() {
            crate::wireguard::WgConfig::parse(&conf)
        } else {
            let key = secret_get(&ctx.secrets, SecretKey::WireGuardKey(*tunnel)).await.with_context(|| {
                format!("the private key of the WireGuard tunnel \"{name}\" is missing; import its configuration again")
            })?;
            crate::wireguard::WgConfig::parse_with_key(&conf, &key)
        }
        .with_context(|| format!("the WireGuard configuration \"{name}\""))?;
        let t = crate::wireguard::tunnel(*tunnel, &config).await?;
        ctx.prompter.status(&format!("Connecting to {} through WireGuard {name}…", hop.destination()));
        let s = t.connect(&hop.host, hop.port).await.with_context(|| format!("through WireGuard {name}"))?;
        return Ok(Box::new(s));
    }
    let Some(user) = hop.proxy.auth_user() else {
        return crate::proxy::connect(&hop.proxy, &hop.host, hop.port, &hop.user, None).await;
    };
    let key = SecretKey::ProxyPassword(hop.id);
    let mut stored = secret_get(&ctx.secrets, key.clone()).await;
    for attempt in 0..PASSWORD_ATTEMPTS {
        let from_store = stored.is_some();
        let (password, remember) = match stored.take() {
            Some(s) => (s, false),
            None => {
                let a = ctx
                    .prompter
                    .ask_secret(SecretPrompt::ProxyPassword {
                        user: user.to_string(),
                        proxy: hop.proxy.label(),
                        retry: attempt > 0,
                    })
                    .await
                    .ok_or(Cancelled)?;
                (a.secret, a.remember)
            }
        };
        match crate::proxy::connect(&hop.proxy, &hop.host, hop.port, &hop.user, Some(&password)).await {
            Ok(s) => {
                if remember {
                    secret_set(&ctx.secrets, key, password).await;
                }
                return Ok(s);
            }
            Err(e) if e.downcast_ref::<crate::proxy::ProxyAuthFailed>().is_some() => {
                if from_store {
                    secret_delete(&ctx.secrets, key.clone()).await;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(crate::proxy::ProxyAuthFailed.into())
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
    if let AuthMethod::Stored { id, name } = &p.auth
        && st.allows(MethodKind::PublicKey)
    {
        let source = stored_source(*id, name, ctx).await?;
        if try_source(h, &user, source, ctx, &mut st).await? {
            return Ok(());
        }
    }
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
    let err = anyhow::Error::from(AuthError { user, host: p.host.clone(), methods });
    if p.teleport.is_some() && p.jump.is_some() {
        // The proxy took the certificate, the server did not: most often the
        // login is not allowed there, or the cluster wants a code per session.
        return Err(err.context(format!(
            "Teleport refused {} on {}. Check that this login is allowed there (Find servers lists each server's logins). \
             If the cluster asks for a code for every session (require_session_mfa), MalasDoh cannot do that yet; \
             use tsh or the web page for it",
            p.user,
            p.display_name()
        )));
    }
    Err(err)
}

/// A raw connection to the local agent, for forwarding.
async fn local_agent(
    socket: Option<&std::path::Path>,
) -> Result<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>> {
    #[cfg(unix)]
    {
        let path = match socket {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(std::env::var_os("SSH_AUTH_SOCK").context("no SSH agent (SSH_AUTH_SOCK)")?),
        };
        Ok(Box::new(
            tokio::net::UnixStream::connect(&path)
                .await
                .with_context(|| format!("connecting to {}", path.display()))?,
        ))
    }
    #[cfg(windows)]
    {
        let _ = socket;
        if let Ok(a) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            return Ok(a.into_inner());
        }
        Ok(AgentClient::connect_pageant().await.context("no SSH agent or Pageant running")?.into_inner())
    }
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

/// A private key to try: its text, a name for messages and the passphrase
/// prompt, where a remembered passphrase is kept, and its certificate.
struct KeySource {
    text: String,
    label: String,
    passphrase_key: SecretKey,
    cert: Option<russh::keys::Certificate>,
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
    let label = path.display().to_string();
    // OpenSSH convention: the certificate for `id_ed25519` is `id_ed25519-cert.pub`.
    let cert_path = PathBuf::from(format!("{}-cert.pub", path.display()));
    let cert = if cert_path.exists() {
        match russh::keys::load_openssh_certificate(&cert_path) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("ignoring certificate {}: {e}", cert_path.display());
                None
            }
        }
    } else {
        None
    };
    let source = KeySource { text, passphrase_key: SecretKey::Passphrase(label.clone()), label, cert };
    try_source(h, user, source, ctx, st).await
}

/// A key pasted into MalasDoh: from the keychain (or, without one, the
/// stored keys file), with its certificate if it has one.
async fn stored_source<P>(id: uuid::Uuid, name: &str, ctx: &ConnectContext<P>) -> Result<KeySource> {
    let entry = ctx.stored_keys.read().unwrap().get(&id).cloned().with_context(|| {
        format!("the stored key \"{name}\" is gone (Tools → SSH keys); choose another key for this connection")
    })?;
    let secrets = ctx.secrets.clone();
    let e = entry.clone();
    let text = tokio::task::spawn_blocking(move || crate::stored_keys::private_key(&e, secrets.as_ref())).await??;
    let cert = match &entry.certificate {
        Some(line) => match russh::keys::Certificate::from_openssh(line) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("ignoring the certificate of {}: {e}", entry.name);
                None
            }
        },
        None => None,
    };
    Ok(KeySource {
        text,
        label: format!("stored key \"{}\"", entry.name),
        passphrase_key: SecretKey::Passphrase(format!("stored:{id}")),
        cert,
    })
}

async fn try_source<H, P>(
    h: &mut Handle<H>,
    user: &str,
    source: KeySource,
    ctx: &ConnectContext<P>,
    st: &mut AuthState,
) -> Result<bool>
where
    H: client::Handler<Error = russh::Error>,
    P: Prompter,
{
    let KeySource { text, label, passphrase_key: secret_key, cert } = source;
    let path_str = label.clone();
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
        Err(e) => return Err(anyhow::Error::from(e).context(format!("could not load key {label}"))),
    };
    let key = Arc::new(key);
    if let Some(cert) = cert
        && st.update(h.authenticate_openssh_cert(user, key.clone(), cert).await?)
    {
        return Ok(true);
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

/// Output of a command that keeps running (`tail -F`), as it arrives.
#[derive(Debug)]
pub enum StreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command ended (exit status, if the server sent one).
    Exited(Option<u32>),
}

/// A running command. Dropping it closes the channel, which ends the command.
pub struct ExecStream {
    pub events: mpsc::UnboundedReceiver<StreamEvent>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Connection {
    /// Start a command and stream its output. `stdin` is sent first (a sudo
    /// password, say); input is then left open, so a program that reads
    /// stdin waits instead of seeing end-of-file.
    pub async fn exec_stream(&self, command: &str, stdin: Option<Vec<u8>>) -> Result<ExecStream> {
        let mut ch = self.handle.channel_open_session().await?;
        ch.exec(true, command).await?;
        if let Some(input) = stdin {
            ch.data(&input[..]).await?;
        }
        let (tx, events) = mpsc::unbounded_channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = ch.wait() => match msg {
                        Some(ChannelMsg::Data { data }) => { let _ = tx.send(StreamEvent::Stdout(data.to_vec())); }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => { let _ = tx.send(StreamEvent::Stderr(data.to_vec())); }
                        Some(ChannelMsg::ExitStatus { exit_status }) => { let _ = tx.send(StreamEvent::Exited(Some(exit_status))); }
                        Some(ChannelMsg::Close) | None => { let _ = tx.send(StreamEvent::Exited(None)); break; }
                        _ => {}
                    },
                    // The stream was dropped: stop the command.
                    // The stream was dropped: stop the command. Closing alone
                    // can leave `tail` waiting until its next write.
                    _ = &mut stopped => {
                        let _ = ch.signal(russh::Sig::TERM).await;
                        let _ = ch.close().await;
                        break;
                    }
                }
            }
        });
        Ok(ExecStream { events, _stop: stop })
    }

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
        if self.profile.forward_agent {
            // A refusal (AllowAgentForwarding no) is not worth failing over.
            let _ = ch.agent_forward(false).await;
        }
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
        let mut ch = self.handle.channel_open_session().await?;
        ch.request_subsystem(true, "sftp").await?;
        // Wait for the server's answer, so a server without SFTP is detected
        // at once instead of after the SFTP handshake times out.
        let answer = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match ch.wait().await {
                    Some(ChannelMsg::Success) => return Ok(()),
                    Some(ChannelMsg::Failure) => bail!("the server has no SFTP subsystem"),
                    Some(ChannelMsg::Close) | None => bail!("the server closed the SFTP channel"),
                    Some(_) => {}
                }
            }
        })
        .await;
        match answer {
            Ok(r) => r?,
            Err(_) => bail!("the server did not answer the SFTP request"),
        }
        let session = russh_sftp::client::SftpSession::new(ch.into_stream()).await.context("starting SFTP")?;
        Ok(crate::sftp::RemoteFs::new(session))
    }

    /// SFTP as root: the server's own `sftp-server` run through sudo, so the
    /// browser and transfers can reach root-only folders. `password` is
    /// sudo's, or `None` when sudo needs none (it is written to the program's
    /// input first, and must not be when sudo will not read it).
    pub async fn open_sftp_sudo(&self, password: Option<&str>) -> Result<crate::sftp::RemoteFs> {
        // Find the program: what sshd is configured with, then the usual places.
        let find = "for p in \"$(awk '$1==\"Subsystem\" && $2==\"sftp\" {print $3}' /etc/ssh/sshd_config 2>/dev/null)\" \
                    /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server \
                    /usr/libexec/sftp-server /usr/lib/sftp-server /usr/local/libexec/sftp-server; do \
                    if [ -x \"$p\" ]; then echo \"$p\"; exit 0; fi; done; exit 1";
        let out = self.exec(find, None).await?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.success() || path.is_empty() {
            bail!("no sftp-server program was found on the server (needed to browse as root)");
        }
        let ch = self.handle.channel_open_session().await?;
        ch.exec(true, format!("exec sudo -S -p '' -- {}", shell_quote(&path))).await?;
        if let Some(p) = password {
            // sudo reads its password a byte at a time up to the newline;
            // what follows is left for sftp-server.
            ch.data(format!("{p}\n").as_bytes()).await?;
        }
        let start = russh_sftp::client::SftpSession::new(ch.into_stream());
        let session = tokio::time::timeout(Duration::from_secs(20), start)
            .await
            .map_err(|_| anyhow!("sftp-server did not start through sudo"))?
            .context("starting SFTP through sudo (does sudo need a terminal here?)")?;
        Ok(crate::sftp::RemoteFs::new(session))
    }

    /// File access: SFTP when the server offers it, otherwise shell commands
    /// (servers without an SFTP subsystem). Fails only if neither works.
    pub async fn open_fs(self: &Arc<Self>) -> Result<crate::sftp::RemoteFs> {
        match self.open_sftp().await {
            Ok(fs) => Ok(fs),
            Err(sftp_err) => {
                let probe = self.exec("sh -c 'echo malasdoh-shell-ok'", None).await;
                match probe {
                    Ok(out) if out.stdout.starts_with(b"malasdoh-shell-ok") => {
                        tracing::info!("no SFTP ({sftp_err:#}); using shell commands for files");
                        Ok(crate::sftp::RemoteFs::shell(self.clone()))
                    }
                    _ => Err(sftp_err.context("SFTP is not available and neither is a POSIX shell")),
                }
            }
        }
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
        let cfg = config_for(&[Algorithm::Rsa { hash: None }], false, 30, false);
        assert!(matches!(cfg.preferred.key[0], Algorithm::Rsa { .. }));
        let cfg = config_for(&[], true, 0, true);
        assert_eq!(cfg.preferred.compression[0], russh::compression::ZLIB_LEGACY);
        assert!(cfg.keepalive_interval.is_none());
        assert!(!cfg.preferred.host_key_certificates.is_empty());
        assert_eq!(cfg.preferred.key[0], Algorithm::Ed25519);
    }
}
