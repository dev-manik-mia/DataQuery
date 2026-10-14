//! Oracle (19c–23ai) adapter for DataQuery.
//!
//! The explorer goes service (pluggable database) → schema (user) → tables,
//! views and materialized views. The driver uses Oracle's client library
//! (Instant Client), loaded at run time.

mod catalog;
mod connect;
mod error;
mod explain;
mod monitor;
mod pool;
mod rows;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::OraSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct OracleAdapter;

impl OracleAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: true,
        materialized_views: true,
        explain: true,
        monitoring: true,
        transactions: true,
        schema_diff: true,
    };
}

#[async_trait]
impl DatabaseAdapter for OracleAdapter {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = OraSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(OraSession::open(params).await?))
    }
}
