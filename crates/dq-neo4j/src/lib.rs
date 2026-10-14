//! Neo4j adapter for DataQuery, over Neo4j's HTTP Query API (`/db/{db}/query/v2`,
//! Neo4j 5.19+; explicit transactions need 5.26 or later).
//!
//! The explorer goes database → labels (as tables), relationship types (as
//! views) and indexes. The editor runs Cypher with `$name` parameters,
//! `EXPLAIN`/`PROFILE` plans and explicit transactions (`BEGIN` … `COMMIT`).
//! Labels and relationship types open read-only in the Documents view.

mod catalog;
mod client;
mod monitor;
mod plan;
mod session;
mod store;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::Neo4jSession;
pub use store::FEATURES;

#[derive(Debug, Default, Clone, Copy)]
pub struct Neo4jAdapter;

impl Neo4jAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: false,
        import_export: true,
        cancel: true,
        ddl: false,
        schemas: false,
        materialized_views: false,
        explain: true,
        monitoring: true,
        transactions: true,
        schema_diff: false,
    };
}

#[async_trait]
impl DatabaseAdapter for Neo4jAdapter {
    fn engine(&self) -> Engine {
        Engine::Neo4j
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = Neo4jSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Neo4jSession::open(params).await?)
    }
}
