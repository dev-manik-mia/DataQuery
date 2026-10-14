//! PostgreSQL adapter for DataQuery (versions 12–17, plus wire-compatible
//! services such as Supabase, Neon and Timescale).

mod catalog;
mod connect;
mod error;
mod session;
mod tls;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::PgSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct PostgresAdapter;

impl PostgresAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: true,
        materialized_views: true,
        explain: false,
        monitoring: false,
        transactions: false,
        schema_diff: false,
    };
}

impl PostgresAdapter {
    async fn open(params: &ConnectParams) -> DbResult<PgSession> {
        let config = &params.config;
        config.validate()?;
        PgSession::open(params, tls::connector(config.ssl_mode, &config.tls_files)?).await
    }
}

#[async_trait]
impl DatabaseAdapter for PostgresAdapter {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        let session = Self::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        Ok(Arc::new(Self::open(params).await?))
    }
}
