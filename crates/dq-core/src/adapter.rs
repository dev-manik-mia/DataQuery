//! The contract every engine adapter implements (feature list §13).
//!
//! Adapters provide catalog access and a handful of execution primitives.
//! Paging, grid edits, export and import are written once in [`crate::ops`]
//! on top of these primitives.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::backup::NativeBackup;
use crate::config::ConnConfig;
use crate::document::DocumentStore;
use crate::edit::BoundStatement;
use crate::engine::{Capabilities, Engine};
use crate::error::{DbError, DbResult};
use crate::explain::{ExplainDialect, ExplainResult};
use crate::keyvalue::KeyValueStore;
use crate::monitor::Monitor;
use crate::node::{Node, ObjectRef};
use crate::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult};
use crate::schema::{Dependencies, KeyRole, SchemaModel, TableStructure};
use crate::sql::Dialect;
use crate::users::UserAdmin;
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

    /// Definition of any object with [`NodeKind::has_definition`](crate::node::NodeKind::has_definition).
    async fn ddl(&self, object: &ObjectRef) -> DbResult<String>;

    /// What a table or view depends on and what depends on it.
    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies>;

    /// Tables, columns and foreign keys of `database`: one schema, or every
    /// user schema when `schema` is `None`. Bounded by [`SchemaModel::MAX_TABLES`].
    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel>;

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

    /// Plans one statement. `analyze` runs it; callers only allow that for
    /// read-only statements. The default runs the engine's `EXPLAIN` on the
    /// query connection.
    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        let dialect = ExplainDialect::new(self.dialect().flavor(), &self.info().version)
            .ok_or_else(|| DbError::unsupported("Query plans are not available for this engine."))?;
        if analyze && !dialect.analyzes() {
            return Err(DbError::unsupported(format!(
                "{} cannot run a statement to measure its plan (no EXPLAIN ANALYZE); Explain shows the estimated plan.",
                self.info().engine.name()
            )));
        }
        let options = ExecOptions { row_limit: 10_000, timeout_secs };
        let result = self.execute(&dialect.statement(sql, analyze), params, options).await?;
        let raw = result
            .rows
            .iter()
            .map(|row| row.iter().filter_map(crate::value::Cell::to_text).collect::<Vec<_>>().join("\t"))
            .collect::<Vec<_>>()
            .join("\n");
        dialect.parse(raw, analyze)
    }

    /// Server monitoring, when the engine supports it.
    fn monitor(&self) -> Option<&dyn Monitor> {
        None
    }

    /// User and role management, when the engine supports it.
    fn users(&self) -> Option<&dyn UserAdmin> {
        None
    }

    /// Document operations, for document engines (`MongoDB`, `DynamoDB`).
    /// Their tables open in the Documents view, and table exports and imports
    /// go through these instead of generated SQL.
    fn documents(&self) -> Option<&dyn DocumentStore> {
        None
    }

    /// Partition and clustering columns of a table, in key order (CQL engines).
    /// Empty for engines without them.
    async fn key_roles(&self, object: &ObjectRef) -> DbResult<Vec<KeyRole>> {
        let _ = object;
        Ok(Vec::new())
    }

    /// Settings of the query connection the editor can change, e.g. CQL consistency.
    fn options(&self) -> Vec<SessionOption> {
        Vec::new()
    }

    /// Changes one of [`Session::options`] for the following statements.
    async fn set_option(&self, name: &str, value: &str) -> DbResult<()> {
        let _ = value;
        Err(DbError::unsupported(format!("This connection has no setting `{name}`.")))
    }

    /// Key browser operations, for key-value engines (Redis, Valkey, Dragonfly).
    fn keys(&self) -> Option<&dyn KeyValueStore> {
        None
    }

    /// Backup and restore without external tools, for embedded engines (`SQLite`).
    fn native_backup(&self) -> Option<&dyn NativeBackup> {
        None
    }

    async fn close(&self);
}

/// A query-connection setting shown in the editor toolbar.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SessionOption {
    pub name: String,
    pub label: String,
    pub value: String,
    /// Allowed values, in display order.
    pub choices: Vec<String>,
}

/// One connection kept between uses, e.g. by [`Session::run_in_transaction`],
/// so back-to-back transactions (grid saves, import chunks) skip the connect
/// handshake. Callers take it out while they use it (a concurrent caller opens
/// its own) and put it back only after a clean finish, so a connection in an
/// unknown state is dropped, never reused. One idle longer than
/// [`IdleConnection::MAX_IDLE`] is dropped too, before a server or network
/// idle timeout can have closed it unnoticed.
pub struct IdleConnection<C> {
    slot: Mutex<Option<(String, Instant, C)>>,
}

impl<C> Default for IdleConnection<C> {
    fn default() -> Self {
        Self { slot: Mutex::new(None) }
    }
}

impl<C> IdleConnection<C> {
    pub const MAX_IDLE: Duration = Duration::from_secs(60);

    /// The kept connection, if it is for `database` and still fresh. A kept
    /// connection that does not qualify is closed.
    pub fn take(&self, database: &str) -> Option<C> {
        let (db, idle_since, conn) = self.lock().take()?;
        (db == database && idle_since.elapsed() < Self::MAX_IDLE).then_some(conn)
    }

    /// Keeps `conn`, opened on `database`, for the next caller, replacing any kept connection.
    pub fn put(&self, database: &str, conn: C) {
        // The replaced connection closes after the lock is released.
        let _replaced = self.lock().replace((database.to_owned(), Instant::now(), conn));
    }

    /// Removes the kept connection, e.g. to close it with the session.
    pub fn clear(&self) -> Option<C> {
        self.lock().take().map(|(_, _, conn)| conn)
    }

    fn lock(&self) -> MutexGuard<'_, Option<(String, Instant, C)>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Checks an affected-row count against a statement's expectation.
pub fn check_affected(statement: &BoundStatement, affected: u64) -> DbResult<()> {
    match statement.expect_rows {
        Some(expected) if expected != affected => Err(DbError::new(
            crate::ErrorKind::Constraint,
            format!(
                "Expected to change {expected} row(s) but {affected} matched. Nothing was saved; the row may have been changed or deleted by someone else."
            ),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_connection_is_reused_only_on_its_database() {
        let idle = IdleConnection::default();
        assert_eq!(idle.take("a"), None);
        idle.put("a", 1);
        assert_eq!(idle.take("a"), Some(1));
        assert_eq!(idle.take("a"), None, "taken out while in use");
        idle.put("a", 2);
        assert_eq!(idle.take("b"), None);
        assert_eq!(idle.take("a"), None, "a connection for another database is closed");
        idle.put("a", 3);
        idle.put("b", 4);
        assert_eq!(idle.clear(), Some(4), "the newest replaces the kept one");
        assert_eq!(idle.take("a"), None);
    }
}
