//! Application state shared by all commands.

use std::collections::HashMap;
use std::sync::Arc;

use rb_core::adapter::Session;
use rb_core::{DbError, DbResult};
use rb_store::{SavedConnection, SecretStore, Settings, Store};
use tokio::sync::RwLock;

use crate::registry::Registry;

pub struct OpenSession {
    pub connection_id: String,
    pub session: Arc<dyn Session>,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub registry: Registry,
    pub sessions: RwLock<HashMap<String, Arc<OpenSession>>>,
}

impl AppState {
    pub fn new(store: Store, secrets: Arc<dyn SecretStore>) -> Self {
        Self { store: Arc::new(store), secrets, registry: Registry::new(), sessions: RwLock::new(HashMap::new()) }
    }

    pub async fn session(&self, id: &str) -> DbResult<Arc<OpenSession>> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DbError::not_found("The connection is closed. Connect again."))
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

    /// Reads a password from the keychain on a blocking thread (it may prompt).
    pub async fn password(&self, connection_id: &str) -> DbResult<Option<String>> {
        let secrets = self.secrets.clone();
        let id = connection_id.to_owned();
        blocking(move || secrets.get(&id)).await
    }
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> DbResult<T> + Send + 'static) -> DbResult<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| DbError::internal(format!("background task failed: {e}")))?
}
