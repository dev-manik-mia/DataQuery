//! An open Neo4j connection over the Query API.
//!
//! Editor statements run one at a time on the query path, which keeps the
//! current database (`USE`), an explicit transaction (`BEGIN` … `COMMIT`,
//! kept alive while idle) and the rows of the last result beyond the row
//! limit. Every editor statement starts with a `/* dq:<tag> */` comment, so a
//! running one can be found in `SHOW TRANSACTIONS` and terminated.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as SyncMutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter};
use dq_core::document::DocumentStore;
use dq_core::explain::ExplainResult;
use dq_core::monitor::Monitor;
use dq_core::node::{Node, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{CypherDialect, Dialect, SqlFlavor, command_tag, named_params, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, oneshot};

use crate::catalog;
use crate::client::{Access, Client, Query, Reply, Transaction};
use crate::plan;
use crate::values::{self, cell, type_label};

const STREAM_BATCH: usize = 1_000;
/// Explicit transactions expire after 60 s without a statement; touch them before that.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Query-path state.
#[derive(Default)]
struct QueryState {
    database: String,
    tx: Option<Transaction>,
    /// Rows of the last result not yet shown.
    pending: VecDeque<Row>,
}

pub struct Neo4jSession {
    pub(crate) client: Arc<Client>,
    info: ServerInfo,
    pub(crate) read_only: bool,
    state: Arc<Mutex<QueryState>>,
    /// Stops the statement running on the query path.
    running: SyncMutex<Option<oneshot::Sender<()>>>,
    /// Transaction ids from the last monitor snapshot, by numeric pid.
    pub(crate) monitored: SyncMutex<std::collections::HashMap<i64, String>>,
}

/// Columns and rows of a reply. Each column's type is that of its first non-null value.
pub fn grid(reply: &Reply, limits: CellLimits) -> (Vec<ColumnMeta>, Vec<Row>) {
    let columns = reply
        .fields
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let type_name = reply
                .values
                .iter()
                .filter_map(|r| r.get(i))
                .find(|v| values::type_of(v) != "Null")
                .map_or("Null", type_label);
            ColumnMeta { name: name.clone(), type_name: type_name.to_owned(), numeric: values::is_numeric(type_name) }
        })
        .collect();
    let rows = reply.values.iter().map(|r| r.iter().map(|v| cell(v, limits)).collect()).collect();
    (columns, rows)
}

/// Typed parameters for a statement's `$name`s, in order of first appearance.
pub fn bind(sql: &str, params: Params<'_>) -> DbResult<Map<String, Value>> {
    if params.is_empty() {
        return Ok(Map::new());
    }
    let names = named_params(sql, SqlFlavor::Cypher);
    if names.len() != params.len() {
        return Err(DbError::invalid(format!(
            "The statement has {} parameter(s) but {} value(s) were given.",
            names.len(),
            params.len()
        )));
    }
    Ok(names.into_iter().zip(params).map(|(n, v)| (n, values::param(v.as_deref()))).collect())
}

/// `BEGIN`, `COMMIT`, `ROLLBACK` or `USE db` / `:use db` typed alone.
enum Control {
    Begin,
    Commit,
    Rollback,
    Use(String),
}

fn control(sql: &str) -> Option<Control> {
    let s = sql.trim().trim_end_matches(';').trim();
    let mut words = s.split_whitespace();
    let first = words.next()?.to_ascii_uppercase();
    let rest: Vec<&str> = words.collect();
    match (first.as_str(), rest.as_slice()) {
        ("BEGIN", []) => Some(Control::Begin),
        ("COMMIT", []) => Some(Control::Commit),
        ("ROLLBACK", []) => Some(Control::Rollback),
        ("USE" | ":USE", [db]) => Some(Control::Use(db.trim_matches('`').to_owned())),
        _ => None,
    }
}

fn notice_result(command: &str, notice: impl Into<String>, started: Instant) -> StatementResult {
    StatementResult {
        columns: Vec::new(),
        rows: Vec::new(),
        has_more: false,
        command: command.to_owned(),
        affected: None,
        duration_ms: started.elapsed().as_millis() as u64,
        notices: vec![notice.into()],
    }
}

impl Neo4jSession {
    pub async fn open(params: &ConnectParams) -> DbResult<Arc<Self>> {
        let config = &params.config;
        let client = Arc::new(Client::new(params)?);
        let started = Instant::now();
        let discovery = client.discovery().await?;
        let database = config.database.trim().to_owned();
        let database = if database.is_empty() { "neo4j".to_owned() } else { database };
        let reply = client.run(&Query::read(&database, "CALL dbms.components() YIELD versions, edition"), false).await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let row = reply.values.first();
        let version = row
            .and_then(|r| r.first())
            .and_then(values::text)
            .or_else(|| discovery.get("neo4j_version").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let edition = row.and_then(|r| r.get(1)).and_then(values::text).unwrap_or_default();
        let tls = (config.ssl_mode != dq_core::config::SslMode::Disable).then(|| TlsInfo {
            version: None,
            verified: config.ssl_mode.verifies_chain(),
            mode: config.ssl_mode,
        });
        let info = ServerInfo {
            engine: dq_core::engine::Engine::Neo4j,
            version: format!("Neo4j {version} ({edition})").replace(" ()", ""),
            latency_ms,
            tls,
            user: config.username.clone(),
            database: database.clone(),
            read_only: config.read_only,
        };
        let session = Arc::new(Self {
            client,
            info,
            read_only: config.read_only,
            state: Arc::new(Mutex::new(QueryState { database, ..QueryState::default() })),
            running: SyncMutex::new(None),
            monitored: SyncMutex::default(),
        });
        spawn_keepalive(Arc::downgrade(&session.state), session.client.clone());
        Ok(session)
    }

    pub(crate) fn access(&self) -> Access {
        if self.read_only { Access::Read } else { Access::Write }
    }

    fn refuse_writes(&self, sql: &str) -> DbResult<()> {
        if self.read_only && split_statements(sql, SqlFlavor::Cypher).iter().any(|s| s.writes) {
            return Err(DbError::new(ErrorKind::Permission, "This connection is read-only.")
                .with_action(Some("Edit the connection and turn off Read-only to make changes.")));
        }
        Ok(())
    }

    /// Runs a statement on the query path, stoppable by [`Session::cancel`] and the timeout.
    async fn run_tracked(
        &self,
        state: &mut QueryState,
        statement: &str,
        parameters: Map<String, Value>,
        access: Access,
        timeout_secs: u32,
    ) -> DbResult<Reply> {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let text = format!("/* dq:{tag} */ {statement}");
        let (stop, stopped) = oneshot::channel();
        let database = state.tx.as_ref().map_or_else(|| state.database.clone(), |t| t.database.clone());
        *self.running.lock().expect("running lock") = Some(stop);
        let query = Query { database: &database, statement: &text, parameters, access, transaction: state.tx.as_ref() };
        let deadline = async {
            if timeout_secs == 0 {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs))).await;
        };
        let outcome = tokio::select! {
            r = self.client.run(&query, false) => Ok(r),
            _ = stopped => Err(ErrorKind::Cancelled),
            () = deadline => Err(ErrorKind::Timeout),
        };
        self.running.lock().expect("running lock").take();
        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(e)) => {
                // A failed statement rolls the explicit transaction back.
                if state.tx.is_some() && e.kind != ErrorKind::Network {
                    state.tx = None;
                }
                Err(e)
            }
            Err(kind) => {
                let ended = terminate(&self.client, &database, &tag).await;
                if state.tx.is_some() && ended {
                    state.tx = None;
                }
                Err(if kind == ErrorKind::Timeout {
                    DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {timeout_secs} seconds and was stopped."))
                } else {
                    DbError::new(ErrorKind::Cancelled, "The statement was cancelled.")
                })
            }
        }
    }

    fn result(reply: &Reply, sql: &str, row_limit: u32, started: Instant, state: &mut QueryState) -> StatementResult {
        let (columns, mut rows) = grid(reply, CellLimits::GRID);
        let limit = row_limit as usize;
        rows.truncate(ExecOptions::MAX_ROW_LIMIT as usize);
        state.pending = if rows.len() > limit { rows.split_off(limit).into() } else { VecDeque::new() };
        let mut notices: Vec<String> = reply.counters_summary().into_iter().collect();
        notices.extend(reply.notifications.iter().cloned());
        StatementResult {
            columns,
            rows,
            has_more: !state.pending.is_empty(),
            command: command_tag(sql, SqlFlavor::Cypher),
            affected: reply.affected(),
            duration_ms: started.elapsed().as_millis() as u64,
            notices,
        }
    }

    async fn control(&self, state: &mut QueryState, control: Control, started: Instant) -> DbResult<StatementResult> {
        match control {
            Control::Begin => {
                if state.tx.is_some() {
                    return Err(DbError::invalid("A transaction is already open."));
                }
                let query = Query { access: self.access(), ..Query::read(&state.database, "RETURN 1") };
                let reply = self.client.run(&query, true).await?;
                state.tx = Some(reply.transaction.ok_or_else(|| DbError::internal("Neo4j opened no transaction."))?);
                Ok(notice_result("BEGIN", "Transaction started.", started))
            }
            Control::Commit => {
                let tx = state.tx.take().ok_or_else(|| DbError::invalid("No transaction is open."))?;
                self.client.commit(&tx).await.map_err(expired)?;
                Ok(notice_result("COMMIT", "Transaction committed.", started))
            }
            Control::Rollback => {
                let tx = state.tx.take().ok_or_else(|| DbError::invalid("No transaction is open."))?;
                self.client.rollback(&tx).await.map_err(expired)?;
                Ok(notice_result("ROLLBACK", "Transaction rolled back.", started))
            }
            Control::Use(db) => {
                if state.tx.is_some() {
                    return Err(DbError::invalid("Commit or roll back the transaction before switching databases."));
                }
                self.client.run(&Query::read(&db, "RETURN 1"), false).await?;
                state.database.clone_from(&db);
                Ok(notice_result("USE", format!("Using database {db}."), started))
            }
        }
    }

    /// A read on the catalog path (not the editor's transaction).
    pub(crate) async fn read(&self, database: &str, statement: &str, parameters: Map<String, Value>) -> DbResult<Reply> {
        let query = Query { parameters, ..Query::read(database, statement) };
        self.client.run(&query, false).await
    }

    pub(crate) fn current_database(&self) -> String {
        self.state.try_lock().map_or_else(|_| self.info.database.clone(), |s| s.database.clone())
    }
}

/// A transaction that is gone on the server: say why.
fn expired(e: DbError) -> DbError {
    if e.kind == ErrorKind::NotFound {
        DbError::new(
            ErrorKind::NotFound,
            "The transaction is no longer open: it timed out on the server or failed, and was rolled back.",
        )
    } else {
        e
    }
}

/// Ends the transaction whose current statement carries `tag`. Retries briefly
/// while the server registers it; returns whether one was terminated.
pub async fn terminate(client: &Client, database: &str, tag: &str) -> bool {
    let find = "SHOW TRANSACTIONS YIELD transactionId, currentQuery WHERE currentQuery CONTAINS $tag RETURN transactionId";
    for _ in 0..10 {
        let query = Query::read(database, find).with("tag", values::param(Some(&format!("\"dq:{tag}\""))));
        if let Ok(reply) = client.run(&Query { access: Access::Write, ..query }, false).await {
            let ids: Vec<Value> = reply.values.iter().filter_map(|r| r.first().cloned()).collect();
            if !ids.is_empty() {
                let ids = serde_json::json!({ "$type": "List", "_value": ids });
                let terminate = Query::write(database, "TERMINATE TRANSACTIONS $ids").with("ids", ids);
                return client.run(&terminate, false).await.is_ok();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Touches an idle explicit transaction so it does not expire.
fn spawn_keepalive(state: Weak<Mutex<QueryState>>, client: Arc<Client>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(KEEPALIVE);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(state) = state.upgrade() else { return };
            // Busy means a statement is running, which keeps it alive anyway.
            let Ok(guard) = state.try_lock() else { continue };
            if let Some(tx) = guard.tx.as_ref() {
                let _ = client.run(&Query { transaction: Some(tx), ..Query::read(&tx.database, "RETURN 1") }, false).await;
            }
        }
    });
}

#[async_trait]
impl Session for Neo4jSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &CypherDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(&self.client, &self.current_database()).await,
            Some(p) => catalog::children(&self.client, p).await,
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&self.client, &self.current_database(), query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let started = Instant::now();
        let mut state = self.state.lock().await;
        state.pending.clear();
        if let Some(c) = control(sql) {
            return self.control(&mut state, c, started).await;
        }
        self.refuse_writes(sql)?;
        let parameters = bind(sql, params)?;
        let statement = sql.trim().trim_end_matches(';');
        let access = self.access();
        let reply = self.run_tracked(&mut state, statement, parameters, access, options.timeout_secs).await?;
        Ok(Self::result(&reply, sql, options.row_limit, started, &mut state))
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut state = self.state.lock().await;
        let take = (limit as usize).min(state.pending.len());
        let rows: Vec<Row> = state.pending.drain(..take).collect();
        Ok(RowsPage { rows, has_more: !state.pending.is_empty(), duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let running = self.running.lock().expect("running lock").take();
        Ok(running.is_some_and(|stop| stop.send(()).is_ok()))
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&self.client, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&self.client, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(&self.client, object).await
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        catalog::schema_model(&self.client, database).await
    }

    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
        self.refuse_writes(sql)?;
        let reply = self.read(database, sql.trim().trim_end_matches(';'), bind(sql, params)?).await?;
        Ok(grid(&reply, limits))
    }

    async fn stream_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let (columns, rows) = self.read_query(database, sql, params, CellLimits::FULL).await?;
        let total = rows.len() as u64;
        let mut rows = rows.into_iter().peekable();
        if rows.peek().is_none() {
            on_batch(&columns, Vec::new())?;
        }
        while rows.peek().is_some() {
            on_batch(&columns, rows.by_ref().take(STREAM_BATCH).collect())?;
        }
        Ok(total)
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::count(&self.client, object).await
    }

    /// Runs the statements in one explicit transaction (`$p1`, `$p2`, … bind their values).
    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(DbError::new(ErrorKind::Permission, "This connection is read-only."));
        }
        let mut tx: Option<Transaction> = None;
        let mut counts = Vec::new();
        let outcome: DbResult<()> = async {
            for statement in statements {
                let statement = statement?;
                let parameters: Map<String, Value> = statement
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (format!("p{}", i + 1), values::param(v.as_deref())))
                    .collect();
                let query =
                    Query { parameters, transaction: tx.as_ref(), ..Query::write(database, &statement.sql) };
                let reply = self.client.run(&query, tx.is_none()).await?;
                if tx.is_none() {
                    tx = reply.transaction.clone();
                }
                counts.push(reply.affected().unwrap_or(1));
            }
            Ok(())
        }
        .await;
        match (outcome, tx) {
            (Ok(()), Some(tx)) => {
                self.client.commit(&tx).await.map_err(expired)?;
                Ok(counts)
            }
            (Ok(()), None) => Ok(counts),
            (Err(e), Some(tx)) => {
                let _ = self.client.rollback(&tx).await;
                Err(e)
            }
            (Err(e), None) => Err(e),
        }
    }

    async fn explain(&self, sql: &str, params: Params<'_>, analyze: bool, timeout_secs: u32) -> DbResult<ExplainResult> {
        let body = sql.trim().trim_end_matches(';');
        let upper = body.to_ascii_uppercase();
        let body = ["EXPLAIN ", "PROFILE "].iter().find(|p| upper.starts_with(*p)).map_or(body, |p| body[p.len()..].trim());
        if analyze {
            self.refuse_writes(body)?;
        }
        let statement = format!("{} {body}", if analyze { "PROFILE" } else { "EXPLAIN" });
        let parameters = bind(body, params)?;
        let started = Instant::now();
        let mut state = self.state.lock().await;
        let access = if analyze { Access::Read } else { self.access() };
        let reply = self.run_tracked(&mut state, &statement, parameters, access, timeout_secs).await?;
        let plan = reply.plan.as_ref().ok_or_else(|| DbError::new(ErrorKind::Server, "Neo4j returned no plan."))?;
        Ok(plan::result(plan, reply.profiled, analyze.then(|| started.elapsed().as_secs_f64() * 1000.0)))
    }

    fn monitor(&self) -> Option<&dyn Monitor> {
        Some(self)
    }

    fn documents(&self) -> Option<&dyn DocumentStore> {
        Some(self)
    }

    async fn close(&self) {
        let tx = self.state.lock().await.tx.take();
        if let Some(tx) = tx {
            let _ = self.client.rollback(&tx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_words() {
        assert!(matches!(control(" begin ; "), Some(Control::Begin)));
        assert!(matches!(control("COMMIT"), Some(Control::Commit)));
        assert!(matches!(control(":use movies"), Some(Control::Use(d)) if d == "movies"));
        assert!(matches!(control("USE `my-db`"), Some(Control::Use(d)) if d == "my-db"));
        assert!(control("USE movies MATCH (n) RETURN n").is_none());
        assert!(control("RETURN 1").is_none());
    }

    #[test]
    fn binds_named_params() {
        let p = bind("MATCH (n {id: $id, name: $name}) WHERE n.id = $id RETURN n", &[Some("7".into()), Some("Ann".into())]).unwrap();
        assert_eq!(p["id"]["$type"], "Integer");
        assert_eq!(p["name"]["_value"], "Ann");
        assert!(bind("RETURN $a", &[Some("1".into()), Some("2".into())]).is_err());
    }
}
