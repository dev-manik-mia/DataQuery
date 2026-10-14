//! MySQL (5.7–8.4) and MariaDB (10.6–11) adapter for DataQuery.
//!
//! MySQL has no schemas: a database is the container for tables and views, so
//! the explorer goes database → tables / views.

mod catalog;
mod connect;
mod error;
mod session;
pub mod values;

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::{DatabaseAdapter, Session};
use dq_core::config::ConnConfig;
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
        explain: false,
        monitoring: false,
        transactions: false,
        schema_diff: false,
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

    async fn test(&self, config: &ConnConfig, password: Option<&str>) -> DbResult<ServerInfo> {
        config.validate()?;
        let session = MySession::open(config, password).await?;
        let info = session.info().clone();
        session.close().await;
        Ok(info)
    }

    async fn connect(&self, config: &ConnConfig, password: Option<&str>) -> DbResult<Arc<dyn Session>> {
        config.validate()?;
        Ok(Arc::new(MySession::open(config, password).await?))
    }
}
