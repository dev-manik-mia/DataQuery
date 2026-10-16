//! An open `SQLite` database file (or in-memory database).
//!
//! User statements run on a connection owned by a worker thread (see
//! [`crate::worker`]). Catalog reads, table pages and exports use a second
//! connection to the same file, and grid edits and imports a third, so the
//! explorer keeps working while a query runs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, SqlFlavor, SqliteDialect, command_tag, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use tokio::sync::{mpsc, oneshot};

use crate::catalog;
use crate::error::map_error;
use crate::values::{self, column_meta};
use crate::worker::{self, Command, Interrupter};

const STREAM_BATCH: usize = 2_048;
/// How long a connection waits for another one's write lock before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SqliteSession {
    info: ServerInfo,
    read_only: bool,
    worker: mpsc::Sender<Command>,
    interrupter: Arc<Interrupter>,
    catalog: Arc<Mutex<Connection>>,
    writer: Arc<Mutex<Connection>>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// Where the three connections open: the file, or one shared in-memory database.
fn location(params: &ConnectParams) -> DbResult<(String, OpenFlags)> {
    let path = params.config.database.trim();
    let base = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if path == ":memory:" {
        let name = format!("file:dataquery-{}?mode=memory&cache=shared", uuid::Uuid::new_v4());
        return Ok((name, base | OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE));
    }
    let file = std::path::Path::new(path);
    if params.config.read_only {
        if !file.exists() {
            return Err(DbError::new(ErrorKind::Storage, format!("{path} does not exist.")));
        }
        return Ok((path.to_owned(), base | OpenFlags::SQLITE_OPEN_READ_ONLY));
    }
    if !file.parent().is_some_and(|p| p.as_os_str().is_empty() || p.exists()) {
        return Err(DbError::new(ErrorKind::Storage, format!("The folder of {path} does not exist.")));
    }
    Ok((path.to_owned(), base | OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE))
}

fn open_connection(path: &str, flags: OpenFlags, read_only: bool) -> DbResult<Connection> {
    let conn = Connection::open_with_flags(path, flags).map_err(|e| map_error(&e))?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| map_error(&e))?;
    // Enforce foreign keys like most clients do; `query_only` backs up the read-only flag.
    let setup =
        if read_only { "PRAGMA foreign_keys = ON; PRAGMA query_only = ON;" } else { "PRAGMA foreign_keys = ON;" };
    conn.execute_batch(setup).map_err(|e| map_error(&e))?;
    Ok(conn)
}

/// `ATTACH` or `DETACH` statements, which change what a connection can see.
fn attaches(sql: &str) -> bool {
    matches!(command_tag(sql, SqlFlavor::Sqlite).as_str(), "ATTACH" | "DETACH")
}

/// Runs `work` with a connection on a blocking thread.
async fn blocking<T: Send + 'static>(
    conn: &Arc<Mutex<Connection>>,
    work: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
) -> DbResult<T> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|_| DbError::internal("connection lock poisoned"))?;
        work(&conn)
    })
    .await
    .map_err(|e| DbError::internal(format!("database task failed: {e}")))?
}

/// Reads a query in batches on `conn`, handing each to `send`.
fn read_blocking(
    conn: &Connection,
    sql: &str,
    params: &[Option<String>],
    limits: CellLimits,
    mut send: impl FnMut(&[ColumnMeta], Vec<CellRow>) -> bool,
) -> DbResult<()> {
    let mut statement = conn.prepare(sql).map_err(|e| map_error(&e))?;
    let meta = column_meta(&statement);
    let declared: Vec<String> = meta.iter().map(|c| c.type_name.clone()).collect();
    let mut rows = statement.query(params_from_iter(params.iter())).map_err(|e| map_error(&e))?;
    let mut batch = Vec::with_capacity(STREAM_BATCH);
    while let Some(row) = rows.next().map_err(|e| map_error(&e))? {
        batch.push(values::row(row, &declared, limits).map_err(|e| map_error(&e))?);
        if batch.len() >= STREAM_BATCH && !send(&meta, std::mem::take(&mut batch)) {
            return Ok(());
        }
    }
    send(&meta, batch);
    Ok(())
}

impl SqliteSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let (path, flags) = location(params)?;
        let read_only = params.config.read_only;
        let engine = params.config.engine;
        let (conn, catalog, writer, info) = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let conn = open_connection(&path, flags, read_only)?;
            let version: String =
                conn.query_row("SELECT sqlite_version()", [], |r| r.get(0)).map_err(|e| map_error(&e))?;
            // Reading the schema here reports a non-database file now rather than on the first query.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
                .map_err(|e| map_error(&e))?;
            let catalog = open_connection(&path, flags, read_only)?;
            let writer = open_connection(&path, flags, read_only)?;
            let info = ServerInfo {
                engine,
                version: format!("SQLite {version}"),
                latency_ms: started.elapsed().as_millis() as u64,
                tls: None,
                user: String::new(),
                database: "main".into(),
                read_only,
            };
            Ok::<_, DbError>((conn, catalog, writer, info))
        })
        .await
        .map_err(|e| DbError::internal(format!("open task failed: {e}")))??;
        let (worker, interrupter) = worker::spawn(conn);
        Ok(Self {
            read_only,
            info,
            worker,
            interrupter,
            catalog: Arc::new(Mutex::new(catalog)),
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    async fn request<T>(&self, make: impl FnOnce(oneshot::Sender<DbResult<T>>) -> Command) -> DbResult<T> {
        let (tx, rx) = oneshot::channel();
        self.worker.send(make(tx)).await.map_err(|_| DbError::invalid("The session is closed."))?;
        rx.await.map_err(|_| DbError::internal("the query thread stopped"))?
    }

    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        let (tx, mut rx) = mpsc::channel::<(Vec<ColumnMeta>, Vec<CellRow>)>(2);
        let work = blocking(&self.catalog, move |conn| {
            read_blocking(conn, &sql, &params, limits, |meta, rows| tx.blocking_send((meta.to_vec(), rows)).is_ok())
        });
        let mut total = 0u64;
        let mut failure = None;
        let consume = async {
            while let Some((meta, rows)) = rx.recv().await {
                total += rows.len() as u64;
                if let Err(e) = on_batch(&meta, rows) {
                    failure = Some(e);
                    break;
                }
            }
            drop(rx);
        };
        let (result, ()) = tokio::join!(work, consume);
        result?;
        failure.map_or(Ok(total), Err)
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let parent = parent.cloned();
        blocking(&self.catalog, move |conn| match parent {
            None => catalog::databases(conn),
            Some(p) if p.kind == NodeKind::Database => catalog::relations(conn, &p.name),
            Some(p) if p.kind == NodeKind::Table => catalog::table_children(conn, &p),
            Some(_) => Ok(Vec::new()),
        })
        .await
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let query = query.to_owned();
        blocking(&self.catalog, move |conn| catalog::search(conn, &query, limit)).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.read_only && split_statements(sql, SqlFlavor::Sqlite).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        let result = self
            .request(|reply| Command::Execute { sql: sql.to_owned(), params: params.to_vec(), options, reply })
            .await?;
        // Attached databases belong to a connection: the explorer and edits need them too.
        if attaches(sql) {
            for conn in [&self.catalog, &self.writer] {
                let (sql, params) = (sql.to_owned(), params.to_vec());
                blocking(conn, move |c| {
                    c.execute(&sql, params_from_iter(params.iter())).map(|_| ()).map_err(|e| map_error(&e))
                })
                .await?;
            }
        }
        Ok(result)
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        self.request(|reply| Command::FetchMore { limit, reply }).await
    }

    async fn cancel(&self) -> DbResult<bool> {
        Ok(self.interrupter.cancel())
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        let object = object.clone();
        blocking(&self.catalog, move |conn| catalog::structure(conn, &object)).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        let object = object.clone();
        blocking(&self.catalog, move |conn| catalog::ddl(conn, &object)).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        let object = object.clone();
        blocking(&self.catalog, move |conn| catalog::dependencies(conn, &object)).await
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.clone() } else { database.to_owned() };
        blocking(&self.catalog, move |conn| catalog::schema_model(conn, &database)).await
    }

    async fn read_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        self.read_batches(sql, params, limits, &mut |meta, batch| {
            if columns.is_empty() {
                columns = meta.to_vec();
            }
            rows.extend(batch);
            Ok(())
        })
        .await?;
        Ok((columns, rows))
    }

    async fn stream_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        self.read_batches(sql, params, CellLimits::FULL, on_batch).await
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        let object = object.clone();
        blocking(&self.catalog, move |conn| Ok(catalog::row_estimate(conn, &object))).await
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let statements: Vec<_> = statements.collect::<DbResult<Vec<_>>>()?;
        blocking(&self.writer, move |conn| {
            // IMMEDIATE takes the write lock up front, so a busy file fails before any change.
            conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| map_error(&e))?;
            let outcome = (|| {
                let mut counts = Vec::with_capacity(statements.len());
                for statement in &statements {
                    let affected = conn
                        .execute(&statement.sql, params_from_iter(statement.params.iter()))
                        .map_err(|e| map_error(&e))? as u64;
                    check_affected(statement, affected)?;
                    counts.push(affected);
                }
                conn.execute_batch("COMMIT").map_err(|e| map_error(&e))?;
                Ok(counts)
            })();
            if outcome.is_err() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            outcome
        })
        .await
    }

    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        _timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        if analyze {
            return Err(DbError::unsupported(
                "SQLite has no EXPLAIN ANALYZE. Run the query to see its time, or use Explain for the plan.",
            ));
        }
        let statement = format!("EXPLAIN QUERY PLAN {}", sql.trim().trim_end_matches(';'));
        let params = params.to_vec();
        blocking(&self.catalog, move |conn| {
            let mut prepared = conn.prepare(&statement).map_err(|e| map_error(&e))?;
            let rows = prepared
                .query_map(params_from_iter(params.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))
                .map_err(|e| map_error(&e))?
                .collect::<Result<Vec<(i64, i64, String)>, _>>()
                .map_err(|e| map_error(&e))?;
            Ok(crate::explain::plan(rows))
        })
        .await
    }

    async fn close(&self) {
        let _ = self.interrupter.cancel();
    }
}
