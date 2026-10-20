//! `SQLite` adapter for DataQuery: a local database file (or `:memory:`).
//!
//! The explorer goes database (`main`, plus any `ATTACH`ed ones) → tables
//! and views → indexes and triggers, like `MySQL`. The catalog, value and plan
//! code is shared with the remote `SQLite` services in `dq-sqlite-http`.

mod backup;
pub mod catalog;
pub mod error;
pub mod explain;
mod session;
pub mod values;
mod worker;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::SqliteSession;

/// Statements whose changed-row count is reported (`INSERT`, `UPDATE`, `DELETE`, upserts).
pub fn counts_rows(sql: &str) -> bool {
    use dq_core::sql::{SqlFlavor, StatementKind, analyze};
    analyze(sql, SqlFlavor::Sqlite).is_some_and(|s| {
        matches!(s.kind, StatementKind::Insert | StatementKind::Update | StatementKind::Delete | StatementKind::Merge)
    })
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteAdapter;

impl SqliteAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: false,
        materialized_views: false,
        explain: true,
        // An embedded file: there is no server with other sessions to watch.
        monitoring: false,
        transactions: true,
        schema_diff: true,
        backup: true,
    };
}

#[async_trait]
impl DatabaseAdapter for SqliteAdapter {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = SqliteSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(SqliteSession::open(params).await?))
    }
}
