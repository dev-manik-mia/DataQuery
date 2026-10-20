//! Google `BigQuery` adapter for DataQuery, over the `BigQuery` REST API v2
//! (HTTPS with rustls). Credentials are a service-account JSON key, an
//! `authorized_user` file, or the Google Cloud CLI; an `endpoint` property
//! points at an emulator.
//!
//! The explorer goes project → dataset → tables, views, materialized views and
//! routines. The connection's project (in `database`) pays for the queries;
//! `dataset` and `location` are optional connection properties.

mod auth;
mod catalog;
mod client;
mod explain;
mod monitor;
mod results;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::BqSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct BigqueryAdapter;

impl BigqueryAdapter {
    /// No interactive transactions: a `BigQuery` transaction lives in one script
    /// (or a session), so manual-commit mode is not offered. No backups: tables
    /// keep Time Travel history and are copied with snapshots or clones.
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
        transactions: false,
        schema_diff: true,
        backup: false,
    };
}

#[async_trait]
impl DatabaseAdapter for BigqueryAdapter {
    fn engine(&self) -> Engine {
        Engine::Bigquery
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = BqSession::open(params).await?;
        Ok(session.info().clone())
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(BqSession::open(params).await?))
    }
}
