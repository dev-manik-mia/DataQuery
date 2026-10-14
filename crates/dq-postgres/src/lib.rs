//! PostgreSQL adapter for DataQuery (versions 12–17, plus wire-compatible
//! services such as Supabase, Neon and Timescale). It also serves `CockroachDB`,
//! which speaks the PostgreSQL protocol; the few catalog differences are
//! handled where they occur (see `connect::Flavor`).

mod catalog;
mod connect;
mod error;
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

pub use session::PgSession;

/// One adapter serves both engines; the server family is detected on connect.
#[derive(Debug, Clone, Copy)]
pub struct PostgresAdapter {
    engine: Engine,
}

impl Default for PostgresAdapter {
    fn default() -> Self {
        Self::POSTGRES
    }
}

impl PostgresAdapter {
    pub const POSTGRES: Self = Self { engine: Engine::Postgres };
    pub const COCKROACH: Self = Self { engine: Engine::Cockroachdb };

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
        self.engine
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
