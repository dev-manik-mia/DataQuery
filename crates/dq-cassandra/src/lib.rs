//! Cassandra and `ScyllaDB` adapter for DataQuery, on the `ScyllaDB` Rust
//! driver (CQL protocol v4, rustls).
//!
//! The explorer goes keyspace → tables, materialized views, user-defined
//! types and functions. CQL is close enough to SQL that the editor, table
//! views, grid edits, imports and exports use the shared SQL paths: `?`
//! markers are typed by the prepared statement, and pages past the first are
//! read with the paging state (CQL has no `OFFSET`).

mod catalog;
pub mod client;
mod monitor;
mod session;
mod tls;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::{CONSISTENCY_LEVELS, CqlSession, PAGE_SIZES, SERIAL_LEVELS};

/// One adapter for both servers; the session reports which one it reached.
#[derive(Debug, Clone, Copy)]
pub struct CassandraAdapter {
    engine: Engine,
}

impl CassandraAdapter {
    pub const CASSANDRA: Self = Self { engine: Engine::Cassandra };
    pub const SCYLLA: Self = Self { engine: Engine::Scylladb };

    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: false,
        materialized_views: true,
        explain: true,
        monitoring: true,
        transactions: false,
        schema_diff: true,
    };
}

#[async_trait]
impl DatabaseAdapter for CassandraAdapter {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = CqlSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(CqlSession::open(params).await?))
    }
}
