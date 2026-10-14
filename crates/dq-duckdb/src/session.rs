//! An open `DuckDB` database file (or in-memory database).
//!
//! User statements run on a connection owned by a worker thread (see
//! [`crate::worker`]). Catalog reads, table pages and exports use a second
//! connection to the same database, and grid edits and imports a third, so
//! the explorer keeps working while a query runs.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, DuckdbDialect, SqlFlavor, split_statements};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult, ErrorKind};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::{AccessMode, Config, Connection, params_from_iter};
use tokio::sync::{mpsc, oneshot};

use crate::catalog;
use crate::error::map_error;
use crate::values::{batch_rows, column_meta};
use crate::worker::{self, Command, Interrupter};

const STREAM_BATCH: usize = 2_048;

pub struct DuckSession {
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

/// Opens the database: the file read-only when the profile is.
fn open_database(params: &ConnectParams) -> DbResult<Connection> {
    let path = params.config.database.trim();
    let mut config = Config::default().with("custom_user_agent", "DataQuery").map_err(|e| map_error(&e))?;
    if params.config.read_only && path != ":memory:" {
        config = config.access_mode(AccessMode::ReadOnly).map_err(|e| map_error(&e))?;
    }
    if path == ":memory:" {
        Connection::open_in_memory_with_flags(config).map_err(|e| map_error(&e))
    } else {
        if !params.config.read_only && !std::path::Path::new(path).parent().is_some_and(std::path::Path::exists) {
            return Err(DbError::new(ErrorKind::Storage, format!("The folder of {path} does not exist.")));
        }
        Connection::open_with_flags(path, config).map_err(|e| map_error(&e))
    }
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
    let schema = statement.query_arrow(params_from_iter(params.iter())).map_err(|e| map_error(&e))?.get_schema();
    let meta = column_meta(&schema);
    let mut batch = Vec::with_capacity(STREAM_BATCH);
    while let Some(array) = statement.step().map_err(|e| map_error(&e))? {
        batch.extend(batch_rows(&RecordBatch::from(&array), limits));
        if batch.len() >= STREAM_BATCH && !send(&meta, std::mem::take(&mut batch)) {
            return Ok(());
        }
    }
    send(&meta, batch);
    Ok(())
}

/// Rows a write changed: `DuckDB` returns them as its single `Count` value.
fn changed_rows(conn: &Connection, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
    let mut count = 0;
    read_blocking(conn, sql, params, CellLimits::GRID, |_, rows| {
        if let Some(n) = rows.first().and_then(|r| r.first()).and_then(Cell::to_text).and_then(|t| t.parse().ok()) {
            count = n;
        }
        true
    })?;
    Ok(count)
}

impl DuckSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let params = params.clone();
        let (conn, catalog, writer, info) = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let conn = open_database(&params)?;
            let version: String = conn.query_row("SELECT version()", [], |r| r.get(0)).map_err(|e| map_error(&e))?;
            let database = catalog::current_database(&conn)?;
            let catalog = conn.try_clone().map_err(|e| map_error(&e))?;
            let writer = conn.try_clone().map_err(|e| map_error(&e))?;
            let info = ServerInfo {
                engine: params.config.engine,
                version: format!("DuckDB {}", version.trim_start_matches('v')),
                latency_ms: started.elapsed().as_millis() as u64,
                tls: None,
                user: String::new(),
                database,
                read_only: params.config.read_only,
            };
            Ok::<_, DbError>((conn, catalog, writer, info))
        })
        .await
        .map_err(|e| DbError::internal(format!("open task failed: {e}")))??;
        let (worker, interrupter) = worker::spawn(conn);
        Ok(Self {
            read_only: info.read_only,
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
impl Session for DuckSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &DuckdbDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let parent = parent.cloned();
        let current = self.info.database.clone();
        blocking(&self.catalog, move |conn| match parent {
            None => catalog::databases(conn, &current),
            Some(p) if p.kind == NodeKind::Database => catalog::schemas(conn, &p.name),
            Some(p) if p.kind == NodeKind::Schema => catalog::relations(conn, &p.database, &p.name),
            Some(p) if p.kind == NodeKind::Table => catalog::table_children(conn, &p),
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(conn, &p),
            Some(_) => Ok(Vec::new()),
        })
        .await
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let query = query.to_owned();
        blocking(&self.catalog, move |conn| catalog::search(conn, &query, limit)).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.read_only && split_statements(sql, SqlFlavor::Duckdb).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        self.request(|reply| Command::Execute { sql: sql.to_owned(), params: params.to_vec(), options, reply }).await
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

    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.clone() } else { database.to_owned() };
        let schema = schema.map(str::to_owned);
        blocking(&self.catalog, move |conn| catalog::schema_model(conn, &database, schema.as_deref())).await
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
        blocking(&self.catalog, move |conn| catalog::row_estimate(conn, &object)).await
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let statements: Vec<_> = statements.collect::<DbResult<Vec<_>>>()?;
        blocking(&self.writer, move |conn| {
            conn.execute_batch("BEGIN TRANSACTION").map_err(|e| map_error(&e))?;
            let outcome = (|| {
                let mut counts = Vec::with_capacity(statements.len());
                for statement in &statements {
                    let affected = changed_rows(conn, &statement.sql, &statement.params)?;
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
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        let sql = sql.trim().trim_end_matches(';');
        let statement = if analyze {
            format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}")
        } else {
            format!("EXPLAIN (FORMAT JSON) {sql}")
        };
        let options = ExecOptions { row_limit: 100, timeout_secs };
        let result = self.execute(&statement, params, options).await?;
        // Rows of (explain_key, explain_value); the plan is the JSON value.
        let raw = result.rows.iter().filter_map(|r| r.last()).filter_map(Cell::to_text).collect::<Vec<_>>().join("\n");
        crate::explain::parse(&raw, analyze)
    }

    async fn close(&self) {
        let _ = self.interrupter.cancel();
    }
}
