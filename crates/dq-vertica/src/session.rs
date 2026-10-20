//! An open Vertica session.
//!
//! User statements run on a query connection that keeps at most one result
//! open between pages. Catalog reads, table pages and monitoring share one
//! read-only catalog connection, so the explorer keeps working while a query
//! runs; exports open a connection of their own. Grid edits and imports run
//! on a write connection with autocommit off, kept between transactions
//! while it is healthy.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, IdleConnection, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, SqlFlavor, TxEffect, VerticaDialect, command_tag, transaction_effect};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult, ErrorKind};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use crate::catalog;
use crate::conn::{Connection, RawRow, Role, Start, Target};
use crate::protocol::{Column, oid};
use crate::values::{column_meta, decode_row};

const STREAM_BATCH: usize = 1_000;

pub struct VtSession {
    pub(crate) target: Arc<Target>,
    info: ServerInfo,
    query: Mutex<QueryState>,
    /// The query connection's cancel key.
    query_key: (u32, u32),
    running: AtomicBool,
    catalog: Mutex<Option<Connection>>,
    writer: IdleConnection<Connection>,
}

struct QueryState {
    conn: Connection,
    cursor: Option<Cursor>,
    /// Autocommit is off for a transaction the user began.
    in_transaction: bool,
}

struct Cursor {
    columns: Vec<Column>,
    /// A row read ahead to learn whether more rows exist.
    pending: Option<RawRow>,
}

/// Cancels the running statement once its time is up. Vertica's own
/// `RUNTIMECAP` cannot be raised by ordinary users, so the limit is kept here.
struct Deadline {
    timer: Option<tokio::task::JoinHandle<()>>,
    fired: Arc<AtomicBool>,
}

impl Deadline {
    fn start(target: &Arc<Target>, key: (u32, u32), secs: u32) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let timer = (secs > 0).then(|| {
            let (target, fired) = (target.clone(), fired.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(u64::from(secs))).await;
                fired.store(true, Ordering::SeqCst);
                let _ = target.cancel(key).await;
            })
        });
        Self { timer, fired }
    }

    /// Stops the timer; a statement cancelled by it becomes a timeout.
    fn finish<T>(self, result: DbResult<T>, secs: u32) -> DbResult<T> {
        if let Some(timer) = self.timer {
            timer.abort();
        }
        match result {
            Err(e) if e.kind == ErrorKind::Cancelled && self.fired.load(Ordering::SeqCst) => Err(DbError::new(
                ErrorKind::Timeout,
                format!("The statement ran longer than {secs} s and was stopped."),
            )
            .with_action(Some("Narrow the query or raise the statement timeout in Settings."))),
            other => other,
        }
    }
}

/// Vertica reports the rows a DML statement changed as one `OUTPUT` row.
fn affected_rows(columns: &[Column], rows: &[RawRow], command: &str) -> Option<u64> {
    let dml = matches!(command, "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY");
    let [column] = columns else { return None };
    let [row] = rows else { return None };
    (dml && column.name.eq_ignore_ascii_case("OUTPUT") && column.type_oid == oid::INT8)
        .then(|| row.first().cloned().flatten())
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
}

/// `Vertica Analytic Database v25.4.0-0` → `Vertica 25.4.0-0`.
fn product_version(banner: &str) -> String {
    let version = banner
        .split_whitespace()
        .find_map(|w| w.strip_prefix('v').filter(|v| v.starts_with(|c: char| c.is_ascii_digit())))
        .unwrap_or(banner);
    format!("Vertica {version}")
}

impl VtSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let target = Arc::new(Target::new(params)?);
        let mut conn = target.connect(Role::Query, true).await?;
        let row = conn
            .text_rows("SELECT version(), CURRENT_USER(), CURRENT_DATABASE()", &[])
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let config = &params.config;
        let info = ServerInfo {
            engine: config.engine,
            version: product_version(&row.string(0)),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: conn.tls_version().map(|v| TlsInfo {
                version: Some(v.to_owned()),
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user: row.string(1),
            database: row.string(2),
            read_only: config.read_only,
        };
        Ok(Self {
            query_key: conn.key(),
            query: Mutex::new(QueryState { conn, cursor: None, in_transaction: false }),
            target,
            info,
            running: AtomicBool::new(false),
            catalog: Mutex::new(None),
            writer: IdleConnection::default(),
        })
    }

    /// The read-only catalog connection, reopened when it broke.
    pub(crate) async fn catalog(&self) -> DbResult<MappedMutexGuard<'_, Connection>> {
        let mut slot = self.catalog.lock().await;
        if slot.as_ref().is_none_or(Connection::is_broken) {
            *slot = Some(self.target.connect(Role::Catalog, true).await?);
        }
        Ok(MutexGuard::map(slot, |c| c.as_mut().expect("opened above")))
    }

    pub(crate) fn info_database(&self) -> &str {
        &self.info.database
    }

    pub(crate) fn info_version(&self) -> &str {
        &self.info.version
    }

    /// Stops the open result, if any: the server stops sending rows, the rest are discarded.
    async fn close_cursor(&self, q: &mut QueryState) {
        q.cursor = None;
        if q.conn.is_open() {
            let _ = self.target.cancel(self.query_key).await;
            let _ = q.conn.drain().await;
        }
    }

    /// Reads up to `limit` rows, then one more to learn whether more exist.
    async fn read_rows(conn: &mut Connection, cursor: &mut Cursor, limit: u32) -> DbResult<(Vec<RawRow>, bool)> {
        let mut rows = Vec::with_capacity(limit.min(1024) as usize);
        rows.extend(cursor.pending.take());
        while rows.len() < limit as usize {
            match conn.next_row().await? {
                Some(row) => rows.push(row),
                None => return Ok((rows, false)),
            }
        }
        cursor.pending = conn.next_row().await?;
        Ok((rows, cursor.pending.is_some()))
    }

    async fn run(&self, q: &mut QueryState, sql: &str, params: Params<'_>, limit: u32) -> DbResult<StatementResult> {
        let started = Instant::now();
        let command = command_tag(sql, SqlFlavor::Vertica);
        let done = |columns, rows, has_more, affected| StatementResult {
            columns,
            rows,
            has_more,
            command: command.clone(),
            affected,
            duration_ms: started.elapsed().as_millis() as u64,
            notices: Vec::new(),
        };
        let columns = match q.conn.start(sql, params).await? {
            Start::Done => return Ok(done(Vec::new(), Vec::new(), false, None)),
            Start::Rows(columns) => columns,
        };
        let mut cursor = Cursor { columns, pending: None };
        let (rows, has_more) = Self::read_rows(&mut q.conn, &mut cursor, limit).await?;
        if !has_more && let Some(count) = affected_rows(&cursor.columns, &rows, &command) {
            return Ok(done(Vec::new(), Vec::new(), false, Some(count)));
        }
        let meta = column_meta(&cursor.columns);
        let rows = rows.into_iter().map(|r| decode_row(&cursor.columns, r, CellLimits::GRID)).collect();
        if has_more {
            q.cursor = Some(cursor);
        }
        Ok(done(meta, rows, has_more, None))
    }

    /// Every row of a read on `conn`, decoded with `limits`.
    async fn collect(
        conn: &mut Connection,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let Start::Rows(columns) = conn.start(sql, params).await? else {
            on_batch(&[], Vec::new())?;
            return Ok(0);
        };
        let meta = column_meta(&columns);
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        while let Some(row) = conn.next_row().await? {
            batch.push(decode_row(&columns, row, limits));
            if batch.len() == STREAM_BATCH {
                total += batch.len() as u64;
                on_batch(&meta, std::mem::take(&mut batch))?;
            }
        }
        total += batch.len() as u64;
        on_batch(&meta, batch)?;
        Ok(total)
    }

    /// Runs one statement of a transaction and returns the rows it changed.
    async fn write(conn: &mut Connection, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        let Start::Rows(columns) = conn.start(sql, params).await? else { return Ok(0) };
        let mut rows = Vec::new();
        while let Some(row) = conn.next_row().await? {
            if rows.len() < 2 {
                rows.push(row);
            }
        }
        Ok(affected_rows(&columns, &rows, &command_tag(sql, SqlFlavor::Vertica)).unwrap_or(0))
    }
}

#[async_trait]
impl Session for VtSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &VerticaDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let mut conn = self.catalog().await?;
        match parent {
            None => Ok(vec![catalog::database(&self.info.database)]),
            Some(p) if p.kind == NodeKind::Database => catalog::schemas(&mut conn, &p.name).await,
            Some(p) if p.kind == NodeKind::Schema => catalog::relations(&mut conn, &p.database, &p.name).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(&mut conn, p).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&mut *self.catalog().await?, &self.info.database, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let options = options.clamped();
        let mut q = self.query.lock().await;
        self.close_cursor(&mut q).await;
        q.conn.take_notices();
        // The connection autocommits, and Vertica's BEGIN does not suspend
        // that: a transaction the user begins turns autocommit off until it ends.
        let effect = transaction_effect(sql, SqlFlavor::Vertica);
        if effect == TxEffect::Begin && !q.in_transaction {
            q.conn.execute("SET SESSION AUTOCOMMIT TO OFF").await?;
            q.in_transaction = true;
        }
        self.running.store(true, Ordering::SeqCst);
        let deadline = Deadline::start(&self.target, self.query_key, options.timeout_secs);
        let result = self.run(&mut q, sql, params, options.row_limit).await;
        let result = deadline.finish(result, options.timeout_secs);
        self.running.store(false, Ordering::SeqCst);
        if effect == TxEffect::End && q.in_transaction && !q.conn.is_open() {
            q.conn.execute("SET SESSION AUTOCOMMIT TO ON").await?;
            q.in_transaction = false;
        }
        let notices = q.conn.take_notices();
        result.map(|r| StatementResult { notices, ..r })
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut q = self.query.lock().await;
        let mut cursor =
            q.cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        self.running.store(true, Ordering::SeqCst);
        let read = Self::read_rows(&mut q.conn, &mut cursor, limit.clamp(1, ExecOptions::MAX_ROW_LIMIT)).await;
        self.running.store(false, Ordering::SeqCst);
        let (rows, has_more) = read?;
        let rows = rows.into_iter().map(|r| decode_row(&cursor.columns, r, CellLimits::GRID)).collect();
        if has_more {
            q.cursor = Some(cursor);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.target.cancel(self.query_key).await?;
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&mut *self.catalog().await?, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&mut *self.catalog().await?, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(&mut *self.catalog().await?, object).await
    }

    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&mut *self.catalog().await?, database, schema).await
    }

    async fn read_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        let (mut columns, mut rows) = (Vec::new(), Vec::new());
        Self::collect(&mut *self.catalog().await?, sql, params, limits, &mut |meta, batch| {
            if columns.is_empty() {
                columns = meta.to_vec();
            }
            rows.extend(batch);
            Ok(())
        })
        .await?;
        Ok((columns, rows))
    }

    /// A connection of its own: a long export would otherwise hold up the explorer.
    async fn stream_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let mut conn = self.target.connect(Role::Export, true).await?;
        let result = Self::collect(&mut conn, sql, params, CellLimits::FULL, on_batch).await;
        conn.terminate().await;
        result
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::row_estimate(&mut *self.catalog().await?, object).await
    }

    /// On any error the connection is dropped (the server rolls back); after
    /// a commit it is kept for the next transaction.
    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        let mut conn = match self.writer.take("").filter(|c| !c.is_broken()) {
            Some(conn) => conn,
            None => self.target.connect(Role::Write, false).await?,
        };
        let outcome: DbResult<Vec<u64>> = async {
            let mut counts = Vec::new();
            for statement in statements {
                let statement = statement?;
                let affected = Self::write(&mut conn, &statement.sql, &statement.params).await?;
                check_affected(&statement, affected)?;
                counts.push(affected);
            }
            conn.execute("COMMIT").await?;
            Ok(counts)
        }
        .await;
        match outcome {
            Ok(counts) => {
                self.writer.put("", conn);
                Ok(counts)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK").await;
                conn.terminate().await;
                Err(e)
            }
        }
    }

    /// `EXPLAIN` returns the plan as text lines.
    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        if analyze {
            return Err(DbError::unsupported(
                "Vertica measures a run with PROFILE; its counters are in v_monitor.execution_engine_profiles.",
            ));
        }
        if !params.is_empty() {
            return Err(DbError::unsupported(
                "Vertica cannot plan a statement with ? parameters. Write the values into the statement to see its plan.",
            ));
        }
        let statement = format!("EXPLAIN {}", sql.trim().trim_end_matches(';'));
        let result = self.execute(&statement, params, ExecOptions { row_limit: 10_000, timeout_secs }).await?;
        let raw = result.rows.iter().filter_map(|r| r.first()).filter_map(Cell::to_text).collect::<Vec<_>>().join("\n");
        Ok(ExplainResult { plan: None, raw, analyzed: false, planning_ms: None, execution_ms: None })
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        self.query.lock().await.conn.terminate().await;
        if let Some(mut conn) = self.catalog.lock().await.take() {
            conn.terminate().await;
        }
        if let Some(mut conn) = self.writer.clear() {
            conn.terminate().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_row_counts() {
        assert_eq!(product_version("Vertica Analytic Database v25.4.0-0"), "Vertica 25.4.0-0");
        let output = Column {
            name: "OUTPUT".into(),
            table_oid: 0,
            type_oid: oid::INT8,
            type_name: None,
            type_modifier: -1,
            nullable: true,
        };
        let rows = vec![vec![Some(b"3".to_vec())]];
        assert_eq!(affected_rows(std::slice::from_ref(&output), &rows, "UPDATE"), Some(3));
        assert_eq!(affected_rows(std::slice::from_ref(&output), &rows, "SELECT"), None);
    }
}
