//! Adapters compiled into this build, keyed by engine.

use std::collections::HashMap;
use std::sync::Arc;

use dq_core::adapter::DatabaseAdapter;
use dq_core::engine::{Capabilities, Engine};
use dq_core::{DbError, DbResult};

pub struct Registry(HashMap<Engine, Arc<dyn DatabaseAdapter>>);

impl Registry {
    pub fn new() -> Self {
        let adapters: Vec<(Engine, Arc<dyn DatabaseAdapter>)> = vec![
            (Engine::Postgres, Arc::new(dq_postgres::PostgresAdapter::POSTGRES)),
            (Engine::Cockroachdb, Arc::new(dq_postgres::PostgresAdapter::COCKROACH)),
            (Engine::Redshift, Arc::new(dq_postgres::PostgresAdapter::REDSHIFT)),
            (Engine::Greenplum, Arc::new(dq_postgres::PostgresAdapter::GREENPLUM)),
            (Engine::Sqlserver, Arc::new(dq_sqlserver::SqlServerAdapter)),
            (Engine::Oracle, Arc::new(dq_oracle::OracleAdapter)),
            (Engine::Clickhouse, Arc::new(dq_clickhouse::ClickhouseAdapter)),
            (Engine::Duckdb, Arc::new(dq_duckdb::DuckdbAdapter)),
            (Engine::Sqlite, Arc::new(dq_sqlite::SqliteAdapter)),
            (Engine::Snowflake, Arc::new(dq_snowflake::SnowflakeAdapter)),
            (Engine::Mongodb, Arc::new(dq_mongodb::MongodbAdapter)),
            (Engine::Dynamodb, Arc::new(dq_dynamodb::DynamodbAdapter)),
            (Engine::Redis, Arc::new(dq_redis::RedisAdapter::REDIS)),
            (Engine::Valkey, Arc::new(dq_redis::RedisAdapter::VALKEY)),
            (Engine::Dragonfly, Arc::new(dq_redis::RedisAdapter::DRAGONFLY)),
            (Engine::Cassandra, Arc::new(dq_cassandra::CassandraAdapter::CASSANDRA)),
            (Engine::Scylladb, Arc::new(dq_cassandra::CassandraAdapter::SCYLLA)),
            (Engine::Elasticsearch, Arc::new(dq_elasticsearch::ElasticsearchAdapter)),
            (Engine::Neo4j, Arc::new(dq_neo4j::Neo4jAdapter)),
            (Engine::Mysql, Arc::new(dq_mysql::MysqlAdapter::MYSQL)),
            (Engine::Mariadb, Arc::new(dq_mysql::MysqlAdapter::MARIADB)),
        ];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_capability_matches_the_backup_table() {
        let registry = Registry::new();
        for (engine, adapter) in &registry.0 {
            let family = dq_core::backup::BackupFamily::of(*engine);
            assert_eq!(adapter.capabilities().backup, family.is_some(), "{engine:?}");
        }
    }
}
