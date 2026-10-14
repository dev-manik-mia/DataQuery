//! SQL text utilities: lexing, statement splitting, safety analysis, quoting
//! and the statement builders shared by every SQL adapter.

pub mod analyze;
pub mod builder;
pub mod dialect;
pub mod format;
pub mod lexer;

pub use analyze::{DestructiveReason, StatementInfo, StatementKind, analyze, command_tag, split_statements};
pub use dialect::{Dialect, MysqlDialect, PostgresDialect, SqlFlavor};
