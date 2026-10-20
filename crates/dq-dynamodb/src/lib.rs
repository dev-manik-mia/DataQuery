//! `DynamoDB` adapter for DataQuery, over the JSON API with `SigV4` signing.
//!
//! The explorer shows the region as the database and its tables below it.
//! The editor runs `PartiQL`; the Documents view reads items with `PartiQL`
//! too and writes single items with conditional `PutItem` / `DeleteItem`.
//! Credentials are an access key ID (username) and secret access key
//! (password), or the JSON that `aws configure export-credentials --format
//! process` prints. The region is the `region` property; an empty host means
//! the AWS endpoint of that region, any other host (e.g. `DynamoDB` Local) is
//! used as given.

mod catalog;
pub mod client;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::DdbSession;

#[derive(Debug, Default, Clone, Copy)]
pub struct DynamodbAdapter;

impl DynamodbAdapter {
    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: false,
        schemas: false,
        materialized_views: false,
        explain: false,
        monitoring: false,
        transactions: true,
        schema_diff: false,
        backup: false,
    };
}

#[async_trait]
impl DatabaseAdapter for DynamodbAdapter {
    fn engine(&self) -> Engine {
        Engine::Dynamodb
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = DdbSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(DdbSession::open(params).await?))
    }
}
