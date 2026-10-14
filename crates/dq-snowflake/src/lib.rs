//! Snowflake adapter for DataQuery, over HTTPS with the REST protocol the
//! official drivers use (password authentication). Results arrive as Arrow.
//!
//! The explorer goes database → schema → tables and views. The account is
//! the host (`xy12345.eu-west-1` or `myorg-myaccount`); warehouse, role and
//! default schema are connection properties.

mod catalog;
mod client;
mod explain;
mod results;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::SfSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct SnowflakeAdapter;

impl SnowflakeAdapter {
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
        monitoring: false,
        transactions: true,
        schema_diff: true,
    };
}

#[async_trait]
impl DatabaseAdapter for SnowflakeAdapter {
    fn engine(&self) -> Engine {
        Engine::Snowflake
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = SfSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(SfSession::open(params).await?))
    }
}
