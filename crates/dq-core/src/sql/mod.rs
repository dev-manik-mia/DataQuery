//! SQL text utilities: lexing, statement splitting, safety analysis, quoting
//! and the statement builders shared by every SQL adapter.

pub mod analyze;
pub mod builder;
pub mod cql;
pub mod cypher;
pub mod ddl;
pub mod dialect;
pub mod format;
pub mod lexer;
mod mongo;
pub mod querydsl;
pub mod redis;
pub mod table_actions;
pub mod users;

pub use analyze::{
    DestructiveReason, StatementInfo, StatementKind, TxEffect, analyze, bind_named, command_tag, named_params,
    redact_literals, split_statements, transaction_effect,
};
pub use dialect::{
    BigQueryDialect, ClickhouseDialect, CqlDialect, CypherDialect, D1Dialect, Dialect, DuckdbDialect, MongoDialect,
    MysqlDialect, OracleDialect, PartiqlDialect, PostgresDialect, QueryDslDialect, RedisDialect, RedshiftDialect,
    SnowflakeDialect, SqlFlavor, SqlServerDialect, SqliteDialect, VerticaDialect,
};
