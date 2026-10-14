//! The query connection, owned by one task.
//!
//! A TDS result borrows its connection, so the task that reads a result also
//! owns the connection: it keeps the unread rest of a result open between
//! "fetch more" requests, and a new statement first stops it with an
//! attention packet (`cancel_query`) instead of reading every remaining row.
//! Cancelling a running statement sends the same packet. If the connection
//! breaks, the next request opens a new one.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dq_core::adapter::ConnectParams;
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, StatementResult};
use dq_core::sql::{SqlFlavor, StatementKind, analyze, command_tag};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use futures_util::TryStreamExt;
use tiberius::{QueryItem, QueryStream, ToSql};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::connect::{self, Conn};
use crate::error::map_error;
use crate::values::{column_meta, decode_row};

pub enum Command {
    Execute { sql: String, params: Vec<Option<String>>, options: ExecOptions, reply: oneshot::Sender<DbResult<StatementResult>> },
    FetchMore { limit: u32, reply: oneshot::Sender<DbResult<RowsPage>> },
    /// Runs `setup`, then `sql`, then `teardown` as separate batches and returns every text value of every result set.
    Batches { setup: String, sql: String, params: Vec<Option<String>>, teardown: String, timeout_secs: u32, reply: oneshot::Sender<DbResult<Vec<Vec<String>>>> },
}

/// Lets another task interrupt the statement the worker is running.
#[derive(Default)]
pub struct CancelSignal {
    running: AtomicBool,
    requested: AtomicBool,
    notify: Notify,
}

impl CancelSignal {
    /// Returns false when nothing was running.
    pub fn cancel(&self) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return false;
        }
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        true
    }

    fn start(&self) {
        self.requested.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    async fn requested(&self) {
        loop {
            let notified = self.notify.notified();
            if self.requested.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Why a running request stopped early.
enum Interrupt {
    Cancelled,
    TimedOut,
}

impl Interrupt {
    fn error(&self, secs: u32) -> DbError {
        match self {
            Self::Cancelled => DbError::new(ErrorKind::Cancelled, "The statement was cancelled."),
            Self::TimedOut => DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {secs} s and was stopped."))
                .with_action(Some("Narrow the query or raise the statement timeout in Settings.")),
        }
    }
}

/// Text parameters bound as `nvarchar`; the SQL converts them.
fn binds(params: &[Option<String>]) -> Vec<&dyn ToSql> {
    params.iter().map(|p| p as &dyn ToSql).collect()
}

/// Runs `fut` until it finishes, the user cancels, or the timeout passes.
async fn guarded<T>(signal: &CancelSignal, timeout_secs: u32, fut: impl Future<Output = T>) -> Result<T, Interrupt> {
    let limit = if timeout_secs == 0 { Duration::from_secs(u64::MAX / 4) } else { Duration::from_secs(u64::from(timeout_secs)) };
    tokio::select! {
        out = fut => Ok(out),
        () = signal.requested() => Err(Interrupt::Cancelled),
        () = tokio::time::sleep(limit) => Err(Interrupt::TimedOut),
    }
}

/// A statement whose result is read as rows (rather than as a row count).
fn returns_rows(sql: &str) -> bool {
    let Some(info) = analyze(sql, SqlFlavor::Sqlserver) else { return true };
    let dml = matches!(info.kind, StatementKind::Insert | StatementKind::Update | StatementKind::Delete | StatementKind::Merge);
    // `OUTPUT` returns the changed rows.
    !dml || dq_core::sql::lexer::tokenize(sql, SqlFlavor::Sqlserver).iter().any(|t| t.is_keyword(sql, "OUTPUT"))
}

pub struct Worker {
    params: ConnectParams,
    conn: Option<Conn>,
    signal: Arc<CancelSignal>,
}

impl Worker {
    pub fn spawn(params: ConnectParams, conn: Conn, signal: Arc<CancelSignal>) -> mpsc::Sender<Command> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(Self { params, conn: Some(conn), signal }.run(rx));
        tx
    }

    /// The connection, opened again if the last one broke. The caller puts it back.
    async fn take_conn(&mut self) -> DbResult<Conn> {
        match self.conn.take() {
            Some(conn) => Ok(conn),
            None => connect::open(&self.params, None).await,
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut next = None;
        loop {
            let command = match next.take() {
                Some(c) => c,
                None => match rx.recv().await {
                    Some(c) => c,
                    None => break,
                },
            };
            match command {
                Command::Execute { sql, params, options, reply } => {
                    next = self.execute(&sql, &params, options, reply, &mut rx).await;
                }
                Command::FetchMore { reply, .. } => {
                    let _ = reply.send(Err(DbError::invalid("No more rows: the result was closed by a newer query.")));
                }
                Command::Batches { setup, sql, params, teardown, timeout_secs, reply } => {
                    let result = self.batches(&setup, &sql, &params, &teardown, timeout_secs).await;
                    let _ = reply.send(result);
                }
            }
        }
        if let Some(conn) = self.conn.take() {
            let _ = conn.close().await;
        }
    }

    /// Runs one statement. Returns the command that arrived while its result
    /// was still open, so the caller handles it next.
    async fn execute(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        options: ExecOptions,
        reply: oneshot::Sender<DbResult<StatementResult>>,
        rx: &mut mpsc::Receiver<Command>,
    ) -> Option<Command> {
        let started = Instant::now();
        let command = command_tag(sql, SqlFlavor::Sqlserver);
        let signal = self.signal.clone();
        let secret = self.params.password().map(str::to_owned);
        let err = |e: &tiberius::error::Error| map_error(e, secret.as_deref());
        let mut conn = match self.take_conn().await {
            Ok(c) => c,
            Err(e) => {
                let _ = reply.send(Err(e));
                return None;
            }
        };
        signal.start();
        if !returns_rows(sql) {
            let outcome = guarded(&signal, options.timeout_secs, async {
                conn.execute(sql, &binds(params)).await.map_err(|e| err(&e))
            })
            .await;
            signal.stop();
            let result = match outcome {
                Ok(Ok(done)) => Ok(StatementResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    has_more: false,
                    command,
                    // Trigger statements report their counts first; the statement's own count is last.
                    affected: Some(done.rows_affected().last().copied().unwrap_or(0)),
                    duration_ms: started.elapsed().as_millis() as u64,
                    notices: Vec::new(),
                }),
                Ok(Err(e)) => Err(e),
                Err(stop) => {
                    self.conn = interrupt(conn).await;
                    let _ = reply.send(Err(stop.error(options.timeout_secs)));
                    return None;
                }
            };
            self.conn = Some(conn);
            let _ = reply.send(result);
            return None;
        }

        let unfinished =
            stream_rows(&mut conn, &signal, sql, params, options, started, command, reply, rx, &err).await;
        let next = match unfinished {
            Unfinished::No => {
                self.conn = Some(conn);
                return None;
            }
            Unfinished::Yes(next) => next,
        };
        // Whatever the server still sends is not needed: tell it to stop.
        self.conn = interrupt(conn).await;
        next
    }

    /// Plan requests: `SET SHOWPLAN_XML ON` must be alone in its batch.
    async fn batches(
        &mut self,
        setup: &str,
        sql: &str,
        params: &[Option<String>],
        teardown: &str,
        timeout_secs: u32,
    ) -> DbResult<Vec<Vec<String>>> {
        let signal = self.signal.clone();
        let secret = self.params.password().map(str::to_owned);
        let err = |e: tiberius::error::Error| map_error(&e, secret.as_deref());
        let mut conn = self.take_conn().await?;
        if let Err(e) = async { conn.simple_query(setup).await?.into_results().await }.await {
            self.conn = Some(conn);
            return Err(err(e));
        }
        signal.start();
        let run = guarded(&signal, timeout_secs, async {
            let bound = binds(params);
            let stream =
                if params.is_empty() { conn.simple_query(sql).await } else { conn.query(sql, &bound).await }.map_err(err)?;
            stream.into_results().await.map_err(err)
        })
        .await;
        signal.stop();
        let results = match run {
            Ok(r) => r,
            Err(stop) => {
                let Some(conn) = interrupt(conn).await else { return Err(stop.error(timeout_secs)) };
                self.conn = Some(conn);
                Err(stop.error(timeout_secs))
            }
        };
        if let Some(conn) = self.conn.as_mut() {
            if let Ok(s) = conn.simple_query(teardown).await {
                let _ = s.into_results().await;
            }
        }
        Ok(results?
            .iter()
            .flatten()
            .map(|row| row.cells().filter_map(|(_, d)| crate::values::cell_text(d, CellLimits::FULL)).collect())
            .collect())
    }
}

/// Whether a result was left unread on the connection, and the command that
/// arrived while it was open.
enum Unfinished {
    No,
    Yes(Option<Command>),
}

/// Starts a row-returning statement, replies with its first page, then serves
/// "fetch more" requests until the result ends or another request arrives.
#[allow(clippy::too_many_arguments)]
async fn stream_rows(
    conn: &mut Conn,
    signal: &CancelSignal,
    sql: &str,
    params: &[Option<String>],
    options: ExecOptions,
    started: Instant,
    command: String,
    reply: oneshot::Sender<DbResult<StatementResult>>,
    rx: &mut mpsc::Receiver<Command>,
    err: &impl Fn(&tiberius::error::Error) -> DbError,
) -> Unfinished {
    let opened = guarded(signal, options.timeout_secs, async {
        let bound = binds(params);
        let mut stream = if params.is_empty() { conn.simple_query(sql).await } else { conn.query(sql, &bound).await }
            .map_err(|e| err(&e))?;
        let mut pending = None;
        let first = read_page(&mut stream, &mut pending, None, options.row_limit, err).await?;
        Ok::<_, DbError>((stream, pending, first))
    })
    .await;
    signal.stop();
    let (mut stream, mut pending, page) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => {
            let _ = reply.send(Err(e));
            return Unfinished::Yes(None);
        }
        Err(stop) => {
            let _ = reply.send(Err(stop.error(options.timeout_secs)));
            return Unfinished::Yes(None);
        }
    };
    let mut columns = page.columns.clone();
    let has_more = page.more;
    let notices = page.notices();
    let unread = page.later_results;
    let _ = reply.send(Ok(StatementResult {
        columns: page.columns,
        rows: page.rows,
        has_more,
        command,
        affected: None,
        duration_ms: started.elapsed().as_millis() as u64,
        notices,
    }));
    if !has_more {
        return if unread { Unfinished::Yes(None) } else { Unfinished::No };
    }
    // Keep the result open for "fetch more" until another request arrives.
    while let Some(command) = rx.recv().await {
        let Command::FetchMore { limit, reply } = command else {
            return Unfinished::Yes(Some(command));
        };
        let started = Instant::now();
        signal.start();
        let limit = limit.clamp(1, ExecOptions::MAX_ROW_LIMIT);
        let read = guarded(signal, options.timeout_secs, read_page(&mut stream, &mut pending, Some(&columns), limit, err)).await;
        signal.stop();
        match read {
            Ok(Ok(page)) => {
                let more = page.more;
                let unread = page.later_results;
                columns = page.columns;
                let duration_ms = started.elapsed().as_millis() as u64;
                let _ = reply.send(Ok(RowsPage { rows: page.rows, has_more: more, duration_ms }));
                if !more {
                    return if unread { Unfinished::Yes(None) } else { Unfinished::No };
                }
            }
            Ok(Err(e)) => {
                let _ = reply.send(Err(e));
                return Unfinished::Yes(None);
            }
            Err(stop) => {
                let _ = reply.send(Err(stop.error(options.timeout_secs)));
                return Unfinished::Yes(None);
            }
        }
    }
    Unfinished::Yes(None)
}

/// Stops whatever the server is still sending. Returns the connection, or
/// `None` when it broke (the next request opens a new one).
async fn interrupt(mut conn: Conn) -> Option<Conn> {
    conn.cancel_query().await.ok().map(|()| conn)
}

/// Rows read from an open result.
struct Page {
    columns: Vec<ColumnMeta>,
    rows: Vec<CellRow>,
    /// More rows of this result set remain.
    more: bool,
    /// Another result set followed and was not read.
    later_results: bool,
}

impl Page {
    fn notices(&self) -> Vec<String> {
        if self.later_results {
            vec!["The batch returned more than one result set; only the first is shown.".to_owned()]
        } else {
            Vec::new()
        }
    }
}

/// Reads up to `limit` rows of the first result set, then one more item to
/// learn whether rows remain; a row read ahead waits in `pending`. `columns`
/// is `None` before the result's metadata was read. Statements that return no
/// rows produce no columns.
async fn read_page(
    stream: &mut QueryStream<'_>,
    pending: &mut Option<tiberius::Row>,
    columns: Option<&Vec<ColumnMeta>>,
    limit: u32,
    err: &impl Fn(&tiberius::error::Error) -> DbError,
) -> DbResult<Page> {
    let mut page =
        Page { columns: columns.cloned().unwrap_or_default(), rows: Vec::new(), more: false, later_results: false };
    let mut started = columns.is_some();
    if let Some(row) = pending.take() {
        page.rows.push(decode_row(&row, CellLimits::GRID));
    }
    loop {
        match stream.try_next().await.map_err(|e| err(&e))? {
            None => return Ok(page),
            Some(QueryItem::Metadata(_)) if started => {
                page.later_results = true;
                return Ok(page);
            }
            Some(QueryItem::Metadata(meta)) => {
                page.columns = column_meta(meta.columns());
                started = true;
            }
            Some(QueryItem::Row(row)) if page.rows.len() >= limit as usize => {
                *pending = Some(row);
                page.more = true;
                return Ok(page);
            }
            Some(QueryItem::Row(row)) => page.rows.push(decode_row(&row, CellLimits::GRID)),
        }
    }
}
