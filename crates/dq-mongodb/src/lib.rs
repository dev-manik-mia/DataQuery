//! `MongoDB` adapter for DataQuery, on the official driver.
//!
//! The explorer goes database → collections and views. Editor statements
//! use `mongosh` syntax (`db.orders.find({ … }).sort({ … })`) parsed into
//! server commands, without running any JavaScript. Collections open in the
//! Documents view ([`dq_core::document::DocumentStore`]), where documents are
//! edited in the same shell syntax so every type and digit is kept.

mod catalog;
mod client;
mod commands;
mod explain;
pub mod literal;
mod monitor;
mod session;
pub mod shell;
mod store;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::MongoSession;
pub use store::FEATURES;

#[derive(Debug, Default, Clone, Copy)]
pub struct MongodbAdapter;

impl MongodbAdapter {
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
        transactions: true,
        schema_diff: false,
    };
}

#[async_trait]
impl DatabaseAdapter for MongodbAdapter {
    fn engine(&self) -> Engine {
        Engine::Mongodb
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = MongoSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(MongoSession::open(params).await?))
    }
}
