//! The contract every engine adapter implements (feature list §13).
//!
//! Adapters provide catalog access and a handful of execution primitives.
//! Paging, grid edits, export and import are written once in [`crate::ops`]
//! on top of these primitives.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConnConfig;
use crate::edit::BoundStatement;
use crate::engine::{Capabilities, Engine};
use crate::error::DbResult;
use crate::node::{Node, ObjectRef};
use crate::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult};
use crate::schema::TableStructure;
use crate::sql::Dialect;
use crate::value::CellLimits;

/// Receives streamed rows in batches.
pub type BatchFn<'a> = dyn FnMut(&[ColumnMeta], Vec<Row>) -> DbResult<()> + Send + 'a;

/// Statements executed inside one transaction, produced lazily.
pub type StatementIter<'a> = dyn Iterator<Item = DbResult<BoundStatement>> + Send + 'a;

/// Text parameters for a statement. `None` is SQL NULL. The server converts
/// each one with the input function of the type it infers for the placeholder.
pub type Params<'a> = &'a [Option<String>];

/// Everything an adapter needs to open connections. Secrets stay in memory.
#[derive(Clone)]
pub struct ConnectParams {
    pub config: ConnConfig,
    /// Password, or a short-lived cloud IAM token used as the password.
    pub password: Option<String>,
    /// Local end of an SSH tunnel. Sockets connect here while TLS still
    /// verifies `config.host`.
    pub endpoint: Option<SocketAddr>,
}

impl ConnectParams {
    pub fn new(config: ConnConfig, password: Option<String>) -> Self {
        Self { config, password, endpoint: None }
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Host and port to open sockets to.
    pub fn socket_target(&self) -> (String, u16) {
        self.endpoint.map_or_else(|| (self.config.host.clone(), self.config.port), |a| (a.ip().to_string(), a.port()))
    }
}

impl std::fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectParams")
            .field("config", &self.config)
            .field("password", &self.password.as_ref().map(|_| crate::redact::MASK))
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

#[async_trait]
pub trait DatabaseAdapter: Send + Sync {
    fn engine(&self) -> Engine;
    fn capabilities(&self) -> Capabilities;

    /// Connects, reads server details and disconnects.
    async fn test(&self, params: &ConnectParams) -> DbResult<ServerInfo>;

    async fn connect(&self, params: &ConnectParams) -> DbResult<Arc<dyn Session>>;
}

/// An open connection. Methods take `&self`; implementations serialise access
/// internally and keep catalog work off the connection that runs user queries,
/// so the explorer stays responsive while a query runs.
#[async_trait]
pub trait Session: Send + Sync {
    fn info(&self) -> &ServerInfo;
    fn dialect(&self) -> &'static dyn Dialect;

    /// Lazily lists children: `None` lists databases.
    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>>;

    /// Finds tables and views whose name contains `query` (case-insensitive).
    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>>;

    /// Runs one user statement on the query connection and returns at most
    /// `row_limit` rows. Remaining rows stay on the server until
    /// [`Session::fetch_more`] or the next `execute`. `params` bind the
    /// statement's placeholders (`$1` / `?`) as text.
    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult>;

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage>;

    /// Asks the server to cancel the running statement. Returns false when
    /// nothing was running.
    async fn cancel(&self) -> DbResult<bool>;

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure>;

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String>;

    /// Runs a read-only query on a catalog connection for `database` and
    /// returns every row (callers bound the query with `LIMIT`).
    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)>;

    /// Streams a read-only query in batches, for exports.
    async fn stream_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64>;

    /// Cheap row estimate from the catalog, if the engine keeps one.
    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>>;

    /// Executes statements in a single transaction on a dedicated connection.
    /// Rolls back on the first error or when a statement's `expect_rows` is not met.
    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>>;

    async fn close(&self);
}

/// Checks an affected-row count against a statement's expectation.
pub fn check_affected(statement: &BoundStatement, affected: u64) -> DbResult<()> {
    match statement.expect_rows {
        Some(expected) if expected != affected => Err(crate::DbError::new(
            crate::ErrorKind::Constraint,
            format!(
                "Expected to change {expected} row(s) but {affected} matched. Nothing was saved; the row may have been changed or deleted by someone else."
            ),
        )),
        _ => Ok(()),
    }
}
