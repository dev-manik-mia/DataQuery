//! Remote `SQLite` adapters for DataQuery: libSQL (Turso, self-hosted
//! `sqld`) over Hrana HTTP, and Cloudflare D1 through the Cloudflare API.
//!
//! Both share one session ([`session::RemoteSession`]) on top of a
//! [`transport::Transport`], and `dq-sqlite`'s catalog, values and plans:
//! the databases are `SQLite`, only the way statements travel differs.

pub mod d1;
mod http;
pub mod libsql;
mod session;
mod transport;
mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::RemoteSession;

/// Explorer, editor, edits, transfers, plans and schema diff work as on a local file.
const SQLITE_OVER_HTTP: Capabilities = Capabilities {
    explorer: true,
    query_editor: true,
    grid_edit: true,
    import_export: true,
    cancel: true,
    ddl: true,
    schemas: false,
    materialized_views: false,
    explain: true,
    // The services expose no sessions or running statements.
    monitoring: false,
    transactions: true,
    schema_diff: true,
    // No page-level copy over HTTP; see the capability matrix for each service's own tools.
    backup: false,
};

async fn test(session: Arc<dyn Session>) -> ServerInfo {
    let info = session.info().clone();
    session.close().await;
    info
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LibsqlAdapter;

impl LibsqlAdapter {
    pub const CAPABILITIES: Capabilities = SQLITE_OVER_HTTP;
}

#[async_trait]
impl DatabaseAdapter for LibsqlAdapter {
    fn engine(&self) -> Engine {
        Engine::Libsql
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        Ok(test(self.connect(params).await?).await)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(RemoteSession::open(libsql::Libsql::connect(params)?, params).await?))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct D1Adapter;

impl D1Adapter {
    /// Each API request is its own transaction: there is no `BEGIN … COMMIT`
    /// to hold open, so the manual-commit mode is off.
    pub const CAPABILITIES: Capabilities = Capabilities { transactions: false, ..SQLITE_OVER_HTTP };
}

#[async_trait]
impl DatabaseAdapter for D1Adapter {
    fn engine(&self) -> Engine {
        Engine::D1
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        Ok(test(self.connect(params).await?).await)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(RemoteSession::open(d1::D1::connect(params).await?, params).await?))
    }

    async fn databases(&self, params: &ConnectParams) -> DbResult<Vec<String>> {
        Ok(d1::D1::databases(params).await?.into_iter().map(|d| d.name).collect())
    }
}
