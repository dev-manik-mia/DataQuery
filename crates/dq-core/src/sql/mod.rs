//! SQL text utilities: lexing, statement splitting, safety analysis, quoting
//! and the statement builders shared by every SQL adapter.

pub mod analyze;
pub mod builder;
pub mod ddl;
pub mod dialect;
pub mod format;
pub mod lexer;
mod mongo;

pub use analyze::{
    DestructiveReason, StatementInfo, StatementKind, TxEffect, analyze, command_tag, named_params, redact_literals,
    split_statements, transaction_effect,
};
pub use dialect::{
    ClickhouseDialect, Dialect, DuckdbDialect, MongoDialect, MysqlDialect, OracleDialect, PartiqlDialect, PostgresDialect,
    SnowflakeDialect, SqlFlavor, SqlServerDialect,
};
