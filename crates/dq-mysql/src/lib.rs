//! MySQL (5.7–8.4) and MariaDB (10.6–11) adapter for DataQuery.
//!
//! MySQL has no schemas: a database is the container for tables and views, so
//! the explorer goes database → tables / views.

mod catalog;
mod connect;
mod error;
mod monitor;
mod session;
mod users;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::engine::{Capabilities, Engine};
use dq_core::result::ServerInfo;

pub use session::MySession;

/// One adapter serves both engines; the server family is detected on connect.
#[derive(Debug, Clone, Copy)]
pub struct MysqlAdapter {
    engine: Engine,
}

impl MysqlAdapter {
    pub const MYSQL: Self = Self { engine: Engine::Mysql };
    pub const MARIADB: Self = Self { engine: Engine::Mariadb };

    pub const CAPABILITIES: Capabilities = Capabilities {
        explorer: true,
        query_editor: true,
        grid_edit: true,
        import_export: true,
        cancel: true,
        ddl: true,
        schemas: false,
        materialized_views: false,
        explain: true,
        monitoring: true,
        transactions: true,
        schema_diff: true,
        backup: true,
    };
}

#[async_trait]
impl DatabaseAdapter for MysqlAdapter {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn capabilities(&self) -> Capabilities {
        Self::CAPABILITIES
    }

    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo> {
        params.config.validate()?;
        let session = MySession::open(params).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>> {
        params.config.validate()?;
        Ok(Arc::new(MySession::open(params).await?))
    }
}
