//! Application state shared by all commands.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use dq_core::adapter::Session;
use dq_core::{DbError, DbResult, ErrorKind};
use dq_ssh::{KnownHosts, Tunnel};
use dq_store::{SavedConnection, SecretStore, Settings, Store};
use serde::Serialize;
use tokio::sync::RwLock;
use ts_rs::TS;

use crate::registry::Registry;

/// Transaction state of a session's query connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TxStatus {
    /// Every statement commits on its own.
    #[default]
    Auto,
    /// A transaction is open; changes are visible only to this session until Commit.
    Open,
    /// A statement failed inside the transaction; only Rollback is possible (PostgreSQL).
    Failed,
}

pub struct OpenSession {
    pub connection_id: String,
    pub session: Arc<dyn Session>,
    /// Kept alive as long as the session; dropping it closes the tunnel.
    pub tunnel: Option<Tunnel>,
    pub tx: Mutex<TxStatus>,
    /// Disconnect after this many idle minutes; 0 never.
    pub idle_timeout_mins: u32,
    last_used_ms: AtomicI64,
}

impl OpenSession {
    pub fn new(
        connection_id: String,
        session: Arc<dyn Session>,
        tunnel: Option<Tunnel>,
        idle_timeout_mins: u32,
    ) -> Self {
        Self {
            connection_id,
            session,
            tunnel,
            tx: Mutex::new(TxStatus::Auto),
            idle_timeout_mins,
            last_used_ms: AtomicI64::new(now_ms()),
        }
    }

    pub fn tx_status(&self) -> TxStatus {
        *self.tx.lock().expect("tx lock")
    }

    pub fn set_tx_status(&self, status: TxStatus) {
        *self.tx.lock().expect("tx lock") = status;
    }

    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// True when the idle timeout has passed. An open transaction keeps the session.
    pub fn idle_expired(&self, now: i64) -> bool {
        self.idle_timeout_mins > 0
            && self.tx_status() == TxStatus::Auto
            && now - self.last_used_ms.load(Ordering::Relaxed) > i64::from(self.idle_timeout_mins) * 60_000
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub struct AppState {
    pub store: Arc<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub registry: Registry,
    pub known_hosts: Arc<KnownHosts>,
    pub sessions: RwLock<HashMap<String, Arc<OpenSession>>>,
    /// Cancel flags of running exports and imports, by job id.
    pub transfers: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
}

impl AppState {
    pub fn new(store: Store, secrets: Arc<dyn SecretStore>, known_hosts: KnownHosts) -> Self {
        Self {
            store: Arc::new(store),
            secrets,
            registry: Registry::new(),
            known_hosts: Arc::new(known_hosts),
            sessions: RwLock::new(HashMap::new()),
            transfers: Mutex::new(HashMap::new()),
        }
    }

    /// The open session `id`; counts as activity for the idle timeout.
    pub async fn session(&self, id: &str) -> DbResult<Arc<OpenSession>> {
        let open = self
            .sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DbError::not_found("The connection is closed. Connect again."))?;
        if open.tunnel.as_ref().is_some_and(|t| !t.is_open()) {
            return Err(DbError::new(ErrorKind::Network, "The SSH tunnel closed.")
                .with_action(Some("Reconnect to open a new tunnel.")));
        }
        open.touch();
        Ok(open)
    }

    /// Runs a store operation on a blocking thread so the async runtime and UI never wait on disk.
    pub async fn with_store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        let store = self.store.clone();
        blocking(move || f(&store)).await
    }

    pub async fn settings(&self) -> DbResult<Settings> {
        self.with_store(Store::settings).await
    }

    pub async fn connection(&self, id: &str) -> DbResult<SavedConnection> {
        let id = id.to_owned();
        self.with_store(move |s| s.connection(&id)).await
    }

    /// Closes sessions idle for longer than their timeout. Returns their ids.
    pub async fn close_idle(&self) -> Vec<String> {
        let now = now_ms();
        let expired: Vec<(String, Arc<OpenSession>)> = {
            let mut sessions = self.sessions.write().await;
            let ids: Vec<String> =
                sessions.iter().filter(|(_, s)| s.idle_expired(now)).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| sessions.remove(&id).map(|s| (id, s))).collect()
        };
        for (_, open) in &expired {
            open.session.close().await;
        }
        expired.into_iter().map(|(id, _)| id).collect()
    }
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> DbResult<T> + Send + 'static) -> DbResult<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| DbError::internal(format!("background task failed: {e}")))?
}
