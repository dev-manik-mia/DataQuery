//! The query connection, owned by one thread. A result keeps its statement
//! open between "fetch more" requests; a new request closes it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dq_core::result::{ExecOptions, Row as CellRow, RowsPage, StatementResult};
use dq_core::sql::{SqlFlavor, command_tag};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use rusqlite::{Connection, InterruptHandle, Rows, params_from_iter};
use tokio::sync::{mpsc, oneshot};

use crate::counts_rows;
use crate::error::map_error;
use crate::values::{self, column_meta};

pub enum Command {
    Execute {
        sql: String,
        params: Vec<Option<String>>,
        options: ExecOptions,
        reply: oneshot::Sender<DbResult<StatementResult>>,
    },
    FetchMore {
        limit: u32,
        reply: oneshot::Sender<DbResult<RowsPage>>,
    },
}

/// Interrupts the running statement, from the cancel button or the timeout.
pub struct Interrupter {
    handle: InterruptHandle,
    running: AtomicBool,
    /// Bumped per statement, so a late timeout never stops the next one.
    generation: AtomicU64,
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
        handle: conn.get_interrupt_handle(),
        running: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        timed_out: AtomicBool::new(false),
    });
    let (tx, mut rx) = mpsc::channel(8);
    let worker_interrupter = interrupter.clone();
    std::thread::Builder::new()
        .name("sqlite-query".into())
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
                        let _ =
                            reply.send(Err(DbError::invalid("No more rows: the result was closed by a newer query.")));
                    }
                }
            }
        })
        .expect("the SQLite query thread starts");
    (tx, interrupter)
}

/// Reads up to `limit` rows, plus one held back to tell whether more remain.
pub fn take(
    rows: &mut Rows<'_>,
    declared: &[String],
    pending: &mut Option<CellRow>,
    limit: usize,
    limits: CellLimits,
) -> DbResult<(Vec<CellRow>, bool)> {
    let mut out = Vec::with_capacity(limit.min(4096));
    if let Some(row) = pending.take() {
        out.push(row);
    }
    while out.len() < limit {
        match rows.next().map_err(|e| map_error(&e))? {
            Some(row) => out.push(values::row(row, declared, limits).map_err(|e| map_error(&e))?),
            None => return Ok((out, false)),
        }
    }
    *pending = match rows.next().map_err(|e| map_error(&e))? {
        Some(row) => Some(values::row(row, declared, limits).map_err(|e| map_error(&e))?),
        None => None,
    };
    Ok((out, pending.is_some()))
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
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
    let command = command_tag(sql, SqlFlavor::Sqlite);
    let mut statement = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(Err(map_error(&e)));
            return None;
        }
    };
    let columns = column_meta(&statement);
    interrupter.start(options.timeout_secs);
    // Statements without result columns (writes, DDL) report how many rows they changed.
    if columns.is_empty() {
        let done = statement.execute(params_from_iter(params.iter())).map_err(|e| map_error(&e));
        let done = interrupter.finish(done, options.timeout_secs);
        let _ = reply.send(done.map(|changed| StatementResult {
            columns: Vec::new(),
            rows: Vec::new(),
            has_more: false,
            command,
            affected: counts_rows(sql).then_some(changed as u64),
            duration_ms: elapsed_ms(started),
            notices: Vec::new(),
        }));
        return None;
    }
    let declared: Vec<String> = columns.iter().map(|c| c.type_name.clone()).collect();
    let mut rows = match statement.query(params_from_iter(params.iter())) {
        Ok(rows) => rows,
        Err(e) => {
            let _ = reply.send(interrupter.finish(Err(map_error(&e)), options.timeout_secs));
            return None;
        }
    };
    let mut pending = None;
    let first = take(&mut rows, &declared, &mut pending, options.row_limit as usize, CellLimits::GRID);
    let (rows_out, has_more) = match interrupter.finish(first, options.timeout_secs) {
        Ok(r) => r,
        Err(e) => {
            let _ = reply.send(Err(e));
            return None;
        }
    };
    // `INSERT … RETURNING` and friends return rows and change rows.
    let affected = (counts_rows(sql) && !has_more).then(|| conn.changes());
    let _ = reply.send(Ok(StatementResult {
        columns,
        rows: rows_out,
        has_more,
        command,
        affected,
        duration_ms: elapsed_ms(started),
        notices: Vec::new(),
    }));
    if !has_more {
        return None;
    }
    while let Some(command) = rx.blocking_recv() {
        let Command::FetchMore { limit, reply } = command else { return Some(command) };
        let started = Instant::now();
        interrupter.start(options.timeout_secs);
        let limit = limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize;
        let page = take(&mut rows, &declared, &mut pending, limit, CellLimits::GRID);
        let page = interrupter.finish(page, options.timeout_secs);
        let more = page.as_ref().is_ok_and(|(_, more)| *more);
        let _ = reply.send(page.map(|(rows, has_more)| RowsPage { rows, has_more, duration_ms: elapsed_ms(started) }));
        if !more {
            return None;
        }
    }
    None
}
