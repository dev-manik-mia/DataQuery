//! A remote `SQLite` database (libSQL, Cloudflare D1) behind a [`Transport`].
//!
//! The catalog, values, plans and DDL are `SQLite`'s own, shared with the
//! local adapter. Services return a statement's rows at once (bounded by
//! the service and [`crate::http`]); rows past the editor's row limit are
//! kept for "fetch more". Cancel and timeout stop waiting for the reply;
//! the service cannot be told to stop a statement it already runs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult, TextRow};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, SqlFlavor, command_tag, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use dq_sqlite::catalog::{self, CatalogSource};
use dq_sqlite::counts_rows;
use dq_sqlite::values::Value;
use tokio::sync::{Mutex, Notify};

use crate::http::timed_out;
use crate::transport::{Column, Output, Transport};
use crate::values::{column_meta, row, text_row};

const STREAM_BATCH: usize = 2_048;
/// Catalog reads give up after this long.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// Rows of the last result not shown yet.
struct Pending {
    columns: Vec<Column>,
    rows: VecDeque<Vec<Value>>,
}

pub struct RemoteSession<T: Transport> {
    transport: Arc<T>,
    info: ServerInfo,
    read_only: bool,
    pending: Mutex<Option<Pending>>,
    /// Wakes the running editor statement to stop waiting for it.
    running: SyncMutex<Option<Arc<Notify>>>,
}

/// Catalog queries through the transport.
struct Catalog<'a, T>(&'a T);

impl<T: Transport> CatalogSource for Catalog<'_, T> {
    async fn rows(&self, sql: &str, params: &[&str]) -> DbResult<Vec<TextRow>> {
        let params: Vec<Option<String>> = params.iter().map(|p| Some((*p).to_owned())).collect();
        let output = self.0.query(sql, &params, Some(CATALOG_TIMEOUT)).await?;
        Ok(output.rows.iter().map(|r| text_row(r)).collect())
    }

    fn attaches(&self) -> bool {
        false
    }

    fn internal_prefixes(&self) -> &'static [&'static str] {
        self.0.internal_prefixes()
    }
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

fn timeout_of(options: ExecOptions) -> Option<Duration> {
    (options.timeout_secs > 0).then(|| Duration::from_secs(u64::from(options.timeout_secs)))
}

/// The editor's view of a statement's output: the first `row_limit` rows,
/// and the rest to fetch later.
fn statement_result(sql: &str, output: Output, row_limit: u32, started: Instant) -> (StatementResult, Option<Pending>) {
    let columns = column_meta(&output);
    let mut rest: VecDeque<Vec<Value>> = output.rows.into();
    let first: Vec<CellRow> =
        rest.drain(..rest.len().min(row_limit as usize)).map(|r| row(&r, &output.columns, CellLimits::GRID)).collect();
    let has_more = !rest.is_empty();
    // Statements without result columns (writes, DDL) and `… RETURNING` report the rows they changed.
    let affected = counts_rows(sql).then_some(output.affected);
    let result = StatementResult {
        columns,
        rows: first,
        has_more,
        command: command_tag(sql, SqlFlavor::Sqlite),
        affected,
        duration_ms: started.elapsed().as_millis() as u64,
        notices: Vec::new(),
    };
    (result, has_more.then_some(Pending { columns: output.columns, rows: rest }))
}

impl<T: Transport> RemoteSession<T> {
    /// Checks the service answers and reads its `SQLite` version.
    pub async fn open(transport: Arc<T>, params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let timeout = Duration::from_secs(u64::from(params.config.connect_timeout_secs));
        let version = transport.query("SELECT sqlite_version()", &[], Some(timeout)).await?;
        let version = version.rows.first().and_then(|r| text_row(r).text(0)).unwrap_or_default();
        let info = ServerInfo {
            engine: params.config.engine,
            version: format!("{} (SQLite {version})", transport.product()),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: transport.http().tls.clone(),
            user: params.config.username.clone(),
            database: "main".into(),
            read_only: params.config.read_only,
        };
        Ok(Self {
            transport,
            read_only: info.read_only,
            info,
            pending: Mutex::new(None),
            running: SyncMutex::new(None),
        })
    }

    fn catalog(&self) -> Catalog<'_, T> {
        Catalog(&self.transport)
    }

    fn set_running(&self, stop: Option<Arc<Notify>>) {
        if let Ok(mut running) = self.running.lock() {
            *running = stop;
        }
    }

    fn check_writes(&self, sql: &str) -> DbResult<()> {
        if self.read_only && split_statements(sql, SqlFlavor::Sqlite).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        Ok(())
    }

    /// Runs `work` until it finishes, the timeout passes or Cancel is pressed.
    async fn cancellable<R>(&self, timeout: Option<Duration>, work: impl Future<Output = DbResult<R>>) -> DbResult<R> {
        let stop = Arc::new(Notify::new());
        self.set_running(Some(stop.clone()));
        let limited = async {
            match timeout {
                Some(limit) => {
                    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| Err(timed_out(limit.as_secs())))
                }
                None => work.await,
            }
        };
        let outcome = tokio::select! {
            result = limited => result,
            () = stop.notified() => Err(DbError::new(ErrorKind::Cancelled, "The statement was cancelled.").with_action(Some(
                "The service may still finish it; an open transaction was rolled back.",
            ))),
        };
        self.set_running(None);
        outcome
    }

    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        timeout: Option<Duration>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let output = self.transport.query(sql, params, timeout).await?;
        let meta = column_meta(&output);
        let total = output.rows.len() as u64;
        let mut rows = output.rows.chunks(STREAM_BATCH).peekable();
        if rows.peek().is_none() {
            return on_batch(&meta, Vec::new()).map(|()| 0);
        }
        for batch in rows {
            on_batch(&meta, batch.iter().map(|r| row(r, &output.columns, limits)).collect())?;
        }
        Ok(total)
    }
}

#[async_trait]
impl<T: Transport> Session for RemoteSession<T> {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        self.transport.dialect()
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        catalog::children(&self.catalog(), parent).await
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&self.catalog(), query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        self.check_writes(sql)?;
        let options = options.clamped();
        let mut pending = self.pending.lock().await;
        *pending = None;
        let started = Instant::now();
        let output =
            self.cancellable(timeout_of(options), self.transport.execute(sql, params, timeout_of(options))).await?;
        let (result, rest) = statement_result(sql, output, options.row_limit, started);
        *pending = rest;
        Ok(result)
    }

    async fn execute_script(
        &self,
        statements: &[&str],
        options: ExecOptions,
    ) -> Option<DbResult<Vec<StatementResult>>> {
        if let Err(e) = statements.iter().try_for_each(|s| self.check_writes(s)) {
            return Some(Err(e));
        }
        let options = options.clamped();
        let mut pending = self.pending.lock().await;
        *pending = None;
        let started = Instant::now();
        let stop = Arc::new(Notify::new());
        self.set_running(Some(stop.clone()));
        let outcome = tokio::select! {
            outcome = self.transport.script(statements, timeout_of(options)) => outcome,
            () = stop.notified() => Some(Err(DbError::new(ErrorKind::Cancelled, "The script was cancelled."))),
        };
        self.set_running(None);
        Some(outcome?.map(|outputs| {
            statements
                .iter()
                .zip(outputs)
                .map(|(sql, output)| {
                    let (mut result, rest) = statement_result(sql, output, options.row_limit, started);
                    if rest.is_some() {
                        result.has_more = false;
                        result.notices.push(format!("Showing the first {} rows of this result.", options.row_limit));
                    }
                    result
                })
                .collect()
        }))
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut slot = self.pending.lock().await;
        let pending =
            slot.as_mut().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let take = pending.rows.len().min(limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize);
        let rows = pending.rows.drain(..take).map(|r| row(&r, &pending.columns, CellLimits::GRID)).collect();
        let has_more = !pending.rows.is_empty();
        if !has_more {
            *slot = None;
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let stop = self.running.lock().ok().and_then(|mut r| r.take());
        Ok(stop.map(|s| s.notify_one()).is_some())
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&self.catalog(), object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&self.catalog(), object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(&self.catalog(), object).await
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&self.catalog(), database).await
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
        self.read_batches(sql, params, limits, Some(CATALOG_TIMEOUT), &mut |meta, batch| {
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
        self.read_batches(sql, params, CellLimits::FULL, None, on_batch).await
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        Ok(catalog::row_estimate(&self.catalog(), object).await)
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let statements = statements.collect::<DbResult<Vec<_>>>()?;
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        self.transport.transaction(&statements).await
    }

    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        if analyze {
            return Err(dq_sqlite::explain::no_analyze());
        }
        let timeout = Some(Duration::from_secs(u64::from(timeout_secs.max(1))));
        let output = self.transport.query(&dq_sqlite::explain::statement(sql), params, timeout).await?;
        let rows = output
            .rows
            .iter()
            .map(|r| {
                let r = text_row(r);
                (r.number(0).unwrap_or(0), r.number(1).unwrap_or(0), r.string(3))
            })
            .collect();
        Ok(dq_sqlite::explain::plan(rows))
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        self.transport.close().await;
    }
}
