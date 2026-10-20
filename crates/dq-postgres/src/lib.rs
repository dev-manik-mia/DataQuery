//! PostgreSQL adapter for DataQuery (versions 12–17, plus wire-compatible
//! services such as Supabase, Neon and Timescale). It also serves the engines
//! that speak the PostgreSQL protocol: `CockroachDB`, Greenplum (and Apache
//! Cloudberry) and Amazon Redshift. Their differences are answered by
//! `connect::Flavor`; Redshift's own catalog lives in `redshift`.

mod catalog;
mod connect;
mod error;
mod monitor;
mod redshift;
mod session;
mod tls;
mod users;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::PgSession;

/// One adapter serves every PostgreSQL-protocol engine; the server family is detected on connect.
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
    pub const REDSHIFT: Self = Self { engine: Engine::Redshift };
    pub const GREENPLUM: Self = Self { engine: Engine::Greenplum };

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
        backup: true,
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
        // CockroachDB and Redshift reject pg_dump (see `dq_core::backup::BackupFamily::of`).
        let backup = dq_core::backup::BackupFamily::of(self.engine).is_some();
        // Redshift's ALTER TABLE cannot change nullability, defaults or most
        // column types, so most of a schema diff could not be applied.
        let schema_diff = self.engine != Engine::Redshift;
        Capabilities { backup, schema_diff, ..Self::CAPABILITIES }
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
