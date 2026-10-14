//! An open MySQL / MariaDB session.
//!
//! User statements run on a dedicated query connection that keeps at most one
//! unfinished result open. Catalog reads, table pages and exports use a small
//! pool of read-only connections, so the explorer keeps working while a query
//! runs. Grid edits and imports use a short-lived connection per transaction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::TableStructure;
use dq_core::sql::{Dialect, MysqlDialect, SqlFlavor, command_tag, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{BinaryProtocol, Column, Conn, Pool, QueryResult, Row, TextProtocol, TxOpts, Value};
use tokio::sync::Mutex;

use crate::catalog;
use crate::connect::{self, Flavor, Role};
use crate::error::map_error;
use crate::values::{column_meta, decode_row, text};

const STREAM_BATCH: usize = 1_000;
const MAX_WARNINGS: u32 = 50;

pub struct MySession {
    /// Connection settings with `prefer` TLS settled by the first connection.
    /// Kept in memory only, to open catalog and edit connections.
    params: ConnectParams,
    info: ServerInfo,
    flavor: Flavor,
    query: Mutex<QueryConnection>,
    /// Server thread id of the query connection, for `KILL QUERY`.
    query_thread: u32,
    running: AtomicBool,
    catalog: Pool,
}

struct QueryConnection {
    conn: Conn,
    /// A row read ahead from a result that is still open on `conn`. The read
    /// position itself lives in the driver's connection state.
    pending: Option<Row>,
    /// The open result came from a prepared (parameterised) statement.
    binary: bool,
    timeout_secs: Option<u32>,
}

/// Reads the next row of the open result on `conn`.
///
/// `QueryResult::next` reads one row at a time and leaves the result open, so a
/// later call can continue where this one stopped. (`QueryResult::stream`
/// would discard the unread rows when dropped.) Parameterised statements use
/// the binary protocol, plain ones the text protocol.
async fn next_row(conn: &mut Conn, binary: bool) -> mysql_async::Result<Option<Row>> {
    if binary {
        QueryResult::<'_, '_, BinaryProtocol>::new(conn).next().await
    } else {
        QueryResult::<'_, '_, TextProtocol>::new(conn).next().await
    }
}

fn bind(values: &[Option<String>]) -> mysql_async::Params {
    if values.is_empty() {
        return mysql_async::Params::Empty;
    }
    mysql_async::Params::Positional(
        values.iter().map(|p| p.as_ref().map_or(Value::NULL, |v| Value::Bytes(v.as_bytes().to_vec()))).collect(),
    )
}

impl MySession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let opened = connect::open_query(params).await?;
        let params = connect::resolved(params, &opened.info);
        Ok(Self {
            catalog: connect::catalog_pool(&params, opened.flavor),
            query_thread: opened.conn.id(),
            query: Mutex::new(QueryConnection { conn: opened.conn, pending: None, binary: false, timeout_secs: None }),
            params,
            info: opened.info,
            flavor: opened.flavor,
            running: AtomicBool::new(false),
        })
    }

    fn err(&self, e: &mysql_async::Error) -> DbError {
        map_error(e, self.params.password())
    }

    async fn catalog(&self) -> DbResult<Conn> {
        self.catalog.get_conn().await.map_err(|e| self.err(&e))
    }

    /// Interrupts whatever the query connection is running, from another connection.
    async fn kill_query(&self) -> DbResult<()> {
        let mut conn = self.catalog().await?;
        match conn.query_drop(format!("KILL QUERY {}", self.query_thread)).await {
            // 1094: the statement already finished.
            Err(mysql_async::Error::Server(e)) if e.code == 1094 => Ok(()),
            result => result.map_err(|e| self.err(&e)),
        }
    }

    /// Stops the open result, if any, so the next statement starts promptly.
    async fn close_cursor(&self, q: &mut QueryConnection) {
        if q.pending.take().is_some() {
            let _ = self.kill_query().await;
            while let Ok(Some(_)) = next_row(&mut q.conn, q.binary).await {}
        }
    }

    async fn set_timeout(&self, q: &mut QueryConnection, secs: u32) -> DbResult<()> {
        if q.timeout_secs != Some(secs) {
            q.conn.query_drop(self.flavor.timeout_statement(secs)).await.map_err(|e| self.err(&e))?;
            q.timeout_secs = Some(secs);
        }
        Ok(())
    }

    /// Reads up to `limit` rows, then one more to learn whether more exist.
    async fn read_rows(
        &self,
        conn: &mut Conn,
        binary: bool,
        pending: Option<Row>,
        limit: u32,
    ) -> DbResult<(Vec<CellRow>, Option<Row>)> {
        let decode = |row: &Row| decode_row(row, row.columns_ref(), CellLimits::GRID);
        let mut rows = Vec::with_capacity(limit.min(1024) as usize);
        rows.extend(pending.as_ref().map(decode));
        while rows.len() < limit as usize {
            match next_row(conn, binary).await.map_err(|e| self.err(&e))? {
                Some(row) => rows.push(decode(&row)),
                None => return Ok((rows, None)),
            }
        }
        Ok((rows, next_row(conn, binary).await.map_err(|e| self.err(&e))?))
    }

    /// Starts `sql` on `conn`: prepared with bound parameters when there are
    /// any, as plain text otherwise. Returns the result's columns and affected rows.
    async fn start(
        &self,
        conn: &mut Conn,
        sql: &str,
        params: Params<'_>,
    ) -> DbResult<(Option<std::sync::Arc<[Column]>>, u64)> {
        if params.is_empty() {
            let result = conn.query_iter(sql).await.map_err(|e| self.err(&e))?;
            return Ok((result.columns(), result.affected_rows()));
        }
        let statement = conn.prep(sql).await.map_err(|e| self.err(&e))?;
        if usize::from(statement.num_params()) != params.len() {
            return Err(DbError::invalid(format!(
                "The statement has {} parameter(s) but {} value(s) were given.",
                statement.num_params(),
                params.len()
            )));
        }
        let result = conn.exec_iter(statement, bind(params)).await.map_err(|e| self.err(&e))?;
        Ok((result.columns(), result.affected_rows()))
    }

    /// `SHOW WARNINGS` after a statement that produced some, as notices.
    async fn warnings(&self, conn: &mut Conn) -> Vec<String> {
        if conn.get_warnings() == 0 {
            return Vec::new();
        }
        let rows: Vec<Row> = conn.query(format!("SHOW WARNINGS LIMIT {MAX_WARNINGS}")).await.unwrap_or_default();
        rows.iter()
            .map(|r| {
                format!(
                    "{} {}: {}",
                    text(r, 0).unwrap_or_default(),
                    text(r, 1).unwrap_or_default(),
                    text(r, 2).unwrap_or_default()
                )
            })
            .collect()
    }

    async fn run(
        &self,
        q: &mut QueryConnection,
        sql: &str,
        params: Params<'_>,
        options: ExecOptions,
    ) -> DbResult<StatementResult> {
        let started = Instant::now();
        q.binary = !params.is_empty();
        let (columns, affected) = self.start(&mut q.conn, sql, params).await?;
        let columns = columns.map(|c| column_meta(&c)).unwrap_or_default();
        let command = command_tag(sql, SqlFlavor::Mysql);
        if columns.is_empty() {
            while next_row(&mut q.conn, q.binary).await.map_err(|e| self.err(&e))?.is_some() {}
            return Ok(StatementResult {
                columns,
                rows: Vec::new(),
                has_more: false,
                command,
                affected: Some(affected),
                duration_ms: started.elapsed().as_millis() as u64,
                notices: self.warnings(&mut q.conn).await,
            });
        }
        let (rows, pending) = self.read_rows(&mut q.conn, q.binary, None, options.row_limit).await?;
        let has_more = pending.is_some();
        let notices = if has_more {
            q.pending = pending;
            Vec::new()
        } else {
            self.warnings(&mut q.conn).await
        };
        Ok(StatementResult {
            columns,
            rows,
            has_more,
            command,
            affected: None,
            duration_ms: started.elapsed().as_millis() as u64,
            notices,
        })
    }

    /// Runs a read-only query on a pooled catalog connection and streams its rows in batches.
    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let mut conn = self.catalog().await?;
        let (columns, _) = self.start(&mut conn, sql, params).await?;
        let columns = columns.unwrap_or_default();
        let meta = column_meta(&columns);
        let binary = !params.is_empty();
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        while let Some(row) = next_row(&mut conn, binary).await.map_err(|e| self.err(&e))? {
            batch.push(decode_row(&row, &columns, limits));
            if batch.len() == STREAM_BATCH {
                total += batch.len() as u64;
                on_batch(&meta, std::mem::take(&mut batch))?;
            }
        }
        total += batch.len() as u64;
        on_batch(&meta, batch)?;
        Ok(total)
    }
}

#[async_trait]
impl Session for MySession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &MysqlDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(&mut self.catalog().await?, &self.info.database).await,
            Some(p) if p.kind == NodeKind::Database => catalog::relations(&mut self.catalog().await?, &p.name).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&mut self.catalog().await?, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        // The driver always enables multi-statement mode; never let one call run two.
        if split_statements(sql, SqlFlavor::Mysql).len() > 1 {
            return Err(DbError::invalid("Run one statement at a time."));
        }
        let options = options.clamped();
        let mut q = self.query.lock().await;
        self.close_cursor(&mut q).await;
        self.set_timeout(&mut q, options.timeout_secs).await?;
        self.running.store(true, Ordering::SeqCst);
        let result = self.run(&mut q, sql, params, options).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut q = self.query.lock().await;
        let pending = q
            .pending
            .take()
            .ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        self.running.store(true, Ordering::SeqCst);
        let binary = q.binary;
        let read = self.read_rows(&mut q.conn, binary, Some(pending), limit.clamp(1, ExecOptions::MAX_ROW_LIMIT)).await;
        self.running.store(false, Ordering::SeqCst);
        let (rows, pending) = read?;
        let has_more = pending.is_some();
        q.pending = pending;
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.kill_query().await?;
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&mut self.catalog().await?, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&mut self.catalog().await?, object).await
    }

    /// Generated SQL is fully qualified, so one pool serves every database.
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
        catalog::row_estimate(&mut self.catalog().await?, object).await
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        let mut conn = connect::open(&self.params, Role::Write, Vec::new()).await?;
        let mut tx = conn.start_transaction(TxOpts::default()).await.map_err(|e| self.err(&e))?;
        let mut counts = Vec::new();
        for statement in statements {
            let statement = statement?;
            tx.exec_drop(statement.sql.as_str(), bind(&statement.params)).await.map_err(|e| self.err(&e))?;
            let affected = tx.affected_rows();
            check_affected(&statement, affected)?;
            counts.push(affected);
        }
        tx.commit().await.map_err(|e| self.err(&e))?;
        let _ = conn.disconnect().await;
        Ok(counts)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        let _ = self.catalog.clone().disconnect().await;
    }
}
