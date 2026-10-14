//! An open Cassandra / `ScyllaDB` session.
//!
//! The driver multiplexes requests over its connection pools, so catalog reads
//! never wait for a user query. A user query keeps its paging state in memory
//! for "fetch more"; it is never stored. CQL has no server-side cancel: cancel
//! and timeouts stop waiting for the request and drop it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, SessionOption, StatementIter, check_affected};
use dq_core::edit::BoundStatement;
use dq_core::explain::{ExplainResult, PlanNode};
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, KeyRole, KeyRoleKind, SchemaModel, TableStructure};
use dq_core::sql::cql::{AnalysisLevel, analyze_select, select_target};
use dq_core::sql::{CqlDialect, Dialect, SqlFlavor, command_tag, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use futures_util::{StreamExt, TryStreamExt, stream};
use scylla::client::session::Session as Driver;
use scylla::response::query_result::QueryResult;
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::batch::{Batch, BatchType};
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::{Consistency, SerialConsistency, Statement};
use scylla::value::{CqlValue, Row};
use tokio::sync::{Mutex, Notify};

use crate::catalog;
use crate::client::{self, exec_error, rows_error};
use crate::values::{self, column_meta};

const STREAM_PAGE: i32 = 1_000;
/// Statements per `run_in_transaction` that still go in one logged batch.
const MAX_BATCH: usize = 20;
const IMPORT_CONCURRENCY: usize = 16;
const PREPARED_CACHE: usize = 512;

pub const CONSISTENCY_LEVELS: [&str; 9] =
    ["ANY", "ONE", "TWO", "THREE", "QUORUM", "ALL", "LOCAL_QUORUM", "EACH_QUORUM", "LOCAL_ONE"];
pub const SERIAL_LEVELS: [&str; 2] = ["SERIAL", "LOCAL_SERIAL"];
pub const PAGE_SIZES: [&str; 4] = ["100", "500", "1000", "5000"];

fn consistency(name: &str) -> Option<Consistency> {
    Some(match name.to_ascii_uppercase().as_str() {
        "ANY" => Consistency::Any,
        "ONE" => Consistency::One,
        "TWO" => Consistency::Two,
        "THREE" => Consistency::Three,
        "QUORUM" => Consistency::Quorum,
        "ALL" => Consistency::All,
        "LOCAL_QUORUM" => Consistency::LocalQuorum,
        "EACH_QUORUM" => Consistency::EachQuorum,
        "LOCAL_ONE" => Consistency::LocalOne,
        _ => return None,
    })
}

fn serial(name: &str) -> Option<SerialConsistency> {
    match name.to_ascii_uppercase().as_str() {
        "SERIAL" => Some(SerialConsistency::Serial),
        "LOCAL_SERIAL" => Some(SerialConsistency::LocalSerial),
        _ => None,
    }
}

/// Per-session settings of the query path, changed from the editor toolbar or cqlsh commands.
#[derive(Debug, Clone)]
struct Settings {
    consistency: String,
    serial: String,
    /// `None`: the editor's row limit (at most 5,000).
    page_size: Option<i32>,
    tracing: bool,
}

/// What to send for the next page of a user query.
#[derive(Clone)]
enum Request {
    Simple(Statement),
    Prepared(PreparedStatement, Vec<Option<CqlValue>>),
}

struct Cursor {
    request: Request,
    paging: Option<PagingState>,
    buffered: VecDeque<CellRow>,
}

struct Page {
    columns: Vec<ColumnMeta>,
    rows: Vec<CellRow>,
    next: Option<PagingState>,
    warnings: Vec<String>,
    tracing_id: Option<uuid::Uuid>,
}

pub struct CqlSession {
    pub(crate) driver: Driver,
    info: ServerInfo,
    pub(crate) scylla: bool,
    read_only: bool,
    settings: SyncMutex<Settings>,
    query: Mutex<Option<Cursor>>,
    running: SyncMutex<Option<Arc<Notify>>>,
    prepared: Mutex<HashMap<String, PreparedStatement>>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

fn cancelled() -> DbError {
    DbError::new(ErrorKind::Cancelled, "The statement was cancelled. CQL cannot stop it on the server.")
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// One result frame as grid rows.
fn decode(result: QueryResult, limits: CellLimits) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>, Vec<String>)> {
    let warnings = result.warnings().map(str::to_owned).collect();
    if !result.is_rows() {
        return Ok((Vec::new(), Vec::new(), warnings));
    }
    let rows = result.into_rows_result().map_err(rows_error)?;
    let columns = rows.column_specs().iter().map(|c| column_meta(c.name(), c.typ())).collect();
    let decoded = rows
        .rows::<Row>()
        .map_err(rows_error)?
        .map(|r| r.map(|r| r.columns.iter().map(|v| values::cell(v.as_ref(), limits)).collect()))
        .collect::<Result<Vec<CellRow>, _>>()
        .map_err(rows_error)?;
    Ok((columns, decoded, warnings))
}

/// cqlsh session commands handled locally.
enum Local {
    Consistency(Option<String>),
    Serial(Option<String>),
    Paging(Option<String>),
    Tracing(Option<String>),
}

fn local_command(sql: &str) -> Option<Local> {
    let words: Vec<String> = sql.split_whitespace().map(|w| w.trim_end_matches(';').to_ascii_uppercase()).collect();
    let arg = |i: usize| words.get(i).filter(|w| !w.is_empty()).cloned();
    match words.first()?.as_str() {
        "CONSISTENCY" => Some(Local::Consistency(arg(1))),
        "SERIAL" if words.get(1).is_some_and(|w| w == "CONSISTENCY") => Some(Local::Serial(arg(2))),
        "PAGING" => Some(Local::Paging(arg(1))),
        "TRACING" => Some(Local::Tracing(arg(1))),
        _ => None,
    }
}

/// `USE ks` / `USE "Ks"` → the keyspace and whether it is case-sensitive.
fn use_target(sql: &str) -> Option<(String, bool)> {
    let rest = sql.trim().trim_end_matches(';').trim();
    let (head, name) = rest.split_once(char::is_whitespace)?;
    if !head.eq_ignore_ascii_case("USE") {
        return None;
    }
    let name = name.trim();
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(quoted) => Some((quoted.replace("\"\"", "\""), true)),
        None => Some((name.to_owned(), false)),
    }
}

fn message_result(command: &str, text: String, started: Instant) -> StatementResult {
    StatementResult {
        columns: vec![ColumnMeta { name: "result".into(), type_name: "text".into(), numeric: false }],
        rows: vec![vec![dq_core::value::Cell::text(text, CellLimits::GRID)]],
        has_more: false,
        command: command.to_owned(),
        affected: None,
        duration_ms: elapsed(started),
        notices: Vec::new(),
    }
}

impl CqlSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let driver = client::connect(params).await?;
        let config = &params.config;
        let local = client::text_rows(&driver, "SELECT release_version FROM system.local WHERE key = 'local'", ())
            .await?
            .into_iter()
            .next()
            .map(|r| r.string(0))
            .unwrap_or_default();
        // ScyllaDB reports a Cassandra-compatible release_version; its own version is in system.versions.
        let scylla_version = client::text_rows(&driver, "SELECT version FROM system.versions WHERE key = 'local'", ())
            .await
            .ok()
            .and_then(|r| r.into_iter().next())
            .map(|r| r.string(0));
        let scylla = scylla_version.is_some();
        let version = match scylla_version {
            Some(v) => format!("ScyllaDB {}", v.split('-').next().unwrap_or(&v)),
            None => format!("Cassandra {local}"),
        };
        let tls = config.ssl_mode != dq_core::config::SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version,
            latency_ms: elapsed(started),
            tls: tls.then(|| TlsInfo { version: None, verified: config.ssl_mode.verifies_chain(), mode: config.ssl_mode }),
            user: if config.username.trim().is_empty() { "anonymous".into() } else { config.username.clone() },
            database: config.database.trim().to_owned(),
            read_only: config.read_only,
        };
        Ok(Self {
            driver,
            read_only: info.read_only,
            info,
            scylla,
            settings: SyncMutex::new(Settings {
                consistency: "LOCAL_QUORUM".into(),
                serial: "SERIAL".into(),
                page_size: None,
                tracing: false,
            }),
            query: Mutex::new(None),
            running: SyncMutex::new(None),
            prepared: Mutex::new(HashMap::new()),
        })
    }

    fn settings(&self) -> Settings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    fn update(&self, f: impl FnOnce(&mut Settings)) {
        if let Ok(mut s) = self.settings.lock() {
            f(&mut s);
        }
    }

    fn keyspace(&self) -> String {
        self.driver.get_keyspace().map_or_else(|| self.info.database.clone(), |k| k.to_string())
    }

    /// Applies the session settings to a statement.
    fn tune(&self, page_size: i32, timeout_secs: u32, tracing: bool) -> impl Fn(&mut dyn Tunable) + use<'_> {
        let s = self.settings();
        move |t: &mut dyn Tunable| {
            t.set_consistency(consistency(&s.consistency).unwrap_or_default());
            t.set_serial(serial(&s.serial));
            t.set_page_size(page_size.max(1));
            t.set_timeout((timeout_secs > 0).then(|| Duration::from_secs(u64::from(timeout_secs))));
            t.set_tracing(tracing);
        }
    }

    pub(crate) async fn prepare(&self, cql: &str) -> DbResult<PreparedStatement> {
        let mut cache = self.prepared.lock().await;
        if let Some(p) = cache.get(cql) {
            return Ok(p.clone());
        }
        let p = self.driver.prepare(cql).await.map_err(|e| exec_error(&e.into()))?;
        if cache.len() >= PREPARED_CACHE {
            cache.clear();
        }
        cache.insert(cql.to_owned(), p.clone());
        Ok(p)
    }

    /// Prepares `cql` and converts each text parameter to its marker's type.
    async fn bind(&self, cql: &str, params: Params<'_>) -> DbResult<(PreparedStatement, Vec<Option<CqlValue>>)> {
        let prepared = self.prepare(cql).await?;
        let specs = prepared.get_variable_col_specs();
        if specs.len() != params.len() {
            return Err(DbError::invalid(format!(
                "The statement has {} parameter(s) but {} value(s) were given.",
                specs.len(),
                params.len()
            )));
        }
        let values = specs
            .iter()
            .zip(params)
            .enumerate()
            .map(|(i, (spec, p))| values::param(i + 1, p.as_deref(), spec.typ()))
            .collect::<DbResult<Vec<_>>>()?;
        Ok((prepared, values))
    }

    async fn request(&self, cql: &str, params: Params<'_>, page: i32, timeout: u32, tracing: bool) -> DbResult<Request> {
        let tune = self.tune(page, timeout, tracing);
        Ok(if params.is_empty() {
            let mut s = Statement::new(cql);
            tune(&mut s);
            Request::Simple(s)
        } else {
            let (mut p, v) = self.bind(cql, params).await?;
            tune(&mut p);
            Request::Prepared(p, v)
        })
    }

    async fn page(&self, request: &Request, state: PagingState, limits: CellLimits) -> DbResult<Page> {
        let (result, next) = match request {
            Request::Simple(s) => self.driver.query_single_page(s.clone(), (), state).await,
            Request::Prepared(p, v) => self.driver.execute_single_page(p, v, state).await,
        }
        .map_err(|e| exec_error(&e))?;
        let tracing_id = result.tracing_id();
        let (columns, rows, warnings) = decode(result, limits)?;
        let next = match next {
            PagingStateResponse::HasMorePages { state } => Some(state),
            PagingStateResponse::NoMorePages => None,
        };
        Ok(Page { columns, rows, next, warnings, tracing_id })
    }

    /// Runs `fut` until it ends, the timeout passes or `cancel` is called.
    async fn guarded<T>(&self, timeout_secs: u32, fut: impl Future<Output = DbResult<T>>) -> DbResult<T> {
        let stop = Arc::new(Notify::new());
        if let Ok(mut r) = self.running.lock() {
            *r = Some(stop.clone());
        }
        let limit = if timeout_secs == 0 { Duration::MAX } else { Duration::from_secs(u64::from(timeout_secs) + 2) };
        let outcome = tokio::select! {
            r = fut => r,
            () = stop.notified() => Err(cancelled()),
            () = tokio::time::sleep(limit) => Err(DbError::new(
                ErrorKind::Timeout,
                format!("The statement did not finish within {timeout_secs} s and was abandoned."),
            )),
        };
        if let Ok(mut r) = self.running.lock() {
            *r = None;
        }
        outcome
    }

    /// Pages until `limit` rows are read; the rest stays in the cursor.
    async fn fill(&self, cursor: &mut Cursor, limit: usize) -> DbResult<(Vec<CellRow>, Option<Page>)> {
        let mut rows: Vec<CellRow> = cursor.buffered.drain(..cursor.buffered.len().min(limit)).collect();
        let mut first = None;
        while rows.len() < limit {
            let Some(state) = cursor.paging.take() else { break };
            let mut page = self.page(&cursor.request, state, CellLimits::GRID).await?;
            cursor.paging = page.next.take();
            let room = limit - rows.len();
            let mut taken = std::mem::take(&mut page.rows);
            if taken.len() > room {
                cursor.buffered.extend(taken.drain(room..));
            }
            rows.extend(taken);
            if first.is_none() {
                first = Some(page);
            }
        }
        Ok((rows, first))
    }

    async fn run(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<(StatementResult, Option<Cursor>)> {
        let started = Instant::now();
        let settings = self.settings();
        let page = settings.page_size.unwrap_or(options.row_limit.min(5_000) as i32);
        let request = self.request(sql, params, page, options.timeout_secs, settings.tracing).await?;
        let mut cursor = Cursor { request, paging: Some(PagingState::start()), buffered: VecDeque::new() };
        let (rows, first) = self.fill(&mut cursor, options.row_limit as usize).await?;
        let first = first.unwrap_or(Page { columns: Vec::new(), rows: Vec::new(), next: None, warnings: Vec::new(), tracing_id: None });
        let has_more = cursor.paging.is_some() || !cursor.buffered.is_empty();
        let mut notices = first.warnings;
        if let Some(id) = first.tracing_id {
            notices.push(format!("Tracing session {id}: SELECT * FROM system_traces.events WHERE session_id = {id};"));
        }
        let result = StatementResult {
            columns: first.columns,
            rows,
            has_more,
            command: command_tag(sql, SqlFlavor::Cql),
            affected: None,
            duration_ms: elapsed(started),
            notices,
        };
        Ok((result, has_more.then_some(cursor)))
    }

    fn local(&self, command: Local, started: Instant) -> DbResult<StatementResult> {
        let invalid = |v: &str, allowed: &[&str]| DbError::invalid(format!("`{v}` is not one of {}.", allowed.join(", ")));
        Ok(match command {
            Local::Consistency(None) => message_result("CONSISTENCY", format!("Current consistency level is {}.", self.settings().consistency), started),
            Local::Consistency(Some(v)) => {
                consistency(&v).ok_or_else(|| invalid(&v, &CONSISTENCY_LEVELS))?;
                self.update(|s| s.consistency.clone_from(&v));
                message_result("CONSISTENCY", format!("Consistency level set to {v}."), started)
            }
            Local::Serial(None) => message_result("SERIAL CONSISTENCY", format!("Current serial consistency level is {}.", self.settings().serial), started),
            Local::Serial(Some(v)) => {
                serial(&v).ok_or_else(|| invalid(&v, &SERIAL_LEVELS))?;
                self.update(|s| s.serial.clone_from(&v));
                message_result("SERIAL CONSISTENCY", format!("Serial consistency level set to {v}."), started)
            }
            Local::Paging(v) => {
                let size = match v.as_deref() {
                    None => return Ok(message_result("PAGING", format!("Page size: {}.", self.settings().page_size.map_or("the row limit".into(), |p| p.to_string())), started)),
                    Some("ON") => None,
                    Some("OFF") => Some(5_000),
                    Some(n) => Some(n.parse::<i32>().ok().filter(|n| (1..=5_000).contains(n)).ok_or_else(|| DbError::invalid("The page size is between 1 and 5000."))?),
                };
                self.update(|s| s.page_size = size);
                message_result("PAGING", format!("Page size: {}.", size.map_or("the row limit".into(), |p| p.to_string())), started)
            }
            Local::Tracing(v) => {
                let on = match v.as_deref() {
                    Some("ON") => true,
                    Some("OFF") => false,
                    _ => return Ok(message_result("TRACING", format!("Tracing is {}.", if self.settings().tracing { "on" } else { "off" }), started)),
                };
                self.update(|s| s.tracing = on);
                message_result("TRACING", format!("Tracing {}.", if on { "enabled" } else { "disabled" }), started)
            }
        })
    }

    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let request = self.request(sql, params, STREAM_PAGE, 0, false).await?;
        let mut state = Some(PagingState::start());
        let mut total = 0u64;
        let mut meta: Option<Vec<ColumnMeta>> = None;
        while let Some(current) = state.take() {
            let page = self.page(&request, current, limits).await?;
            state = page.next;
            if meta.is_none() || !page.rows.is_empty() {
                let columns = meta.get_or_insert(page.columns);
                total += page.rows.len() as u64;
                on_batch(columns, page.rows)?;
            }
        }
        Ok(total)
    }

    /// One statement as a prepared statement with converted values, or plain text when it has none.
    async fn bound(&self, statement: &BoundStatement) -> DbResult<Request> {
        let s = self.settings();
        if statement.params.is_empty() {
            let mut st = Statement::new(statement.sql.clone());
            st.set_consistency(consistency(&s.consistency).unwrap_or_default());
            return Ok(Request::Simple(st));
        }
        let (mut p, v) = self.bind(&statement.sql, &statement.params).await?;
        p.set_consistency(consistency(&s.consistency).unwrap_or_default());
        p.set_serial_consistency(serial(&s.serial));
        Ok(Request::Prepared(p, v))
    }

    async fn run_bound(&self, statement: &BoundStatement) -> DbResult<QueryResult> {
        match self.bound(statement).await? {
            Request::Simple(s) => self.driver.query_unpaged(s, ()).await,
            Request::Prepared(p, v) => self.driver.execute_unpaged(&p, &v).await,
        }
        .map_err(|e| exec_error(&e))
    }

    /// A single grid change as a lightweight transaction, so it never
    /// recreates a row deleted meanwhile or overwrites an existing one.
    async fn single_change(&self, statement: BoundStatement) -> DbResult<Vec<u64>> {
        let head = statement.sql.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
        let has_condition = statement.sql.to_ascii_uppercase().contains(" IF ");
        let (suffix, conflict) = match head.as_str() {
            "UPDATE" | "DELETE" if !has_condition => (" IF EXISTS", "The row changed or no longer exists. Reload."),
            "INSERT" if !has_condition => (" IF NOT EXISTS", "A row with this primary key already exists."),
            _ => ("", ""),
        };
        let conditional = BoundStatement { sql: format!("{}{suffix}", statement.sql.trim_end_matches(';')), ..statement.clone() };
        let rows = client::rows(self.run_bound(&conditional).await?)?;
        if !suffix.is_empty() {
            let applied = rows.first().and_then(|r| r.columns.first()).is_some_and(|v| matches!(v, Some(CqlValue::Boolean(true))));
            if !applied {
                return Err(DbError::new(ErrorKind::Constraint, conflict));
            }
        }
        check_affected(&statement, 1)?;
        Ok(vec![1])
    }

    /// Up to [`MAX_BATCH`] changes in one logged batch: applied together, though not isolated.
    async fn logged_batch(&self, statements: &[BoundStatement]) -> DbResult<Vec<u64>> {
        let mut batch = Batch::new(BatchType::Logged);
        let mut values: Vec<Vec<Option<CqlValue>>> = Vec::with_capacity(statements.len());
        for s in statements {
            match self.bound(s).await? {
                Request::Simple(st) => {
                    batch.append_statement(st);
                    values.push(Vec::new());
                }
                Request::Prepared(p, v) => {
                    batch.append_statement(p);
                    values.push(v);
                }
            }
        }
        batch.set_consistency(consistency(&self.settings().consistency).unwrap_or_default());
        self.driver.batch(&batch, values).await.map_err(|e| exec_error(&e))?;
        statements.iter().map(|s| check_affected(s, 1).map(|()| 1)).collect()
    }

    /// Plans without running: the read analysis for `SELECT`s.
    async fn plan(&self, sql: &str, params: Params<'_>) -> DbResult<ExplainResult> {
        // Preparing checks the statement and its parameters without running it.
        if params.is_empty() {
            self.prepare(sql).await?;
        } else {
            self.bind(sql, params).await?;
        }
        let consistency = self.settings().consistency;
        let keyspace = self.keyspace();
        let analysis = match select_target(sql, &keyspace) {
            Some(object) => {
                let keys = catalog::key_roles(&self.driver, &object).await.unwrap_or_default();
                analyze_select(sql, object, keys)
            }
            None => None,
        };
        let Some(a) = analysis else {
            let tag = command_tag(sql, SqlFlavor::Cql);
            let node = PlanNode {
                label: if tag.is_empty() { "Statement".into() } else { tag },
                props: vec![
                    ("Consistency".into(), consistency),
                    ("Note".into(), "CQL has no query plans. Writes go to the replicas that own the partition.".into()),
                ],
                ..PlanNode::default()
            };
            return Ok(ExplainResult { plan: Some(node), raw: "No plan: CQL routes each statement to the partition's replicas.".into(), analyzed: false, planning_ms: None, execution_ms: None });
        };
        let label = match a.level {
            AnalysisLevel::Good => "Single-partition read",
            AnalysisLevel::Info => "Multi-partition read",
            AnalysisLevel::Caution => "Scan",
        };
        let keys = |role: KeyRoleKind| {
            a.keys.iter().filter(|k| k.role == role).map(|k| if k.descending { format!("{} DESC", k.column) } else { k.column.clone() }).collect::<Vec<_>>().join(", ")
        };
        let node = PlanNode {
            label: label.into(),
            detail: Some(format!("{}.{}", a.object.database, a.object.name)),
            props: vec![
                ("Partition key".into(), keys(KeyRoleKind::Partition)),
                ("Clustering".into(), keys(KeyRoleKind::Clustering)),
                ("Consistency".into(), consistency),
                ("Analysis".into(), a.message.clone()),
            ],
            ..PlanNode::default()
        };
        Ok(ExplainResult { plan: Some(node), raw: a.message, analyzed: false, planning_ms: None, execution_ms: None })
    }

    /// Runs the statement with tracing and shows the trace events.
    async fn trace(&self, sql: &str, params: Params<'_>, timeout_secs: u32) -> DbResult<ExplainResult> {
        let request = self.request(sql, params, 100, timeout_secs, true).await?;
        let page = self.guarded(timeout_secs, self.page(&request, PagingState::start(), CellLimits::GRID)).await?;
        let id = page.tracing_id.ok_or_else(|| DbError::internal("The server returned no tracing session."))?;
        let trace = self.driver.get_tracing_info(&id).await.map_err(|e| DbError::new(ErrorKind::Server, format!("Could not read the trace: {e}")))?;
        let ms = |us: Option<i32>| us.map(|u| f64::from(u) / 1000.0);
        let mut raw = String::new();
        let children = trace
            .events
            .iter()
            .map(|e| {
                let source = e.source.map(|s| s.to_string());
                raw.push_str(&format!(
                    "{:>8} µs  {:<15}  {}\n",
                    e.source_elapsed.unwrap_or_default(),
                    source.clone().unwrap_or_default(),
                    e.activity.clone().unwrap_or_default()
                ));
                PlanNode {
                    label: e.activity.clone().unwrap_or_default(),
                    detail: source,
                    actual_ms: ms(e.source_elapsed),
                    props: e.thread.clone().map(|t| vec![("Thread".into(), t)]).unwrap_or_default(),
                    ..PlanNode::default()
                }
            })
            .collect();
        let root = PlanNode {
            label: trace.request.clone().or(trace.command.clone()).unwrap_or_else(|| "Request".into()),
            detail: trace.coordinator.map(|c| format!("coordinator {c}")),
            actual_ms: ms(trace.duration),
            actual_rows: Some(page.rows.len() as f64),
            props: vec![("Trace id".into(), id.to_string())],
            children,
            ..PlanNode::default()
        };
        Ok(ExplainResult { plan: Some(root), raw, analyzed: true, planning_ms: None, execution_ms: ms(trace.duration) })
    }
}

/// The statement kinds share their tuning setters only by name.
trait Tunable {
    fn set_consistency(&mut self, c: Consistency);
    fn set_serial(&mut self, c: Option<SerialConsistency>);
    fn set_page_size(&mut self, size: i32);
    fn set_timeout(&mut self, timeout: Option<Duration>);
    fn set_tracing(&mut self, on: bool);
}

macro_rules! tunable {
    ($t:ty) => {
        impl Tunable for $t {
            fn set_consistency(&mut self, c: Consistency) {
                <$t>::set_consistency(self, c);
            }
            fn set_serial(&mut self, c: Option<SerialConsistency>) {
                self.set_serial_consistency(c);
            }
            fn set_page_size(&mut self, size: i32) {
                <$t>::set_page_size(self, size);
            }
            fn set_timeout(&mut self, timeout: Option<Duration>) {
                self.set_request_timeout(timeout);
            }
            fn set_tracing(&mut self, on: bool) {
                <$t>::set_tracing(self, on);
            }
        }
    };
}

tunable!(Statement);
tunable!(PreparedStatement);

#[async_trait]
impl Session for CqlSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &CqlDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::keyspaces(&self.driver, &self.keyspace()).await,
            Some(p) if p.kind == NodeKind::Database => catalog::keyspace_children(&self.driver, &p.name).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(&self.driver, p).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&self.driver, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let options = options.clamped();
        let started = Instant::now();
        let sql = sql.trim().trim_end_matches(';').trim();
        let mut cursor = self.query.lock().await;
        *cursor = None;
        if let Some(command) = local_command(sql) {
            return self.local(command, started);
        }
        if self.read_only && split_statements(sql, SqlFlavor::Cql).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        if let Some((keyspace, case_sensitive)) = use_target(sql) {
            self.driver.use_keyspace(keyspace, case_sensitive).await.map_err(|e| DbError::invalid(e.to_string()))?;
            return Ok(StatementResult {
                columns: Vec::new(),
                rows: Vec::new(),
                has_more: false,
                command: "USE".into(),
                affected: None,
                duration_ms: elapsed(started),
                notices: Vec::new(),
            });
        }
        let (result, open) = self.guarded(options.timeout_secs, self.run(sql, params, options)).await?;
        *cursor = open;
        Ok(result)
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut cursor = self.query.lock().await;
        let mut open = cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let limit = limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize;
        let (rows, _) = self.guarded(0, self.fill(&mut open, limit)).await?;
        let has_more = open.paging.is_some() || !open.buffered.is_empty();
        if has_more {
            *cursor = Some(open);
        }
        Ok(RowsPage { rows, has_more, duration_ms: elapsed(started) })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let running = self.running.lock().ok().and_then(|r| r.clone());
        Ok(running.map(|stop| stop.notify_one()).is_some())
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&self.driver, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&self.driver, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(&self.driver, object).await
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let keyspace = if database.is_empty() { self.keyspace() } else { database.to_owned() };
        catalog::schema_model(&self.driver, &keyspace).await
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

    async fn stream_query(&self, _database: &str, sql: &str, params: Params<'_>, on_batch: &mut BatchFn<'_>) -> DbResult<u64> {
        self.read_batches(sql, params, CellLimits::FULL, on_batch).await
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::row_estimate(&self.driver, object).await
    }

    /// CQL has no transactions. One change runs as a lightweight transaction
    /// (`IF EXISTS` / `IF NOT EXISTS`); up to 20 run as one logged batch
    /// (applied together, not isolated); more (imports) run one by one, 16 at a
    /// time, and those before a failure stay applied.
    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let mut head = Vec::new();
        for s in statements.by_ref() {
            head.push(s?);
            if head.len() > MAX_BATCH {
                break;
            }
        }
        match head.len() {
            0 => return Ok(Vec::new()),
            1 => return self.single_change(head.remove(0)).await,
            n if n <= MAX_BATCH => return self.logged_batch(&head).await,
            _ => {}
        }
        let mut done = 0u64;
        let results: Vec<DbResult<u64>> = stream::iter(head.into_iter().map(Ok).chain(statements))
            .map(|s| async move {
                let s = s?;
                self.run_bound(&s).await?;
                check_affected(&s, 1).map(|()| 1)
            })
            .buffered(IMPORT_CONCURRENCY)
            .collect()
            .await;
        let mut counts = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Ok(n) => {
                    done += n;
                    counts.push(n);
                }
                Err(e) => {
                    return Err(DbError { message: format!("{} {done} statements before it were applied.", e.message), ..e });
                }
            }
        }
        Ok(counts)
    }

    async fn explain(&self, sql: &str, params: Params<'_>, analyze: bool, timeout_secs: u32) -> DbResult<ExplainResult> {
        let sql = sql.trim().trim_end_matches(';');
        if analyze { self.trace(sql, params, timeout_secs).await } else { self.plan(sql, params).await }
    }

    async fn key_roles(&self, object: &ObjectRef) -> DbResult<Vec<KeyRole>> {
        catalog::key_roles(&self.driver, object).await
    }

    fn options(&self) -> Vec<SessionOption> {
        let s = self.settings();
        let choices = |c: &[&str]| c.iter().map(|&x| x.to_owned()).collect();
        vec![
            SessionOption { name: "consistency".into(), label: "Consistency".into(), value: s.consistency, choices: choices(&CONSISTENCY_LEVELS) },
            SessionOption { name: "serial_consistency".into(), label: "Serial consistency".into(), value: s.serial, choices: choices(&SERIAL_LEVELS) },
            SessionOption {
                name: "page_size".into(),
                label: "Page size".into(),
                value: s.page_size.map_or_else(|| "100".into(), |p| p.to_string()),
                choices: choices(&PAGE_SIZES),
            },
        ]
    }

    async fn set_option(&self, name: &str, value: &str) -> DbResult<()> {
        let value = value.trim().to_ascii_uppercase();
        match name {
            "consistency" if consistency(&value).is_some() => self.update(|s| s.consistency = value),
            "serial_consistency" if serial(&value).is_some() => self.update(|s| s.serial = value),
            "page_size" if PAGE_SIZES.contains(&value.as_str()) => {
                let size = value.parse().ok();
                self.update(|s| s.page_size = size);
            }
            "consistency" | "serial_consistency" | "page_size" => {
                return Err(DbError::invalid(format!("`{value}` is not a valid {name}.")));
            }
            _ => return Err(DbError::unsupported(format!("This connection has no setting `{name}`."))),
        }
        Ok(())
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cqlsh_commands() {
        assert!(matches!(local_command("CONSISTENCY one;"), Some(Local::Consistency(Some(v))) if v == "ONE"));
        assert!(matches!(local_command("consistency"), Some(Local::Consistency(None))));
        assert!(matches!(local_command("SERIAL CONSISTENCY LOCAL_SERIAL"), Some(Local::Serial(Some(_)))));
        assert!(matches!(local_command("TRACING ON"), Some(Local::Tracing(Some(v))) if v == "ON"));
        assert!(local_command("SELECT 1").is_none());
        assert_eq!(use_target("USE telemetry;"), Some(("telemetry".into(), false)));
        assert_eq!(use_target("use \"My\"\"Ks\""), Some(("My\"Ks".into(), true)));
        assert_eq!(use_target("USER x"), None);
        assert!(consistency("local_one").is_some() && consistency("most").is_none());
    }
}
