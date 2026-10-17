//! `DuckDB` adapter for DataQuery: a local database file (or `:memory:`),
//! with Parquet, CSV and JSON readable straight from SQL.
//!
//! The explorer goes database (the file, plus any `ATTACH`ed ones) → schema
//! (`main`, …) → tables and views, like `PostgreSQL`.

mod catalog;
mod error;
mod explain;
mod monitor;
mod session;
pub mod values;
mod worker;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::DuckSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct DuckdbAdapter;

impl DuckdbAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: true,
        materialized_views: false,
        explain: true,
        monitoring: true,
        transactions: true,
        schema_diff: true,
    };
}

#[async_trait]
impl DatabaseAdapter for DuckdbAdapter {
    fn engine(&self) -> Engine {
        Engine::Duckdb
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = DuckSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(DuckSession::open(params).await?))
    }
}
