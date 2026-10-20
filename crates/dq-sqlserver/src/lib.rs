//! SQL Server (2017–2022, and Azure SQL) adapter for DataQuery.
//!
//! The explorer goes database → schema (`dbo`, …) → tables and views, like
//! PostgreSQL. Connections use TDS through `tiberius` with rustls.

mod catalog;
mod connect;
mod error;
mod explain;
mod monitor;
mod pool;
mod rows;
mod session;
pub mod values;
mod worker;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::MsSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct SqlServerAdapter;

impl SqlServerAdapter {
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
        backup: false,
    };
}

#[async_trait]
impl DatabaseAdapter for SqlServerAdapter {
    fn engine(&self) -> Engine {
        Engine::Sqlserver
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = MsSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(MsSession::open(params).await?))
    }
}
