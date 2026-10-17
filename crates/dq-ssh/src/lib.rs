//! SSH tunnels for database connections.
//!
//! A tunnel connects through optional jump hosts to the tunnel host, then
//! listens on `127.0.0.1:<random port>` and forwards every accepted socket to
//! the database through a `direct-tcpip` channel. Adapters connect to the local
//! address while TLS still verifies the database's own host name.
//!
//! Host keys are verified against `~/.ssh/known_hosts` and an app-managed
//! known-hosts file. Unknown keys fail with their SHA-256 fingerprint so the
//! user can compare and trust them explicitly; changed keys always fail.

mod key;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dq_core::config::{SshAuth, SshHop, SshTunnel};
use dq_core::{DbError, DbResult, ErrorKind};
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Error code for an unknown host key; `detail` carries the fingerprint.
pub const UNKNOWN_HOST_KEY: &str = "SSH_HOST_KEY_UNKNOWN";
/// Error code for a host key that differs from the recorded one.
pub const CHANGED_HOST_KEY: &str = "SSH_HOST_KEY_CHANGED";

/// Known host keys: the user's `~/.ssh/known_hosts` (read-only) plus a file
/// owned by the app for keys the user trusted in DataQuery. Keys seen but not
/// yet trusted are remembered in memory so they can be trusted by fingerprint.
pub struct KnownHosts {
    app_file: PathBuf,
    user_file: Option<PathBuf>,
    pending: Mutex<HashMap<(String, u16), PublicKey>>,
}

impl KnownHosts {
    pub fn new(app_file: PathBuf) -> Self {
        let user_file = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| Path::new(&home).join(".ssh").join("known_hosts"));
        Self { app_file, user_file, pending: Mutex::new(HashMap::new()) }
    }

    /// Uses only `app_file`; for tests.
    pub fn app_only(app_file: PathBuf) -> Self {
        Self { app_file, user_file: None, pending: Mutex::new(HashMap::new()) }
    }

    fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyCheck {
        let mut known = false;
        for file in self.user_file.iter().chain(std::iter::once(&self.app_file)).filter(|f| f.exists()) {
            match russh::keys::check_known_hosts_path(host, port, key, file) {
                Ok(true) => known = true,
                Ok(false) => {}
                Err(russh::keys::Error::KeyChanged { line }) => {
                    return HostKeyCheck::Changed { file: file.clone(), line };
                }
                Err(e) => tracing::warn!(error = %e, file = %file.display(), "cannot read known_hosts"),
            }
        }
        if known {
            return HostKeyCheck::Trusted;
        }
        self.pending.lock().expect("known hosts lock").insert((host.to_owned(), port), key.clone());
        HostKeyCheck::Unknown { fingerprint: fingerprint(key), algorithm: key.algorithm().to_string() }
    }

    /// Trusts the key last offered by `host:port` if its fingerprint is
    /// `expected` (the one the user was shown).
    pub fn trust(&self, host: &str, port: u16, expected: &str) -> DbResult<()> {
        let key = self
            .pending
            .lock()
            .expect("known hosts lock")
            .remove(&(host.to_owned(), port))
            .ok_or_else(|| DbError::invalid("Connect again to see this host's key before trusting it."))?;
        if fingerprint(&key) != expected {
            return Err(DbError::invalid("The host key changed since it was shown. Connect again and compare."));
        }
        if let Some(dir) = self.app_file.parent() {
            std::fs::create_dir_all(dir).map_err(storage)?;
        }
        russh::keys::known_hosts::learn_known_hosts_path(host, port, &key, &self.app_file)
            .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot save the host key: {e}")))
    }
}

fn storage(e: std::io::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Cannot save the host key: {e}"))
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

enum HostKeyCheck {
    Trusted,
    Unknown { fingerprint: String, algorithm: String },
    Changed { file: PathBuf, line: usize },
}

/// Verifies the server key of one hop and remembers why it was rejected.
struct Client {
    host: String,
    port: u16,
    known: Arc<KnownHosts>,
    rejected: Arc<Mutex<Option<HostKeyCheck>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    /// The check is a synchronous known-hosts lookup, so the future is ready at once.
    fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let key = match offered {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::new(cert.public_key().clone(), ""),
        };
        let trusted = match self.known.check(&self.host, self.port, &key) {
            HostKeyCheck::Trusted => true,
            other => {
                *self.rejected.lock().expect("host key lock") = Some(other);
                false
            }
        };
        std::future::ready(Ok(trusted))
    }
}

/// One secret per hop, in connection order (jump hosts first): the password,
/// or the private-key passphrase. Agent auth needs none.
pub type HopSecrets = Vec<Option<String>>;

/// An open tunnel. Dropping it closes the listener and the SSH sessions.
pub struct Tunnel {
    local: SocketAddr,
    accept: JoinHandle<()>,
    sessions: Vec<Arc<Handle<Client>>>,
}

impl Tunnel {
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// True while every SSH session in the chain is open.
    pub fn is_open(&self) -> bool {
        !self.accept.is_finished() && self.sessions.iter().all(|s| !s.is_closed())
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel").field("local", &self.local).field("hops", &self.sessions.len()).finish_non_exhaustive()
    }
}

/// Opens the tunnel to `target` (the database host and port as seen from the
/// tunnel host).
pub async fn open(
    tunnel: &SshTunnel,
    secrets: &HopSecrets,
    target: (&str, u16),
    known: &Arc<KnownHosts>,
    timeout: Duration,
) -> DbResult<Tunnel> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        inactivity_timeout: None,
        // Database protocols are request/response; Nagle would delay each reply.
        nodelay: true,
        ..client::Config::default()
    });
    let mut sessions: Vec<Arc<Handle<Client>>> = Vec::new();
    for (i, hop) in tunnel.hops().enumerate() {
        let secret = secrets.get(i).cloned().flatten();
        let handle = tokio::time::timeout(timeout, connect_hop(&config, hop, secret, sessions.last(), known))
            .await
            .map_err(|_| {
            DbError::new(ErrorKind::Timeout, format!("Timed out connecting to SSH host {}:{}.", hop.host, hop.port))
        })??;
        sessions.push(Arc::new(handle));
    }
    let last = sessions.last().cloned().ok_or_else(|| DbError::internal("tunnel without hops"))?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(network)?;
    let local = listener.local_addr().map_err(network)?;
    let (host, port) = (target.0.to_owned(), target.1);
    let accept = tokio::spawn(async move {
        while let Ok((mut socket, peer)) = listener.accept().await {
            let session = last.clone();
            let host = host.clone();
            tokio::spawn(async move {
                let channel =
                    match session.channel_open_direct_tcpip(host, u32::from(port), peer.ip().to_string(), 0).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(error = %e, "ssh tunnel could not open a channel");
                            return;
                        }
                    };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
    });
    Ok(Tunnel { local, accept, sessions })
}

async fn connect_hop(
    config: &Arc<client::Config>,
    hop: &SshHop,
    secret: Option<String>,
    via: Option<&Arc<Handle<Client>>>,
    known: &Arc<KnownHosts>,
) -> DbResult<Handle<Client>> {
    let rejected = Arc::new(Mutex::new(None));
    let handler = Client { host: hop.host.clone(), port: hop.port, known: known.clone(), rejected: rejected.clone() };
    let connected = match via {
        None => client::connect(config.clone(), (hop.host.as_str(), hop.port), handler).await,
        Some(previous) => {
            let channel = previous
                .channel_open_direct_tcpip(hop.host.clone(), u32::from(hop.port), "127.0.0.1", 0)
                .await
                .map_err(|e| ssh_error(hop, &e))?;
            client::connect_stream(config.clone(), channel.into_stream(), handler).await
        }
    };
    let mut handle = match connected {
        Ok(h) => h,
        Err(e) => {
            let rejection = rejected.lock().expect("host key lock").take();
            return Err(match rejection {
                Some(check) => host_key_error(hop, check),
                None => ssh_error(hop, &e),
            });
        }
    };
    authenticate(&mut handle, hop, secret).await?;
    Ok(handle)
}

async fn authenticate(handle: &mut Handle<Client>, hop: &SshHop, secret: Option<String>) -> DbResult<()> {
    let user = hop.username.clone();
    let result = match &hop.auth {
        SshAuth::Password => {
            let password = secret.ok_or_else(|| auth_error(hop, "No SSH password is saved for this host."))?;
            handle.authenticate_password(user, password).await.map_err(|e| ssh_error(hop, &e))?
        }
        SshAuth::PrivateKey { path } => {
            let key = key::load_private_key(path, secret.as_deref()).map_err(|e| match e {
                key::KeyError::Encrypted => {
                    auth_error(hop, "The SSH key is encrypted. Save its passphrase with the connection.")
                }
                key::KeyError::Unusable(message) => auth_error(hop, &message),
            })?;
            let hash = handle.best_supported_rsa_hash().await.map_err(|e| ssh_error(hop, &e))?.flatten();
            handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| ssh_error(hop, &e))?
        }
        SshAuth::Agent => return authenticate_with_agent(handle, hop).await,
    };
    if result.success() { Ok(()) } else { Err(auth_error(hop, "The SSH server rejected the credentials.")) }
}

#[cfg(unix)]
async fn authenticate_with_agent(handle: &mut Handle<Client>, hop: &SshHop) -> DbResult<()> {
    use russh::keys::agent::client::AgentClient;
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| auth_error(hop, &format!("Cannot reach the SSH agent ({e}). Is SSH_AUTH_SOCK set?")))?;
    let identities = agent.request_identities().await.map_err(|e| auth_error(hop, &e.to_string()))?;
    if identities.is_empty() {
        return Err(auth_error(hop, "The SSH agent has no keys. Add one with ssh-add."));
    }
    let hash = handle.best_supported_rsa_hash().await.map_err(|e| ssh_error(hop, &e))?.flatten();
    for identity in identities {
        let key = identity.public_key().into_owned();
        match handle.authenticate_publickey_with(hop.username.clone(), key, hash, &mut agent).await {
            Ok(result) if result.success() => return Ok(()),
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "agent key was not accepted"),
        }
    }
    Err(auth_error(hop, "The SSH server accepted none of the agent's keys."))
}

#[cfg(not(unix))]
async fn authenticate_with_agent(_handle: &mut Handle<Client>, hop: &SshHop) -> DbResult<()> {
    Err(auth_error(hop, "SSH agent authentication is available on macOS and Linux. Use a key file instead."))
}

fn host_key_error(hop: &SshHop, check: HostKeyCheck) -> DbError {
    match check {
        HostKeyCheck::Unknown { fingerprint, algorithm } => DbError::new(
            ErrorKind::Tls,
            format!("The SSH host {}:{} is not known yet.", hop.host, hop.port),
        )
        .with_code(UNKNOWN_HOST_KEY)
        .with_detail(Some(format!("Host {}:{} · {algorithm} · {fingerprint}", hop.host, hop.port)))
        .with_action(Some(
            "Compare the fingerprint with the one your administrator gave you, then trust the key to continue.",
        )),
        HostKeyCheck::Changed { file, line } => DbError::new(
            ErrorKind::Tls,
            format!("The SSH host key of {}:{} changed. The connection was refused.", hop.host, hop.port),
        )
        .with_code(CHANGED_HOST_KEY)
        .with_detail(Some(format!("Recorded in {} line {line}.", file.display())))
        .with_action(Some(
            "This can mean someone is intercepting the connection. If the server was reinstalled, remove the old key from known_hosts.",
        )),
        HostKeyCheck::Trusted => DbError::internal("trusted host key was rejected"),
    }
}

fn auth_error(hop: &SshHop, message: &str) -> DbError {
    DbError::new(ErrorKind::Auth, format!("SSH {}@{}: {message}", hop.username, hop.host)).with_code("SSH")
}

fn ssh_error(hop: &SshHop, e: &russh::Error) -> DbError {
    let kind = match e {
        russh::Error::IO(_) | russh::Error::Disconnect | russh::Error::HUP | russh::Error::ChannelOpenFailure(_) => {
            ErrorKind::Network
        }
        _ => ErrorKind::Server,
    };
    DbError::new(kind, format!("SSH {}:{}: {e}", hop.host, hop.port)).with_code("SSH")
}

fn network(e: std::io::Error) -> DbError {
    DbError::new(ErrorKind::Network, format!("Cannot open the local tunnel port: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_trusted_only_by_matching_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let known = KnownHosts::app_only(dir.path().join("known_hosts"));
        let key = russh::keys::parse_public_key_base64(
            "AAAAC3NzaC1lZDI1NTE5AAAAIBIaTVo9Oi7QgCtZ9I1WE0+gyit5Hje4Qq6l0jiptYyz",
        )
        .unwrap();
        let HostKeyCheck::Unknown { fingerprint: shown, .. } = known.check("db", 2222, &key) else {
            panic!("a new key must be unknown");
        };
        assert!(known.trust("db", 2222, "SHA256:wrong").is_err());
        // A failed attempt forgets the offered key; offer it again.
        known.check("db", 2222, &key);
        known.trust("db", 2222, &shown).unwrap();
        assert!(matches!(known.check("db", 2222, &key), HostKeyCheck::Trusted));
        assert!(matches!(known.check("db", 22, &key), HostKeyCheck::Unknown { .. }));
    }
}
