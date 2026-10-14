//! Opens database sessions: secrets from the keychain (or the form), cloud IAM
//! tokens, SSH tunnels, connect retries. Used by test, connect, reconnect and
//! schema diff so every path behaves the same.

use std::sync::Arc;
use std::time::Duration;

use dq_core::adapter::{ConnectParams, Session};
use dq_core::config::ConnConfig;
use dq_core::{DbResult, ErrorKind};
use dq_ssh::{HopSecrets, Tunnel};
use dq_store::{ConnectionDraft, PasswordChange, SavedConnection};

use crate::cloud_auth;
use crate::state::{AppState, blocking};

/// Secrets for one connection attempt. Held in memory only.
#[derive(Default)]
pub struct Secrets {
    pub password: Option<String>,
    pub ssh: HopSecrets,
}

/// An open session and the tunnel it runs through, if any.
pub struct Opened {
    pub session: Arc<dyn Session>,
    pub tunnel: Option<Tunnel>,
}

async fn keychain(state: &AppState, key: String) -> DbResult<Option<String>> {
    let secrets = state.secrets.clone();
    blocking(move || secrets.get(&key)).await
}

/// Secrets of a saved connection, read from the keychain.
pub async fn saved_secrets(state: &AppState, connection: &SavedConnection) -> DbResult<Secrets> {
    let password = if connection.has_password {
        keychain(state, SavedConnection::password_key(&connection.id)).await?
    } else {
        None
    };
    let mut ssh = Vec::new();
    for (hop, stored) in connection.ssh_secrets.iter().enumerate() {
        ssh.push(if *stored { keychain(state, SavedConnection::ssh_secret_key(&connection.id, hop)).await? } else { None });
    }
    Ok(Secrets { password, ssh })
}

/// Secrets for testing a form: typed values, or the saved ones for `Keep`.
pub async fn draft_secrets(state: &AppState, draft: &ConnectionDraft) -> DbResult<Secrets> {
    async fn pick(state: &AppState, change: &PasswordChange, key: Option<String>) -> DbResult<Option<String>> {
        Ok(match (change, key) {
            (PasswordChange::Set { value }, _) => Some(value.clone()).filter(|v| !v.is_empty()),
            (PasswordChange::Keep, Some(key)) => keychain(state, key).await?,
            _ => None,
        })
    }
    let id = draft.id.as_deref();
    let password = pick(state, &draft.password, id.map(SavedConnection::password_key)).await?;
    let hops = draft.config.ssh.as_ref().map_or(0, |t| t.hops().count());
    let mut ssh = Vec::with_capacity(hops);
    for hop in 0..hops {
        let change = draft.ssh_secrets.get(hop).unwrap_or(&PasswordChange::Keep);
        ssh.push(pick(state, change, id.map(|id| SavedConnection::ssh_secret_key(id, hop))).await?);
    }
    Ok(Secrets { password, ssh })
}

/// Opens a session, through a tunnel when configured, retrying network
/// failures as the connection's options allow.
pub async fn open(state: &AppState, config: &ConnConfig, secrets: &Secrets) -> DbResult<Opened> {
    config.validate()?;
    let adapter = state.registry.get(config.engine)?;
    let attempts = config.options.connect_retries + 1;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = async {
            let password = cloud_auth::password(config, secrets.password.clone()).await?;
            let tunnel = match &config.ssh {
                Some(ssh) => Some(
                    dq_ssh::open(
                        ssh,
                        &secrets.ssh,
                        (&config.host, config.port),
                        &state.known_hosts,
                        Duration::from_secs(u64::from(config.connect_timeout_secs)),
                    )
                    .await?,
                ),
                None => None,
            };
            let params = ConnectParams {
                config: config.clone(),
                password,
                endpoint: tunnel.as_ref().map(Tunnel::local_addr),
            };
            let session = adapter.connect(&params).await?;
            DbResult::Ok(Opened { session, tunnel })
        }
        .await;
        match result {
            Err(e) if attempt < attempts && matches!(e.kind, ErrorKind::Network | ErrorKind::Timeout) => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt.min(5) - 1));
                tracing::info!(attempt, ?delay, "connect failed, retrying");
                tokio::time::sleep(delay).await;
            }
            other => return other,
        }
    }
}
