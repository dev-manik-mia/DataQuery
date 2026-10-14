//! An open Oracle session.
//!
//! Oracle calls block, so every call runs on a blocking thread. User
//! statements run on one query connection that keeps at most one open
//! result. Cancelling (and the statement timeout) asks the server to cancel
//! the session's statement (`ALTER SYSTEM CANCEL SQL`), and falls back to a
//! client break when the account may not. Catalog
//! reads, table pages and exports use a small pool; grid edits and imports
//! use a short-lived connection per transaction.
//!
//! Oracle has no autocommit on the server: a write outside an explicit
//! transaction (`SET TRANSACTION …`) is committed right after it runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{
    Dialect, OracleDialect, SqlFlavor, StatementKind, TxEffect, command_tag, split_statements, transaction_effect,
};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use oracle::{Connection, ResultSet, Row};
use tokio::sync::Mutex;

use crate::catalog;
use crate::connect;
use crate::error::map_error;
use crate::pool::Pool;
use crate::rows::prepare;
use crate::values::{column_meta, decode_row};

const STREAM_BATCH: usize = 1_000;

/// Columns, the open result (if rows remain), the first rows, whether more exist, affected rows.
type Outcome = (Vec<ColumnMeta>, Option<Cursor>, Vec<CellRow>, bool, Option<u64>);

pub struct OraSession {
    info: ServerInfo,
    /// Session id of the query connection, hidden from the process list.
    pub(crate) sid: i64,
    query: Mutex<QueryState>,
    /// Stops the query connection's running statement from another thread.
    interrupter: std::sync::Mutex<Arc<Interrupter>>,
    running: AtomicBool,
    pub(crate) pool: Arc<Pool>,
}

/// Cancels whatever the query connection is running.
struct Interrupter {
    conn: Arc<Connection>,
    sid: i64,
    serial: i64,
    pool: Arc<Pool>,
}

impl Interrupter {
    /// Asks the server to cancel the statement; when the account lacks
    /// `ALTER SYSTEM`, sends a break on the connection instead (which the
    /// server sees at once only where the network carries out-of-band data).
    async fn interrupt(&self) {
        let sql = format!("ALTER SYSTEM CANCEL SQL '{}, {}'", self.sid, self.serial);
        let cancelled =
            self.pool.run(move |conn| conn.execute(&sql, &[]).map(|_| ()).map_err(|e| map_error(&e, None))).await;
        if cancelled.is_err() {
            let conn = self.conn.clone();
            let _ = tokio::task::spawn_blocking(move || conn.break_execution()).await;
        }
    }
}

struct QueryState {
    conn: Arc<Connection>,
    cursor: Option<Cursor>,
    /// An explicit transaction is open; writes wait for COMMIT or ROLLBACK.
    in_transaction: bool,
}

struct Cursor {
    rows: ResultSet<'static, Row>,
    /// A row read ahead to learn whether more rows exist.
    pending: Option<Row>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// SQL statements cannot end with `;` or `/`; PL/SQL blocks keep their final `;`.
fn trim_terminator(sql: &str) -> &str {
    let sql = sql.trim_end();
    let sql = sql.strip_suffix('/').map_or(sql, str::trim_end);
    let plsql = split_statements(sql, SqlFlavor::Oracle).first().is_some_and(|s| {
        let head = sql[s.byte_start..].trim_start().to_ascii_uppercase();
        s.kind == StatementKind::Call
            || head.starts_with("DECLARE")
            || head.starts_with("BEGIN")
            || ["PROCEDURE", "FUNCTION", "PACKAGE", "TRIGGER", "TYPE"]
                .iter()
                .any(|k| head.contains(k) && head.starts_with("CREATE"))
    });
    if plsql { sql } else { sql.strip_suffix(';').map_or(sql, str::trim_end) }
}

/// Reads up to `limit` rows, then one more to learn whether more exist.
fn read_rows(cursor: &mut Cursor, limit: u32, secret: Option<&str>) -> DbResult<(Vec<CellRow>, bool)> {
    let mut rows = Vec::with_capacity(limit.min(1024) as usize);
    if let Some(row) = cursor.pending.take() {
        rows.push(decode_row(&row, CellLimits::GRID));
    }
    while rows.len() < limit as usize {
        match cursor.rows.next() {
            Some(row) => rows.push(decode_row(&row.map_err(|e| map_error(&e, secret))?, CellLimits::GRID)),
            None => return Ok((rows, false)),
        }
    }
    match cursor.rows.next() {
        Some(row) => {
            cursor.pending = Some(row.map_err(|e| map_error(&e, secret))?);
            Ok((rows, true))
        }
        None => Ok((rows, false)),
    }
}

/// Runs blocking `work` on the query connection. After `timeout_secs` a
/// cancel stops it; unlike Oracle's call timeout, that keeps the connection usable.
async fn with_deadline<T: Send + 'static>(
    interrupter: Arc<Interrupter>,
    timeout_secs: u32,
    work: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
) -> DbResult<T> {
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let conn = interrupter.conn.clone();
    let timer = (timeout_secs > 0).then(|| {
        let (done, timed_out) = (done.clone(), timed_out.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs))).await;
            if !done.load(Ordering::SeqCst) {
                timed_out.store(true, Ordering::SeqCst);
                interrupter.interrupt().await;
            }
        })
    });
    let finished = done.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = work(&conn);
        finished.store(true, Ordering::SeqCst);
        result
    })
    .await
    .map_err(|e| DbError::internal(format!("query task failed: {e}")))?;
    if let Some(timer) = timer {
        timer.abort();
    }
    match result {
        Err(e) if e.kind == ErrorKind::Cancelled && timed_out.load(Ordering::SeqCst) => Err(DbError::new(
            ErrorKind::Timeout,
            format!("The statement ran longer than {timeout_secs} s and was stopped."),
        )
        .with_action(Some("Narrow the query or raise the statement timeout in Settings."))),
        other => other,
    }
}

impl OraSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let opened = connect::open_query(params).await?;
        let conn = Arc::new(opened.conn);
        let pool = Arc::new(Pool::new(params.clone()));
        let interrupter =
            Interrupter { conn: conn.clone(), sid: opened.sid, serial: opened.serial, pool: pool.clone() };
        Ok(Self {
            interrupter: std::sync::Mutex::new(Arc::new(interrupter)),
            query: Mutex::new(QueryState { conn, cursor: None, in_transaction: false }),
            running: AtomicBool::new(false),
            pool,
            sid: opened.sid,
            info: opened.info,
        })
    }

    fn interrupter(&self) -> DbResult<Arc<Interrupter>> {
        self.interrupter.lock().map(|i| i.clone()).map_err(|_| DbError::internal("interrupter lock poisoned"))
    }

    fn params(&self) -> &ConnectParams {
        self.pool.params()
    }

    fn secret(&self) -> Option<String> {
        self.params().password().map(str::to_owned)
    }

    /// Replaces a query connection that broke (a network error or call timeout).
    async fn reconnect_if_broken(&self, q: &mut QueryState) -> DbResult<()> {
        let conn = q.conn.clone();
        let healthy = tokio::task::spawn_blocking(move || conn.status().is_ok_and(|s| s == oracle::ConnStatus::Normal))
            .await
            .unwrap_or(false);
        if !healthy {
            let conn = Arc::new(connect::open(self.params()).await?);
            let probe = conn.clone();
            let (sid, serial) = tokio::task::spawn_blocking(move || connect::session_id(&probe))
                .await
                .map_err(|e| DbError::internal(format!("connect task failed: {e}")))??;
            q.conn = conn.clone();
            q.cursor = None;
            q.in_transaction = false;
            if let Ok(mut current) = self.interrupter.lock() {
                *current = Arc::new(Interrupter { conn, sid, serial, pool: self.pool.clone() });
            }
        }
        Ok(())
    }

    async fn run(
        &self,
        q: &mut QueryState,
        sql: &str,
        params: Params<'_>,
        options: ExecOptions,
    ) -> DbResult<StatementResult> {
        let started = Instant::now();
        let sql = trim_terminator(sql).to_owned();
        let effect = transaction_effect(&sql, SqlFlavor::Oracle);
        let command = command_tag(&sql, SqlFlavor::Oracle);
        let params = params.to_vec();
        let interrupter = self.interrupter()?;
        let in_transaction = q.in_transaction || effect == TxEffect::Begin;
        let secret = self.secret();
        let outcome = with_deadline(interrupter, options.timeout_secs, move |conn| -> DbResult<Outcome> {
            let err = |e: oracle::Error| map_error(&e, secret.as_deref());
            let mut statement = prepare(conn, &sql, &params).map_err(err)?;
            if statement.is_query() {
                let rows = statement.into_result_set::<Row>(&[]).map_err(err)?;
                let columns = column_meta(rows.column_info());
                let mut cursor = Cursor { rows, pending: None };
                let (rows, more) = read_rows(&mut cursor, options.row_limit, secret.as_deref())?;
                return Ok((columns, more.then_some(cursor), rows, more, None));
            }
            statement.execute(&[]).map_err(err)?;
            let affected = statement.row_count().map_err(err)?;
            // Outside an explicit transaction every write is committed at once.
            if !in_transaction && !statement.is_ddl() {
                conn.commit().map_err(err)?;
            }
            Ok((Vec::new(), None, Vec::new(), false, Some(affected)))
        })
        .await;
        let (columns, cursor, rows, has_more, affected) = match outcome {
            Ok(o) => o,
            Err(e) => {
                if matches!(e.kind, ErrorKind::Network | ErrorKind::Timeout) {
                    let _ = self.reconnect_if_broken(q).await;
                }
                return Err(e);
            }
        };
        q.cursor = cursor;
        q.in_transaction = match effect {
            TxEffect::Begin => true,
            TxEffect::End => false,
            TxEffect::RollbackToSavepoint | TxEffect::None => q.in_transaction,
        };
        Ok(StatementResult {
            columns,
            rows,
            has_more,
            command,
            affected,
            duration_ms: started.elapsed().as_millis() as u64,
            notices: Vec::new(),
        })
    }

    /// Streams a read-only query on a pooled connection in batches.
    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let sql = trim_terminator(sql).to_owned();
        let params = params.to_vec();
        let secret = self.secret();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<DbResult<(Vec<ColumnMeta>, Vec<CellRow>)>>(2);
        let work = self.pool.run(move |conn| {
            let err = |e: oracle::Error| map_error(&e, secret.as_deref());
            let rows = prepare(conn, &sql, &params).and_then(|s| s.into_result_set::<Row>(&[])).map_err(err)?;
            let meta = column_meta(rows.column_info());
            let mut batch = Vec::with_capacity(STREAM_BATCH);
            for row in rows {
                batch.push(decode_row(&row.map_err(err)?, limits));
                if batch.len() == STREAM_BATCH
                    && tx.blocking_send(Ok((meta.clone(), std::mem::take(&mut batch)))).is_err()
                {
                    return Ok(());
                }
            }
            let _ = tx.blocking_send(Ok((meta, batch)));
            Ok(())
        });
        let mut total = 0u64;
        let mut failure = None;
        let consume = async {
            while let Some(item) = rx.recv().await {
                match item.and_then(|(meta, batch)| {
                    total += batch.len() as u64;
                    on_batch(&meta, batch)
                }) {
                    Ok(()) => {}
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
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
impl Session for OraSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &OracleDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let database = self.info.database.clone();
        let Some(parent) = parent.cloned() else {
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, database.clone(), None, database));
            node.current = true;
            return Ok(vec![node]);
        };
        self.pool
            .run(move |conn| match parent.kind {
                NodeKind::Database => catalog::schemas(conn, &parent.name),
                NodeKind::Schema => catalog::relations(conn, &parent.database, &parent.name),
                NodeKind::Table | NodeKind::MaterializedView => catalog::table_children(conn, &parent),
                NodeKind::Group => catalog::group_children(conn, &parent),
                _ => Ok(Vec::new()),
            })
            .await
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let (database, query) = (self.info.database.clone(), query.to_owned());
        self.pool.run(move |conn| catalog::search(conn, &database, &query, limit)).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.params().config.read_only && split_statements(sql, SqlFlavor::Oracle).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        let mut q = self.query.lock().await;
        q.cursor = None;
        self.running.store(true, Ordering::SeqCst);
        let result = self.run(&mut q, sql, params, options).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut q = self.query.lock().await;
        let mut cursor =
            q.cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let secret = self.secret();
        let limit = limit.clamp(1, ExecOptions::MAX_ROW_LIMIT);
        self.running.store(true, Ordering::SeqCst);
        let read = tokio::task::spawn_blocking(move || {
            let read = read_rows(&mut cursor, limit, secret.as_deref());
            (cursor, read)
        })
        .await
        .map_err(|e| DbError::internal(format!("fetch task failed: {e}")));
        self.running.store(false, Ordering::SeqCst);
        let (cursor, read) = read?;
        let (rows, has_more) = read?;
        if has_more {
            q.cursor = Some(cursor);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.interrupter()?.interrupt().await;
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        let object = object.clone();
        self.pool.run(move |conn| catalog::structure(conn, &object)).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        let object = object.clone();
        self.pool.run(move |conn| catalog::ddl(conn, &object)).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        let object = object.clone();
        self.pool.run(move |conn| catalog::dependencies(conn, &object)).await
    }

    async fn schema_model(&self, _database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = self.info.database.clone();
        let schema = schema.map_or_else(|| self.info.user.clone(), str::to_owned);
        self.pool.run(move |conn| catalog::schema_model(conn, &database, &schema)).await
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
        self.pool.run(move |conn| catalog::row_estimate(conn, &object)).await
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.params().config.read_only {
            return Err(read_only_error());
        }
        // Statements are produced lazily; build them here, run them together.
        let statements: Vec<_> = statements.collect::<DbResult<Vec<_>>>()?;
        let params = self.params().clone();
        tokio::task::spawn_blocking(move || {
            let conn = connect::open_blocking(&params)?;
            let err = |e: oracle::Error| map_error(&e, params.password());
            let mut counts = Vec::with_capacity(statements.len());
            let outcome: DbResult<()> = (|| {
                for statement in &statements {
                    let mut stmt = prepare(&conn, &statement.sql, &statement.params).map_err(err)?;
                    stmt.execute(&[]).map_err(err)?;
                    let affected = stmt.row_count().map_err(err)?;
                    check_affected(statement, affected)?;
                    counts.push(affected);
                }
                conn.commit().map_err(err)
            })();
            if outcome.is_err() {
                let _ = conn.rollback();
            }
            let _ = conn.close();
            outcome.map(|()| counts)
        })
        .await
        .map_err(|e| DbError::internal(format!("transaction task failed: {e}")))?
    }

    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        let _q = self.query.lock().await;
        let interrupter = self.interrupter()?;
        let sql = trim_terminator(sql).to_owned();
        let params = params.to_vec();
        let secret = self.secret();
        with_deadline(interrupter, timeout_secs, move |conn| {
            crate::explain::explain(conn, &sql, &params, analyze, secret.as_deref())
        })
        .await
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        self.pool.close().await;
        let q = self.query.lock().await;
        let conn = q.conn.clone();
        let _ = tokio::task::spawn_blocking(move || conn.close()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::trim_terminator;

    #[test]
    fn sql_loses_its_terminator_and_plsql_keeps_it() {
        assert_eq!(trim_terminator("SELECT 1 FROM dual;"), "SELECT 1 FROM dual");
        assert_eq!(trim_terminator("BEGIN NULL; END;\n/"), "BEGIN NULL; END;");
        assert_eq!(
            trim_terminator("CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;"),
            "CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;"
        );
        assert_eq!(trim_terminator("CREATE TABLE t (a NUMBER);"), "CREATE TABLE t (a NUMBER)");
    }
}
