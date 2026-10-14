//! The query connection, owned by one thread. A result keeps its Arrow
//! stream open between "fetch more" requests; a new request drops it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, StatementResult};
use dq_core::sql::{SqlFlavor, StatementKind, analyze, command_tag};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::{Connection, InterruptHandle, Statement, params_from_iter};
use tokio::sync::{mpsc, oneshot};

use crate::error::map_error;
use crate::values::{batch_rows, column_meta};

pub enum Command {
    Execute { sql: String, params: Vec<Option<String>>, options: ExecOptions, reply: oneshot::Sender<DbResult<StatementResult>> },
    FetchMore { limit: u32, reply: oneshot::Sender<DbResult<RowsPage>> },
}

/// Interrupts the running statement, from the cancel button or the timeout.
pub struct Interrupter {
    handle: Arc<InterruptHandle>,
    running: AtomicBool,
    /// Bumped per statement, so a late timeout never stops the next one.
    generation: std::sync::atomic::AtomicU64,
    timed_out: AtomicBool,
}

impl Interrupter {
    pub fn cancel(&self) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return false;
        }
        self.handle.interrupt();
        true
    }

    fn start(self: &Arc<Self>, timeout_secs: u32) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.timed_out.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        if timeout_secs > 0 {
            let me = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(u64::from(timeout_secs)));
                if me.running.load(Ordering::SeqCst) && me.generation.load(Ordering::SeqCst) == generation {
                    me.timed_out.store(true, Ordering::SeqCst);
                    me.handle.interrupt();
                }
            });
        }
    }

    /// Ends the running statement; turns an interrupt from the timer into a timeout.
    fn finish<T>(&self, result: DbResult<T>, timeout_secs: u32) -> DbResult<T> {
        self.running.store(false, Ordering::SeqCst);
        match result {
            Err(e) if e.kind == ErrorKind::Cancelled && self.timed_out.load(Ordering::SeqCst) => Err(DbError::new(
                ErrorKind::Timeout,
                format!("The statement ran longer than {timeout_secs} s and was stopped."),
            )
            .with_action(Some("Narrow the query or raise the statement timeout in Settings."))),
            other => other,
        }
    }
}

/// Starts the worker thread on `conn`.
pub fn spawn(conn: Connection) -> (mpsc::Sender<Command>, Arc<Interrupter>) {
    let interrupter = Arc::new(Interrupter {
        handle: conn.interrupt_handle(),
        running: AtomicBool::new(false),
        generation: std::sync::atomic::AtomicU64::new(0),
        timed_out: AtomicBool::new(false),
    });
    let (tx, mut rx) = mpsc::channel(8);
    let worker_interrupter = interrupter.clone();
    std::thread::Builder::new()
        .name("duckdb-query".into())
        .spawn(move || {
            let mut next: Option<Command> = None;
            loop {
                let command = match next.take() {
                    Some(c) => c,
                    None => match rx.blocking_recv() {
                        Some(c) => c,
                        None => break,
                    },
                };
                match command {
                    Command::Execute { sql, params, options, reply } => {
                        next = execute(&conn, &worker_interrupter, &sql, &params, options, reply, &mut rx);
                    }
                    Command::FetchMore { reply, .. } => {
                        let _ = reply.send(Err(DbError::invalid("No more rows: the result was closed by a newer query.")));
                    }
                }
            }
        })
        .expect("the DuckDB query thread starts");
    (tx, interrupter)
}

/// Rows read from Arrow batches but not handed out yet.
struct Buffer {
    rows: VecDeque<CellRow>,
}

impl Buffer {
    /// Takes up to `limit` rows, reading more batches as needed. Returns the
    /// rows and whether more remain.
    fn take(&mut self, statement: &Statement<'_>, limit: usize) -> DbResult<(Vec<CellRow>, bool)> {
        // `step` reports errors (an interrupt, a failed cast) instead of panicking like the Arrow iterator.
        let next = || -> DbResult<Option<RecordBatch>> {
            Ok(statement.step().map_err(|e| map_error(&e))?.map(|array| RecordBatch::from(&array)))
        };
        let mut out = Vec::with_capacity(limit.min(4096));
        loop {
            while out.len() < limit {
                match self.rows.pop_front() {
                    Some(row) => out.push(row),
                    None => break,
                }
            }
            if out.len() == limit {
                // Make sure at least one more row exists before saying so.
                while self.rows.is_empty() {
                    match next()? {
                        Some(batch) => self.rows.extend(batch_rows(&batch, CellLimits::GRID)),
                        None => return Ok((out, false)),
                    }
                }
                return Ok((out, true));
            }
            match next()? {
                Some(batch) => self.rows.extend(batch_rows(&batch, CellLimits::GRID)),
                None => return Ok((out, false)),
            }
        }
    }
}

fn is_write(sql: &str) -> bool {
    analyze(sql, SqlFlavor::Duckdb).is_some_and(|s| {
        matches!(s.kind, StatementKind::Insert | StatementKind::Update | StatementKind::Delete | StatementKind::Merge)
    })
}

/// Runs one statement and serves "fetch more" until another command arrives.
fn execute(
    conn: &Connection,
    interrupter: &Arc<Interrupter>,
    sql: &str,
    params: &[Option<String>],
    options: ExecOptions,
    reply: oneshot::Sender<DbResult<StatementResult>>,
    rx: &mut mpsc::Receiver<Command>,
) -> Option<Command> {
    let started = Instant::now();
    let command = command_tag(sql, SqlFlavor::Duckdb);
    let mut statement = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(Err(map_error(&e)));
            return None;
        }
    };
    interrupter.start(options.timeout_secs);
    // Execute, then read batches with `step` below.
    let opened = statement.query_arrow(params_from_iter(params.iter())).map(|batches| batches.get_schema()).map_err(|e| map_error(&e));
    let schema = match opened {
        Ok(schema) => schema,
        Err(e) => {
            let _ = reply.send(interrupter.finish(Err(e), options.timeout_secs));
            return None;
        }
    };
    let columns: Vec<ColumnMeta> = column_meta(&schema);
    let mut buffer = Buffer { rows: VecDeque::new() };
    let first = buffer.take(&statement, options.row_limit as usize);
    let first = interrupter.finish(first, options.timeout_secs);
    let (rows, has_more) = match first {
        Ok(r) => r,
        Err(e) => {
            let _ = reply.send(Err(e));
            return None;
        }
    };
    // Writes report their row count as a single `Count` value.
    let counted = columns.len() == 1 && columns[0].name == "Count" && is_write(sql);
    let result = if counted {
        let affected = rows.first().and_then(|r| r.first()).and_then(dq_core::value::Cell::to_text).and_then(|t| t.parse().ok());
        StatementResult { columns: Vec::new(), rows: Vec::new(), has_more: false, command, affected, duration_ms: started.elapsed().as_millis() as u64, notices: Vec::new() }
    } else {
        StatementResult { columns, rows, has_more, command, affected: None, duration_ms: started.elapsed().as_millis() as u64, notices: Vec::new() }
    };
    let keep_open = result.has_more;
    let _ = reply.send(Ok(result));
    if !keep_open {
        return None;
    }
    while let Some(command) = rx.blocking_recv() {
        let Command::FetchMore { limit, reply } = command else { return Some(command) };
        let started = Instant::now();
        interrupter.start(options.timeout_secs);
        let page = buffer.take(&statement, limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize);
        let page = interrupter.finish(page, options.timeout_secs);
        let more = page.as_ref().is_ok_and(|(_, more)| *more);
        let _ = reply.send(page.map(|(rows, has_more)| RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 }));
        if !more {
            return None;
        }
    }
    None
}
