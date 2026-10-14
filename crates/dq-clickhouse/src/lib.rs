//! `ClickHouse` adapter for DataQuery, over the HTTP interface (port 8123,
//! or 8443 with TLS). Read-heavy: rows are not edited from the grid.
//!
//! The explorer goes database → tables, views and materialized views; a
//! database is the container, as in `MySQL`.

mod catalog;
mod client;
mod monitor;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::ChSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct ClickhouseAdapter;

impl ClickhouseAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: false,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: false,
        materialized_views: true,
        explain: true,
        monitoring: true,
        transactions: false,
        schema_diff: false,
    };
}

#[async_trait]
impl DatabaseAdapter for ClickhouseAdapter {
    fn engine(&self) -> Engine {
        Engine::Clickhouse
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = ChSession::open(params).await?;
        Ok(session.info().clone())
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(ChSession::open(params).await?))
    }
}
