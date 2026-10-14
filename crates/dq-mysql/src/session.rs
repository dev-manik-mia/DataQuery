//! An open MySQL / MariaDB session.
//!
//! User statements run on a dedicated query connection that keeps at most one
//! unfinished result open. Catalog reads, table pages and exports use a small
//! pool of read-only connections, so the explorer keeps working while a query
//! runs. Grid edits and imports use a short-lived connection per transaction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, Session, StatementIter, check_affected};
use dq_core::config::ConnConfig;
use dq_core::edit::BoundStatement;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::TableStructure;
use dq_core::sql::{Dialect, MysqlDialect, SqlFlavor, command_tag, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Params, Pool, QueryResult, Row, TextProtocol, TxOpts, Value};
use tokio::sync::Mutex;

use crate::catalog;
use crate::connect::{self, Flavor, Role};
use crate::error::map_error;
use crate::values::{column_meta, decode_row, text};

const STREAM_BATCH: usize = 1_000;
const MAX_WARNINGS: u32 = 50;

pub struct MySession {
    /// Connection settings with `prefer` TLS settled by the first connection.
    config: ConnConfig,
    /// Kept in memory only to open catalog and edit connections.
    password: Option<String>,
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
    timeout_secs: Option<u32>,
}

/// Reads the next row of the open result on `conn`.
///
/// `QueryResult::next` reads one row at a time and leaves the result open, so a
/// later call can continue where this one stopped. (`QueryResult::stream`
/// would discard the unread rows when dropped.)
async fn next_row(conn: &mut Conn) -> mysql_async::Result<Option<Row>> {
    QueryResult::<'_, '_, TextProtocol>::new(conn).next().await
}

impl MySession {
    pub(crate) async fn open(config: &ConnConfig, password: Option<&str>) -> DbResult<Self> {
        let opened = connect::open_query(config, password).await?;
        let config = connect::resolved(config, &opened.info);
        Ok(Self {
            catalog: connect::catalog_pool(&config, password, opened.flavor),
            query_thread: opened.conn.id(),
            query: Mutex::new(QueryConnection { conn: opened.conn, pending: None, timeout_secs: None }),
            config,
            password: password.map(str::to_owned),
            info: opened.info,
            flavor: opened.flavor,
            running: AtomicBool::new(false),
        })
    }

    fn err(&self, e: &mysql_async::Error) -> DbError {
        map_error(e, self.password.as_deref())
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
            while let Ok(Some(_)) = next_row(&mut q.conn).await {}
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
        pending: Option<Row>,
        limit: u32,
    ) -> DbResult<(Vec<CellRow>, Option<Row>)> {
        let decode = |row: &Row| decode_row(row, row.columns_ref(), CellLimits::GRID);
        let mut rows = Vec::with_capacity(limit.min(1024) as usize);
        rows.extend(pending.as_ref().map(decode));
        while rows.len() < limit as usize {
            match next_row(conn).await.map_err(|e| self.err(&e))? {
                Some(row) => rows.push(decode(&row)),
                None => return Ok((rows, None)),
            }
        }
        Ok((rows, next_row(conn).await.map_err(|e| self.err(&e))?))
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

    async fn run(&self, q: &mut QueryConnection, sql: &str, options: ExecOptions) -> DbResult<StatementResult> {
        let started = Instant::now();
        let result = q.conn.query_iter(sql).await.map_err(|e| self.err(&e))?;
        let columns = result.columns().map(|c| column_meta(&c)).unwrap_or_default();
        let affected = result.affected_rows();
        let command = command_tag(sql, SqlFlavor::Mysql);
        if columns.is_empty() {
            result.drop_result().await.map_err(|e| self.err(&e))?;
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
        let (rows, pending) = self.read_rows(&mut q.conn, None, options.row_limit).await?;
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

    /// Runs a read-only query on a pooled catalog connection.
    async fn read_all(&self, sql: &str, limits: CellLimits) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        let mut conn = self.catalog().await?;
        let mut result = conn.query_iter(sql).await.map_err(|e| self.err(&e))?;
        let columns = result.columns().unwrap_or_default();
        let rows: Vec<Row> = result.collect().await.map_err(|e| self.err(&e))?;
        Ok((column_meta(&columns), rows.iter().map(|r| decode_row(r, &columns, limits)).collect()))
    }
}

fn params(statement: &BoundStatement) -> Params {
    if statement.params.is_empty() {
        return Params::Empty;
    }
    Params::Positional(
        statement
            .params
            .iter()
            .map(|p| p.as_ref().map_or(Value::NULL, |v| Value::Bytes(v.as_bytes().to_vec())))
            .collect(),
    )
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

    async fn execute(&self, sql: &str, options: ExecOptions) -> DbResult<StatementResult> {
        // The driver always enables multi-statement mode; never let one call run two.
        if split_statements(sql, SqlFlavor::Mysql).len() > 1 {
            return Err(DbError::invalid("Run one statement at a time."));
        }
        let options = options.clamped();
        let mut q = self.query.lock().await;
        self.close_cursor(&mut q).await;
        self.set_timeout(&mut q, options.timeout_secs).await?;
        self.running.store(true, Ordering::SeqCst);
        let result = self.run(&mut q, sql, options).await;
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
        let read = self.read_rows(&mut q.conn, Some(pending), limit.clamp(1, ExecOptions::MAX_ROW_LIMIT)).await;
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
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        self.read_all(sql, limits).await
    }

    async fn stream_query(&self, _database: &str, sql: &str, on_batch: &mut BatchFn<'_>) -> DbResult<u64> {
        let mut conn = self.catalog().await?;
        let result = conn.query_iter(sql).await.map_err(|e| self.err(&e))?;
        let columns = result.columns().unwrap_or_default();
        let meta = column_meta(&columns);
        drop(result);
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        while let Some(row) = next_row(&mut conn).await.map_err(|e| self.err(&e))? {
            batch.push(decode_row(&row, &columns, CellLimits::FULL));
            if batch.len() == STREAM_BATCH {
                total += batch.len() as u64;
                on_batch(&meta, std::mem::take(&mut batch))?;
            }
        }
        total += batch.len() as u64;
        on_batch(&meta, batch)?;
        Ok(total)
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::row_estimate(&mut self.catalog().await?, object).await
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        let mut conn = connect::open(&self.config, self.password.as_deref(), Role::Write, Vec::new()).await?;
        let mut tx = conn.start_transaction(TxOpts::default()).await.map_err(|e| self.err(&e))?;
        let mut counts = Vec::new();
        for statement in statements {
            let statement = statement?;
            tx.exec_drop(statement.sql.as_str(), params(&statement)).await.map_err(|e| self.err(&e))?;
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
