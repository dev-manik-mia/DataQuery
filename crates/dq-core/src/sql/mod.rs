//! SQL text utilities: lexing, statement splitting, safety analysis, quoting
//! and the statement builders shared by every SQL adapter.

pub mod analyze;
pub mod builder;
pub mod dialect;
pub mod format;
pub mod lexer;

pub use analyze::{
    DestructiveReason, StatementInfo, StatementKind, TxEffect, analyze, command_tag, redact_literals, split_statements,
    transaction_effect,
};
pub use dialect::{Dialect, MysqlDialect, PostgresDialect, SqlFlavor};
