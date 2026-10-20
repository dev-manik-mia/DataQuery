//! Elasticsearch and `OpenSearch` adapter for DataQuery, over the REST API.
//!
//! The explorer shows the cluster as the database, with its indices, data
//! streams and aliases. The editor runs requests in Kibana Dev Tools form
//! (`GET /orders/_search` followed by a JSON body). Indices open in the
//! Documents view, where documents can be added, replaced and deleted.

mod catalog;
pub mod client;
mod explain;
mod monitor;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::{EsSession, FEATURES};

#[derive(Debug, Default, Clone, Copy)]
pub struct ElasticsearchAdapter;

impl ElasticsearchAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: false,
        schemas: false,
        materialized_views: false,
        explain: true,
        monitoring: true,
        transactions: false,
        schema_diff: false,
        backup: false,
    };
}

#[async_trait]
impl DatabaseAdapter for ElasticsearchAdapter {
    fn engine(&self) -> Engine {
        Engine::Elasticsearch
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        Ok(EsSession::open(params).await?.info().clone())
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(EsSession::open(params).await?))
    }
}
