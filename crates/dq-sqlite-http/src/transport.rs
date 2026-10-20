//! What a remote `SQLite` service offers the shared session: statements in,
//! `SQLite` values out.

use std::time::Duration;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::edit::BoundStatement;
use dq_core::sql::Dialect;
use dq_sqlite::values::Value;

use crate::http::Http;

/// A result column. Services without declared types leave `decltype` empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub decltype: String,
}

/// One statement's result as the service returned it, every row at once.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// Rows the statement changed.
    pub affected: u64,
    /// Some integer arrived rounded (D1 sends JavaScript numbers, exact only up to 2^53).
    pub imprecise: bool,
}

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Product name for [`dq_core::result::ServerInfo::version`], e.g. `libSQL`.
    fn product(&self) -> &'static str;

    /// The client requests go through, for the connection's TLS details.
    fn http(&self) -> &Http;

    fn dialect(&self) -> &'static dyn Dialect;

    /// `LIKE` prefixes of the service's internal tables, hidden in the explorer.
    fn internal_prefixes(&self) -> &'static [&'static str];

    /// Runs one statement on its own: catalog reads, table pages, exports.
    async fn query(&self, sql: &str, params: &[Option<String>], timeout: Option<Duration>) -> DbResult<Output>;

    /// Runs one statement of the query editor. A service with sessions keeps
    /// `BEGIN … COMMIT` and pragmas between them.
    async fn execute(&self, sql: &str, params: &[Option<String>], timeout: Option<Duration>) -> DbResult<Output> {
        self.query(sql, params, timeout).await
    }

    /// Runs statements all or nothing. A statement's `expect_rows` that is
    /// not met undoes every change. Returns each statement's changed rows.
    async fn transaction(&self, statements: &[BoundStatement]) -> DbResult<Vec<u64>>;

    /// Runs the statements of a script in one request that applies all or
    /// none of them, for services without interactive transactions.
    /// `None` when the service runs scripts statement by statement.
    async fn script(&self, statements: &[&str], timeout: Option<Duration>) -> Option<DbResult<Vec<Output>>> {
        let _ = (statements, timeout);
        None
    }

    /// Ends the session's server state, if any.
    async fn close(&self) {}
}
