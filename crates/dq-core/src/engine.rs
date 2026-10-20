//! The engine catalog shown in the engine picker and the capability matrix.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Engine {
    Postgres,
    Mysql,
    Mariadb,
    Sqlite,
    Sqlserver,
    Oracle,
    Cockroachdb,
    Clickhouse,
    Duckdb,
    Snowflake,
    Redis,
    Valkey,
    Dragonfly,
    Mongodb,
    Dynamodb,
    Cassandra,
    Scylladb,
    Elasticsearch,
    Neo4j,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub enum Priority {
    #[serde(rename = "MVP")]
    Mvp,
    P0,
    P1,
    P2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum EngineGroup {
    Relational,
    Analytical,
    KeyValue,
    Document,
    WideColumn,
    SearchGraph,
}

/// What an adapter can do at runtime. Controls for unsupported capabilities are hidden.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct Capabilities {
    pub explorer: bool,
    pub query_editor: bool,
    pub grid_edit: bool,
    pub import_export: bool,
    pub cancel: bool,
    pub ddl: bool,
    /// Databases contain schemas (`PostgreSQL`) rather than being the schema (`MySQL`).
    pub schemas: bool,
    pub materialized_views: bool,
    pub explain: bool,
    pub monitoring: bool,
    pub transactions: bool,
    pub schema_diff: bool,
    /// Backup and restore, see [`crate::backup::BackupFamily`].
    pub backup: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct EngineInfo {
    pub engine: Engine,
    pub name: &'static str,
    /// Two-letter monogram used for the engine tile.
    pub mark: &'static str,
    /// OKLCH hue for the monogram tile.
    pub hue: u16,
    pub group: EngineGroup,
    pub language: &'static str,
    pub default_port: Option<u16>,
    /// Short description such as `5432 · SQL · v12–17`.
    pub summary: &'static str,
    pub versions: &'static str,
    pub priority: Priority,
    /// True when an adapter for this engine ships in this build.
    pub available: bool,
    /// Connects to a local file instead of host and port.
    pub file_based: bool,
    pub capabilities: Capabilities,
}

struct Spec {
    engine: Engine,
    name: &'static str,
    mark: &'static str,
    hue: u16,
    group: EngineGroup,
    language: &'static str,
    port: Option<u16>,
    summary: &'static str,
    versions: &'static str,
    priority: Priority,
}

#[allow(clippy::too_many_arguments)] // a table row, read positionally
const fn spec(
    engine: Engine,
    name: &'static str,
    mark: &'static str,
    hue: u16,
    group: EngineGroup,
    language: &'static str,
    port: Option<u16>,
    summary: &'static str,
    versions: &'static str,
    priority: Priority,
) -> Spec {
    Spec { engine, name, mark, hue, group, language, port, summary, versions, priority }
}

use EngineGroup as G;
use Priority as P;

const SPECS: &[Spec] = &[
    spec(
        Engine::Postgres,
        "PostgreSQL",
        "Pg",
        250,
        G::Relational,
        "SQL",
        Some(5432),
        "5432 · SQL · v12–17",
        "Versions 12–17 · also Timescale, Supabase, Neon",
        P::Mvp,
    ),
    spec(
        Engine::Mysql,
        "MySQL",
        "My",
        220,
        G::Relational,
        "SQL",
        Some(3306),
        "3306 · SQL · v5.7–8.4",
        "Versions 5.7–8.4",
        P::P0,
    ),
    spec(
        Engine::Mariadb,
        "MariaDB",
        "Ma",
        60,
        G::Relational,
        "SQL",
        Some(3306),
        "3306 · SQL · v10.6–11",
        "Versions 10.6–11 · MySQL adapter",
        P::P0,
    ),
    spec(
        Engine::Sqlite,
        "SQLite",
        "Sl",
        200,
        G::Relational,
        "SQL",
        None,
        "local file · SQL",
        "Local database file",
        P::P0,
    ),
    spec(
        Engine::Sqlserver,
        "SQL Server",
        "Ms",
        0,
        G::Relational,
        "T-SQL",
        Some(1433),
        "1433 · T-SQL · 2017–2022",
        "2017–2022",
        P::P1,
    ),
    spec(
        Engine::Oracle,
        "Oracle",
        "Or",
        15,
        G::Relational,
        "PL/SQL",
        Some(1521),
        "1521 · PL/SQL · 19c–23ai",
        "19c–23ai",
        P::P2,
    ),
    spec(
        Engine::Cockroachdb,
        "CockroachDB",
        "Cr",
        280,
        G::Relational,
        "SQL",
        Some(26257),
        "26257 · Postgres wire",
        "Postgres wire protocol",
        P::P2,
    ),
    spec(
        Engine::Clickhouse,
        "ClickHouse",
        "Ch",
        95,
        G::Analytical,
        "SQL",
        Some(8123),
        "8123 · SQL",
        "Read-heavy analytics",
        P::P1,
    ),
    spec(
        Engine::Duckdb,
        "DuckDB",
        "Dk",
        85,
        G::Analytical,
        "SQL",
        None,
        "local file · SQL",
        "Local files, Parquet and CSV",
        P::P2,
    ),
    spec(
        Engine::Snowflake,
        "Snowflake",
        "Sf",
        210,
        G::Analytical,
        "SQL",
        Some(443),
        "HTTPS · SQL",
        "Cloud warehouse",
        P::P2,
    ),
    spec(
        Engine::Redis,
        "Redis",
        "Rd",
        25,
        G::KeyValue,
        "Commands",
        Some(6379),
        "6379 · commands · v6–7",
        "Versions 6–7",
        P::P1,
    ),
    spec(
        Engine::Valkey,
        "Valkey",
        "Vk",
        175,
        G::KeyValue,
        "Commands",
        Some(6379),
        "6379 · Redis-compatible",
        "Redis-compatible",
        P::P2,
    ),
    spec(
        Engine::Dragonfly,
        "Dragonfly",
        "Df",
        265,
        G::KeyValue,
        "Commands",
        Some(6379),
        "6379 · Redis-compatible",
        "Redis-compatible (Dragonfly 1.x)",
        P::P2,
    ),
    spec(
        Engine::Mongodb,
        "MongoDB",
        "Mg",
        145,
        G::Document,
        "MQL",
        Some(27017),
        "27017 · MQL · v5–8",
        "Versions 5–8",
        P::P2,
    ),
    spec(
        Engine::Dynamodb,
        "DynamoDB",
        "Dy",
        330,
        G::Document,
        "PartiQL",
        Some(443),
        "AWS · PartiQL",
        "AWS or DynamoDB Local",
        P::P2,
    ),
    spec(
        Engine::Cassandra,
        "Cassandra",
        "Cs",
        300,
        G::WideColumn,
        "CQL",
        Some(9042),
        "9042 · CQL · v4–5",
        "Versions 4–5",
        P::P2,
    ),
    spec(
        Engine::Scylladb,
        "ScyllaDB",
        "Sc",
        330,
        G::WideColumn,
        "CQL",
        Some(9042),
        "9042 · CQL",
        "Cassandra adapter",
        P::P2,
    ),
    spec(
        Engine::Elasticsearch,
        "Elasticsearch",
        "Es",
        55,
        G::SearchGraph,
        "Query DSL",
        Some(9200),
        "9200 · Query DSL",
        "Also OpenSearch",
        P::P2,
    ),
    spec(
        Engine::Neo4j,
        "Neo4j",
        "N4",
        190,
        G::SearchGraph,
        "Cypher",
        Some(7474),
        "7474 · Cypher (HTTP)",
        "Graph · Neo4j 5.19+ Query API",
        P::P2,
    ),
];

/// Builds the full engine catalog. `capabilities_of` returns `Some` for engines
/// that have an adapter compiled into this build.
pub fn catalog(capabilities_of: impl Fn(Engine) -> Option<Capabilities>) -> Vec<EngineInfo> {
    SPECS
        .iter()
        .map(|s| {
            let caps = capabilities_of(s.engine);
            EngineInfo {
                engine: s.engine,
                name: s.name,
                mark: s.mark,
                hue: s.hue,
                group: s.group,
                language: s.language,
                default_port: s.port,
                summary: s.summary,
                versions: s.versions,
                priority: s.priority,
                available: caps.is_some(),
                file_based: s.engine.is_file_based(),
                capabilities: caps.unwrap_or_default(),
            }
        })
        .collect()
}

impl Engine {
    pub fn name(self) -> &'static str {
        SPECS.iter().find(|s| s.engine == self).map_or("Unknown", |s| s.name)
    }

    pub fn default_port(self) -> Option<u16> {
        SPECS.iter().find(|s| s.engine == self).and_then(|s| s.port)
    }

    /// Speaks the Redis protocol (Redis, Valkey, Dragonfly): numbered databases, the key browser.
    pub fn is_redis_like(self) -> bool {
        matches!(self, Self::Redis | Self::Valkey | Self::Dragonfly)
    }

    /// Connects to a local database file (path in `database`) instead of a server.
    pub fn is_file_based(self) -> bool {
        matches!(self, Self::Sqlite | Self::Duckdb)
    }
}
