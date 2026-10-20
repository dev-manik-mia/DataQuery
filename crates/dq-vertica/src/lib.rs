//! Vertica adapter for DataQuery, with its own client for
//! Vertica's protocol: startup, TLS, MD5 and SHA-512 password hashes, simple
//! and extended queries with typed parameters, text-format rows and cancel
//! requests. No maintained Rust driver exists, and PostgreSQL drivers cannot
//! complete Vertica's startup.
//!
//! A cluster serves one database, so the explorer goes database → schema →
//! tables, views, sequences and functions; projections show as the indexes
//! of a table.

mod catalog;
mod conn;
mod error;
mod monitor;
mod protocol;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::VtSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct VerticaAdapter;

impl VerticaAdapter {
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
impl DatabaseAdapter for VerticaAdapter {
    fn engine(&self) -> Engine {
        Engine::Vertica
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = VtSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(VtSession::open(params).await?))
    }
}
