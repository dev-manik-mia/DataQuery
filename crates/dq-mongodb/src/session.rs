//! An open `MongoDB` connection.
//!
//! Editor statements run one at a time on the query path, which keeps the
//! current database (`use`), an open transaction (`BEGIN` … `COMMIT`) and the
//! cursor of the last result for "fetch more". The explorer, the Documents
//! view and exports use the client's pool directly, so they stay responsive
//! while a statement runs. Every command carries a `comment`, so a running
//! statement can be found and stopped with `killOp`.

use std::sync::Mutex as SyncMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::document::{DocumentRows, DocumentStore, GridColumns};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, MongoDialect, SqlFlavor, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::{Client, ClientSession, Cursor, SessionCursor};
use tokio::sync::{Mutex, Notify};

use crate::client::{Connected, check_reply, int_field, map_error};
use crate::commands::{self, Plan, Shape};
use crate::shell::{self, Call, Statement};
use crate::values::{fields, grid};
use crate::{catalog, explain, literal};

const STREAM_BATCH: usize = 1_000;

/// An open server cursor, inside a transaction or not.
pub enum Rows {
    Plain(Cursor<Document>),
    Txn(SessionCursor<Document>),
}

impl Rows {
    pub async fn next(&mut self, txn: Option<&mut ClientSession>) -> DbResult<Option<Document>> {
        match self {
            Self::Plain(c) => c.try_next().await.map_err(map_error),
            Self::Txn(c) => {
                let session = txn.ok_or_else(|| DbError::invalid("The transaction of this result has ended."))?;
                c.next(session).await.transpose().map_err(map_error)
            }
        }
    }
}

/// What a command produced.
pub enum Output {
    Rows(Box<Rows>),
    Docs(Vec<Document>),
    Done { affected: Option<u64>, notices: Vec<String> },
}

/// Runs a command, with the transaction's session when one is open.
pub async fn command(
    client: &Client,
    database: &str,
    command: Document,
    txn: Option<&mut ClientSession>,
) -> DbResult<Document> {
    let db = client.database(database);
    let reply = match txn {
        Some(session) => db.run_command(command).session(session).await,
        None => db.run_command(command).await,
    }
    .map_err(map_error)?;
    check_reply(&reply)?;
    Ok(reply)
}

pub async fn cursor(
    client: &Client,
    database: &str,
    command: Document,
    txn: Option<&mut ClientSession>,
) -> DbResult<Rows> {
    let db = client.database(database);
    match txn {
        Some(session) => db.run_cursor_command(command).session(session).await.map(Rows::Txn),
        None => db.run_cursor_command(command).await.map(Rows::Plain),
    }
    .map_err(map_error)
}

/// Every document of a cursor command, up to `limit`.
pub async fn documents(client: &Client, database: &str, command: Document, limit: usize) -> DbResult<Vec<Document>> {
    let mut rows = cursor(client, database, command, None).await?;
    let mut out = Vec::new();
    while out.len() < limit {
        match rows.next(None).await? {
            Some(d) => out.push(d),
            None => break,
        }
    }
    Ok(out)
}

/// The reply without driver bookkeeping fields.
fn reply_row(mut reply: Document) -> Document {
    for key in [
        "ok",
        "$clusterTime",
        "operationTime",
        "$gleStats",
        "lastCommittedOpTime",
        "electionId",
        "$configTime",
        "$topologyTime",
    ] {
        reply.remove(key);
    }
    reply
}

fn first_batch(reply: &Document) -> Vec<Document> {
    reply
        .get_document("cursor")
        .and_then(|c| c.get_array("firstBatch"))
        .map(|b| b.iter().filter_map(|v| v.as_document().cloned()).collect())
        .unwrap_or_default()
}

/// Runs a plan and reads its reply.
pub async fn run_plan(client: &Client, plan: Plan, txn: Option<&mut ClientSession>) -> DbResult<Output> {
    if plan.shape == Shape::Cursor {
        return cursor(client, &plan.database, plan.command, txn).await.map(|r| Output::Rows(Box::new(r)));
    }
    let reply = command(client, &plan.database, plan.command, txn).await?;
    let n = || int_field(&reply, "n").unwrap_or(0).max(0) as u64;
    Ok(match plan.shape {
        Shape::Reply => Output::Docs(vec![reply_row(reply)]),
        Shape::Field { field, name } => {
            Output::Docs(vec![doc! { name: reply.get(field).cloned().unwrap_or(Bson::Null) }])
        }
        Shape::FirstBatch => Output::Docs(first_batch(&reply)),
        Shape::CountDocuments => {
            let n = first_batch(&reply).first().and_then(|d| int_field(d, "n")).unwrap_or(0);
            Output::Docs(vec![doc! { "count": n }])
        }
        Shape::Distinct => Output::Docs(
            reply
                .get_array("values")
                .map(|v| v.iter().map(|v| doc! { "value": v.clone() }).collect())
                .unwrap_or_default(),
        ),
        Shape::Databases => Output::Docs(
            reply
                .get_array("databases")
                .map(|d| d.iter().filter_map(|v| v.as_document().cloned()).collect())
                .unwrap_or_default(),
        ),
        Shape::FindAndModify => Output::Docs(reply.get_document("value").map(|d| vec![d.clone()]).unwrap_or_default()),
        Shape::Insert { ids } => {
            let shown: Vec<String> = ids.iter().take(20).map(literal::compact).collect();
            let more = if ids.len() > 20 { format!(" and {} more", ids.len() - 20) } else { String::new() };
            Output::Done { affected: Some(n()), notices: vec![format!("Inserted ids: {}{more}", shown.join(", "))] }
        }
        Shape::Update => {
            let upserted: Vec<String> = reply
                .get_array("upserted")
                .map(|u| u.iter().filter_map(|v| v.as_document()?.get("_id")).map(literal::compact).collect())
                .unwrap_or_default();
            let mut notices = vec![format!(
                "Matched {} · modified {}",
                n() - upserted.len() as u64,
                int_field(&reply, "nModified").unwrap_or(0)
            )];
            if !upserted.is_empty() {
                notices.push(format!("Upserted ids: {}", upserted.join(", ")));
            }
            Output::Done { affected: Some(n()), notices }
        }
        Shape::Delete => Output::Done { affected: Some(n()), notices: Vec::new() },
        // Cursor plans returned above.
        Shape::Done | Shape::Cursor => Output::Done { affected: None, notices: Vec::new() },
    })
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// The cursor of the last result, for "fetch more".
struct Open {
    rows: Rows,
    pending: Option<Document>,
    columns: GridColumns,
}

/// State of the query path.
struct QueryState {
    database: String,
    txn: Option<ClientSession>,
    open: Option<Open>,
}

/// Reads up to `limit` documents, then one more to learn whether more exist.
async fn read_page(
    rows: &mut Rows,
    pending: &mut Option<Document>,
    mut txn: Option<&mut ClientSession>,
    limit: u32,
) -> DbResult<(Vec<Document>, bool)> {
    let mut out: Vec<Document> = pending.take().into_iter().collect();
    while out.len() < limit as usize {
        match rows.next(txn.as_deref_mut()).await? {
            Some(d) => out.push(d),
            None => return Ok((out, false)),
        }
    }
    *pending = rows.next(txn).await?;
    Ok((out, pending.is_some()))
}

pub struct MongoSession {
    pub(crate) client: Client,
    _connected: Connected,
    info: ServerInfo,
    read_only: bool,
    state: Mutex<QueryState>,
    /// The comment of the running editor statement, for `killOp`.
    running: SyncMutex<Option<String>>,
    cancelled: Notify,
}

impl MongoSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let connected = crate::client::connect(params).await?;
        let client = connected.client.clone();
        let config = &params.config;
        let build = command(&client, "admin", doc! { "buildInfo": 1 }, None).await?;
        let status = command(&client, "admin", doc! { "connectionStatus": 1 }, None).await.ok();
        let user = status
            .as_ref()
            .and_then(|s| s.get_document("authInfo").ok())
            .and_then(|a| a.get_array("authenticatedUsers").ok())
            .and_then(|u| u.first()?.as_document()?.get_str("user").ok().map(str::to_owned))
            .unwrap_or_else(|| config.username.clone());
        let database = match config.database.trim() {
            "" => "test".to_owned(),
            db => db.to_owned(),
        };
        let tls = config.ssl_mode != dq_core::config::SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version: format!("MongoDB {}", build.get_str("version").unwrap_or("")),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: tls.then(|| TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user,
            database: database.clone(),
            read_only: config.read_only,
        };
        Ok(Self {
            client,
            _connected: connected,
            read_only: info.read_only,
            info,
            state: Mutex::new(QueryState { database, txn: None, open: None }),
            running: SyncMutex::new(None),
            cancelled: Notify::new(),
        })
    }

    /// The database the connection opened with.
    pub(crate) fn info_database(&self) -> String {
        self.info.database.clone()
    }

    pub(crate) fn database_or_current<'a>(&'a self, database: &'a str) -> &'a str {
        if database.is_empty() { &self.info.database } else { database }
    }

    pub(crate) fn refuse_if_read_only(&self) -> DbResult<()> {
        if self.read_only { Err(read_only_error()) } else { Ok(()) }
    }

    fn set_running(&self, comment: Option<String>) {
        if let Ok(mut r) = self.running.lock() {
            *r = comment;
        }
    }

    /// Stops the server operations tagged with `comment`.
    async fn kill_by_comment(&self, comment: &str) -> DbResult<bool> {
        let ops = documents(
            &self.client,
            "admin",
            doc! {
                "aggregate": 1,
                "pipeline": [
                    { "$currentOp": { "allUsers": false, "idleConnections": false } },
                    { "$match": { "$or": [
                        { "command.comment": comment },
                        { "cursor.originatingCommand.comment": comment },
                        { "originatingCommand.comment": comment },
                    ] } },
                    { "$project": { "opid": 1 } },
                ],
                "cursor": {},
            },
            100,
        )
        .await?;
        let mut killed = false;
        for op in ops {
            if let Some(opid) = op.get("opid") {
                killed |= command(&self.client, "admin", doc! { "killOp": 1, "op": opid.clone() }, None).await.is_ok();
            }
        }
        Ok(killed)
    }

    async fn run_statement(
        &self,
        state: &mut QueryState,
        statement: Statement,
        options: ExecOptions,
        comment: &str,
    ) -> DbResult<StatementResult> {
        let started = Instant::now();
        let done = |command: &str, affected: Option<u64>, notices: Vec<String>| StatementResult {
            columns: Vec::new(),
            rows: Vec::new(),
            has_more: false,
            command: command.to_owned(),
            affected,
            duration_ms: started.elapsed().as_millis() as u64,
            notices,
        };
        let (command_tag, plan) = match statement {
            Statement::Use(database) => {
                state.database.clone_from(&database);
                return Ok(done("USE", None, vec![format!("Switched to database {database}.")]));
            }
            Statement::Begin => {
                if state.txn.is_some() {
                    return Err(DbError::invalid("A transaction is already open."));
                }
                let mut session = self.client.start_session().await.map_err(map_error)?;
                session.start_transaction().await.map_err(map_error)?;
                state.txn = Some(session);
                return Ok(done("BEGIN", None, Vec::new()));
            }
            Statement::Commit | Statement::Rollback => {
                let commit = statement == Statement::Commit;
                let mut session = state.txn.take().ok_or_else(|| DbError::invalid("No transaction is open."))?;
                if commit {
                    session.commit_transaction().await.map_err(map_error)?;
                } else {
                    session.abort_transaction().await.map_err(map_error)?;
                }
                return Ok(done(if commit { "COMMIT" } else { "ROLLBACK" }, None, Vec::new()));
            }
            Statement::Show(what) => ("SHOW".to_owned(), commands::show(&what, &state.database)),
            Statement::Call(call) => {
                let max_time = u64::from(options.timeout_secs) * 1000;
                (call.method.to_uppercase(), commands::plan(&call, &state.database, comment, Some(max_time))?)
            }
        };
        let output = run_plan(&self.client, plan, state.txn.as_mut()).await?;
        let (docs, open) = match output {
            Output::Done { affected, notices } => return Ok(done(&command_tag, affected, notices)),
            Output::Docs(docs) => (docs, None),
            Output::Rows(mut rows) => {
                let mut pending = None;
                let (docs, has_more) =
                    read_page(&mut rows, &mut pending, state.txn.as_mut(), options.row_limit).await?;
                (docs, has_more.then_some((*rows, pending)))
            }
        };
        let mut docs = docs;
        let mut notices = Vec::new();
        if open.is_none() && docs.len() > options.row_limit as usize {
            notices.push(format!("Showing the first {} of {} documents.", options.row_limit, docs.len()));
            docs.truncate(options.row_limit as usize);
        }
        let (columns, rows) = grid(&docs);
        let has_more = open.is_some();
        if let Some((rows_left, pending)) = open {
            notices.push(format!(
                "Columns come from the first {} documents; fields that only later documents have are left out. Open the \
                 collection in the Documents view to see every field.",
                docs.len()
            ));
            let mut grid_columns = GridColumns::default();
            for d in &docs {
                grid_columns.observe(&fields(d, CellLimits::GRID));
            }
            state.open = Some(Open { rows: rows_left, pending, columns: grid_columns });
        }
        Ok(StatementResult {
            columns,
            rows,
            has_more,
            command: command_tag,
            affected: None,
            duration_ms: started.elapsed().as_millis() as u64,
            notices,
        })
    }

    /// Parses a statement that runs outside the query path (exports, catalog reads).
    fn detached_plan(&self, database: &str, sql: &str) -> DbResult<Plan> {
        if split_statements(sql, SqlFlavor::Mongodb).iter().any(|s| s.writes) {
            return Err(DbError::invalid("Only read-only statements can run here."));
        }
        let database = self.database_or_current(database);
        match shell::parse(sql)? {
            Statement::Call(call) => commands::plan(&call, database, "dataquery-read", None),
            Statement::Show(what) => Ok(commands::show(&what, database)),
            _ => Err(DbError::invalid("Use a db.… statement here.")),
        }
    }

    /// Runs a read-only statement and hands its documents over in batches.
    async fn read_batches(
        &self,
        database: &str,
        sql: &str,
        mut on_docs: impl FnMut(Vec<Document>) -> DbResult<()>,
    ) -> DbResult<()> {
        let plan = self.detached_plan(database, sql)?;
        match run_plan(&self.client, plan, None).await? {
            Output::Docs(docs) => on_docs(docs),
            Output::Done { .. } => Ok(()),
            Output::Rows(mut rows) => {
                let mut batch = Vec::with_capacity(STREAM_BATCH);
                while let Some(d) = rows.next(None).await? {
                    batch.push(d);
                    if batch.len() == STREAM_BATCH {
                        on_docs(std::mem::take(&mut batch))?;
                    }
                }
                on_docs(batch)
            }
        }
    }

    /// Runs one statement inside `txn`, for [`Session::run_in_transaction`].
    async fn run_in(&self, database: &str, sql: &str, txn: &mut ClientSession) -> DbResult<u64> {
        let Statement::Call(call) = shell::parse(sql)? else {
            return Err(DbError::invalid("Only db.… statements can run in a transaction."));
        };
        let plan = commands::plan(&call, database, "dataquery-tx", None)?;
        Ok(match run_plan(&self.client, plan, Some(txn)).await? {
            Output::Done { affected, .. } => affected.unwrap_or(0),
            Output::Docs(docs) => docs.len() as u64,
            Output::Rows(_) => 0,
        })
    }
}

/// Runs `work`, stopping it on cancel or after `timeout_secs` (0 = no limit).
async fn guarded<T>(work: impl Future<Output = DbResult<T>>, timeout_secs: u32, cancelled: &Notify) -> DbResult<T> {
    let limit = async {
        if timeout_secs == 0 {
            std::future::pending::<()>().await;
        }
        // The server's maxTimeMS normally answers first, with its own error.
        tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs)) + Duration::from_secs(2)).await;
    };
    tokio::select! {
        result = work => result,
        () = cancelled.notified() => Err(DbError::new(ErrorKind::Cancelled, "The statement was cancelled.")),
        () = limit => Err(DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {timeout_secs} s and was stopped."))),
    }
}

#[async_trait]
impl Session for MongoSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &MongoDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(self).await,
            Some(p) if p.kind == dq_core::node::NodeKind::Database => catalog::collections(self, &p.name).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(self, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if !params.is_empty() {
            return Err(DbError::invalid(
                "MongoDB statements take no parameters; write the values into the statement.",
            ));
        }
        let options = options.clamped();
        if self.read_only && split_statements(sql, SqlFlavor::Mongodb).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let statement = shell::parse(sql)?;
        let mut state = self.state.lock().await;
        state.open = None;
        let comment = format!("dataquery-{}", uuid::Uuid::new_v4());
        self.set_running(Some(comment.clone()));
        let result = guarded(
            self.run_statement(&mut state, statement, options, &comment),
            options.timeout_secs,
            &self.cancelled,
        )
        .await;
        self.set_running(None);
        if let Err(e) = &result
            && matches!(e.kind, ErrorKind::Timeout | ErrorKind::Cancelled)
        {
            let _ = self.kill_by_comment(&comment).await;
        }
        result
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut state = self.state.lock().await;
        let QueryState { txn, open, .. } = &mut *state;
        let mut cursor =
            open.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let (docs, has_more) =
            read_page(&mut cursor.rows, &mut cursor.pending, txn.as_mut(), limit.clamp(1, ExecOptions::MAX_ROW_LIMIT))
                .await?;
        let rows = docs.iter().map(|d| cursor.columns.row(&fields(d, CellLimits::GRID))).collect();
        if has_more {
            *open = Some(cursor);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let Some(comment) = self.running.lock().ok().and_then(|r| r.clone()) else { return Ok(false) };
        let _ = self.kill_by_comment(&comment).await;
        self.cancelled.notify_waiters();
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(self, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(self, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(self, object).await
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        catalog::schema_model(self, self.database_or_current(database)).await
    }

    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
        if !params.is_empty() {
            return Err(DbError::invalid("MongoDB statements take no parameters."));
        }
        let mut all = Vec::new();
        self.read_batches(database, sql, |docs| {
            all.extend(docs.iter().map(|d| fields(d, limits)));
            Ok(())
        })
        .await?;
        Ok(dq_core::document::to_grid(&all))
    }

    async fn stream_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        if !params.is_empty() {
            return Err(DbError::invalid("MongoDB statements take no parameters."));
        }
        let mut stream = DocumentRows::new(on_batch);
        self.read_batches(database, sql, |docs| {
            let batch: Vec<_> = docs.iter().map(|d| fields(d, CellLimits::FULL)).collect();
            stream.push(&batch)
        })
        .await?;
        Ok(stream.rows())
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::estimate(self, &object.database, &object.name).await.map(Some)
    }

    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        self.refuse_if_read_only()?;
        let database = self.database_or_current(database).to_owned();
        let mut session = self.client.start_session().await.map_err(map_error)?;
        session.start_transaction().await.map_err(map_error)?;
        let mut counts = Vec::new();
        let outcome: DbResult<()> = async {
            for statement in statements {
                let statement = statement?;
                if !statement.params.is_empty() {
                    return Err(DbError::invalid("MongoDB statements take no parameters."));
                }
                let affected = self.run_in(&database, &statement.sql, &mut session).await?;
                check_affected(&statement, affected)?;
                counts.push(affected);
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => {
                session.commit_transaction().await.map_err(map_error)?;
                Ok(counts)
            }
            Err(e) => {
                let _ = session.abort_transaction().await;
                Err(e)
            }
        }
    }

    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        if !params.is_empty() {
            return Err(DbError::invalid("MongoDB statements take no parameters."));
        }
        let Statement::Call(mut call) = shell::parse(sql)? else {
            return Err(DbError::invalid("Explain a db.… statement, e.g. db.orders.find({ status: 'paid' })."));
        };
        call.chain.retain(|(name, _)| name != "explain");
        let database = self.state.lock().await.database.clone();
        explain::run(self, &call_plan(&call, &database)?, analyze, timeout_secs).await
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    fn documents(&self) -> Option<&dyn DocumentStore> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        let mut state = self.state.lock().await;
        state.open = None;
        if let Some(mut txn) = state.txn.take() {
            let _ = txn.abort_transaction().await;
        }
    }
}

fn call_plan(call: &Call, database: &str) -> DbResult<Plan> {
    let plan = commands::plan(call, database, "dataquery-explain", None)?;
    if !plan.explainable() {
        return Err(DbError::unsupported(format!(
            "{}() has no query plan. Explain find, aggregate, count or distinct.",
            call.method
        )));
    }
    Ok(plan)
}
