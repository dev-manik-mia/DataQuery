//! Adapters compiled into this build, keyed by engine.

use std::collections::HashMap;
use std::sync::Arc;

use dq_core::adapter::DatabaseAdapter;
use dq_core::engine::{Capabilities, Engine};
use dq_core::{DbError, DbResult};

pub struct Registry(HashMap<Engine, Arc<dyn DatabaseAdapter>>);

impl Registry {
    pub fn new() -> Self {
        let adapters: Vec<(Engine, Arc<dyn DatabaseAdapter>)> =
            vec![(Engine::Postgres, Arc::new(dq_postgres::PostgresAdapter))];
        Self(adapters.into_iter().collect())
    }

    pub fn get(&self, engine: Engine) -> DbResult<Arc<dyn DatabaseAdapter>> {
        self.0
            .get(&engine)
            .cloned()
            .ok_or_else(|| DbError::unsupported(format!("{} is not available in this build yet.", engine.name())))
    }

    pub fn capabilities(&self, engine: Engine) -> Option<Capabilities> {
        self.0.get(&engine).map(|a| a.capabilities())
    }
}
