//! PostgreSQL adapter for RustBase (versions 12–17, plus wire-compatible
//! services such as Supabase, Neon and Timescale).

mod catalog;
mod connect;
mod error;
mod session;
mod tls;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use rb_core::DbResult;
use rb_core::adapter::{DatabaseAdapter, Session};
use rb_core::config::ConnConfig;
use rb_core::engine::{Capabilities, Engine};
use rb_core::result::ServerInfo;

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

#[async_trait]
impl DatabaseAdapter for PostgresAdapter {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, config: &ConnConfig, password: Option<&str>) -> DbResult<ServerInfo> {
        config.validate()?;
        let session = PgSession::open(config, password, tls::connector(config.ssl_mode)?).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, config: &ConnConfig, password: Option<&str>) -> DbResult<Arc<dyn Session>> {
        config.validate()?;
        Ok(Arc::new(PgSession::open(config, password, tls::connector(config.ssl_mode)?).await?))
    }
}
