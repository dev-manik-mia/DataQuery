//! Redis, Valkey and Dragonfly adapter for DataQuery (standalone servers, RESP2).
//!
//! The explorer lists databases `0…n-1`; keys are browsed with `SCAN` in the
//! key browser ([`dq_core::keyvalue::KeyValueStore`]). The editor and the
//! console run one command per line on a dedicated connection.

mod client;
mod monitor;
mod reply;
mod session;
mod store;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::RedisSession;

/// One adapter for the Redis protocol: Redis, Valkey and Dragonfly.
#[derive(Debug, Clone, Copy)]
pub struct RedisAdapter {
    engine: Engine,
}

impl RedisAdapter {
    pub const REDIS: Self = Self { engine: Engine::Redis };
    pub const VALKEY: Self = Self { engine: Engine::Valkey };
    pub const DRAGONFLY: Self = Self { engine: Engine::Dragonfly };

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
        monitoring: true,
        transactions: true,
        schema_diff: false,
    };
}

#[async_trait]
impl DatabaseAdapter for RedisAdapter {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = RedisSession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(RedisSession::open(params).await?))
    }
}
