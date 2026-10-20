//! An open `BigQuery` connection.
//!
//! Every statement is a query job with an id of our own, so cancel and the
//! statement timeout stop it on the server at once (`jobs.cancel`). A script
//! (several statements, `DECLARE`, `BEGIN … END`, a transaction) is one job.
//! The API is stateless, so the explorer reads alongside a running query.
//! Grid edits and imports run in a `BigQuery` session that holds one
//! transaction across their statements. `BigQuery` has no read-only mode, so
//! read-only profiles are enforced here.

use std::sync::Mutex as SyncMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::edit::BoundStatement;
use dq_core::explain::ExplainResult;
use dq_core::monitor::Monitor;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{BigQueryDialect, Dialect, SqlFlavor, bind_named, command_tag, named_params, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

use crate::catalog;
use crate::client::{Client, JobRef, JobSpec, QueryPage, new_job_id};
use crate::explain;
use crate::results::ResultSet;
use crate::values::parameter;

pub struct BqSession {
    pub(crate) client: Client,
    info: ServerInfo,
    cursor: Mutex<Option<ResultSet>>,
    /// The running user statement's job, for `jobs.cancel`.
    running: SyncMutex<Option<JobRef>>,
    cancelled: Notify,
    /// Jobs of the last monitoring snapshot, so a process id can be cancelled.
    listed: SyncMutex<Vec<JobRef>>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// Query parameters for `values`: named when the statement uses `@name`,
/// else positional (`?`). Editor values are typed by their spelling
/// ([`crate::values::infer_type`]); generated statements cast `STRING`s.
fn parameters(sql: &str, values: Params<'_>, infer: bool) -> DbResult<(Vec<Value>, bool)> {
    if values.is_empty() {
        return Ok((Vec::new(), true));
    }
    if named_params(sql, SqlFlavor::Bigquery).is_empty() {
        return Ok((values.iter().map(|v| parameter(None, v.as_deref(), infer)).collect(), false));
    }
    let bound = bind_named(sql, SqlFlavor::Bigquery, values)?;
    Ok((bound.iter().map(|(name, v)| parameter(Some(name), v.as_deref(), infer)).collect(), true))
}

impl BqSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let client = Client::open(params).await?;
        // Checks the credentials and the project without running (or billing) a job.
        client.get(&client.project, &["datasets"], &[("maxResults", "1")]).await?;
        let config = &params.config;
        let info = ServerInfo {
            engine: config.engine,
            version: "BigQuery (REST API v2)".into(),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: client.tls.then(|| TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user: client.account().unwrap_or_default().to_owned(),
            database: client.project.clone(),
            read_only: config.read_only,
        };
        Ok(Self {
            client,
            info,
            cursor: Mutex::new(None),
            running: SyncMutex::new(None),
            cancelled: Notify::new(),
            listed: SyncMutex::new(Vec::new()),
        })
    }

    pub(crate) fn remember_jobs(&self, jobs: Vec<JobRef>) {
        if let Ok(mut listed) = self.listed.lock() {
            *listed = jobs;
        }
    }

    pub(crate) fn listed_job(&self, pid: i64) -> Option<JobRef> {
        let listed = self.listed.lock().ok()?;
        listed.iter().find(|j| crate::monitor::pid_of(&j.job_id) == pid).cloned()
    }

    fn set_running(&self, job: Option<JobRef>) {
        if let Ok(mut r) = self.running.lock() {
            *r = job;
        }
    }

    /// Runs a user job; cancel and the timeout stop it on the server and return at once.
    async fn run_tracked(&self, spec: &JobSpec<'_>, rows: u32) -> DbResult<(JobRef, QueryPage)> {
        let job_id = new_job_id();
        self.set_running(Some(self.client.job_ref(&job_id, spec)));
        let limit = if spec.timeout_secs == 0 {
            Duration::from_secs(u64::MAX / 4)
        } else {
            // A moment longer than the server-side limit, which reports the timeout itself.
            Duration::from_secs(u64::from(spec.timeout_secs) + 5)
        };
        let outcome = tokio::select! {
            result = self.client.run(spec, &job_id, rows, |job| self.set_running(Some(job.clone()))) => result,
            () = self.cancelled.notified() => Err(DbError::new(ErrorKind::Cancelled, "The statement was cancelled.")),
            () = tokio::time::sleep(limit) => {
                if let Some(job) = self.running.lock().ok().and_then(|r| r.clone()) {
                    let _ = self.client.cancel(&job).await;
                }
                Err(DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {} s and was stopped.", spec.timeout_secs))
                    .with_action(Some("Narrow the query or raise the statement timeout in Settings.")))
            }
        };
        self.set_running(None);
        outcome
    }

    /// The job's statistics (bytes, statement type), or nothing where they are unavailable.
    async fn statistics(&self, job: &JobRef) -> Value {
        self.client.job(job).await.map(|mut j| j["statistics"].take()).unwrap_or_default()
    }

    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let (params, named) = parameters(sql, params, false)?;
        let spec = JobSpec {
            sql,
            params,
            named,
            default_dataset: true,
            location: self.client.location.as_deref(),
            ..JobSpec::default()
        };
        let (job, page) = self.client.run(&spec, &new_job_id(), 2_000, |_| {}).await?;
        let mut result = ResultSet::new(job, page, limits);
        let meta = result.columns();
        let mut total = 0u64;
        loop {
            let (rows, more) = result.take(&self.client, 2_000).await?;
            total += rows.len() as u64;
            on_batch(&meta, rows)?;
            if !more {
                return Ok(total);
            }
        }
    }

    /// Where a statement runs: the connection's location, else the one `BigQuery`
    /// picks for it in a dry run (from the tables it touches).
    async fn locate(&self, statement: &BoundStatement) -> Option<String> {
        if let Some(location) = &self.client.location {
            return Some(location.clone());
        }
        let (params, named) = parameters(&statement.sql, &statement.params, false).ok()?;
        let spec = JobSpec { sql: &statement.sql, params, named, dry_run: true, ..JobSpec::default() };
        let job = self.client.insert_job(&spec, &new_job_id()).await.ok()?;
        job.pointer("/jobReference/location").and_then(Value::as_str).map(str::to_owned)
    }

    /// One statement of a grid save or import, in the transaction; the rows it changed.
    async fn run_bound(&self, statement: &BoundStatement, tx: &Transaction) -> DbResult<u64> {
        let (params, named) = parameters(&statement.sql, &statement.params, false)?;
        let spec = JobSpec { sql: &statement.sql, params, named, ..tx.spec("") };
        let (_, page) = self.client.run(&spec, &new_job_id(), 0, |_| {}).await?;
        // Emulators do not report changed rows; the expectation then stands for them.
        let changed = page.affected().or(statement.expect_rows).unwrap_or(0);
        check_affected(statement, changed)?;
        Ok(changed)
    }

    /// Runs `sql` alone in the transaction's session (`COMMIT`, `ROLLBACK`, ending the session).
    async fn in_session(&self, sql: &str, tx: &Transaction) -> DbResult<()> {
        self.client.run(&tx.spec(sql), &new_job_id(), 0, |_| {}).await.map(|_| ())
    }
}

/// The `BigQuery` session and location a grid save or import runs in.
struct Transaction {
    session: Option<String>,
    location: Option<String>,
}

impl Transaction {
    fn spec<'a>(&'a self, sql: &'a str) -> JobSpec<'a> {
        JobSpec { sql, session: self.session.as_deref(), location: self.location.as_deref(), ..JobSpec::default() }
    }
}

#[async_trait]
impl Session for BqSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &BigQueryDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::projects(&self.client).await,
            Some(p) if p.kind == NodeKind::Database => catalog::datasets(&self.client, &p.name).await,
            Some(p) if p.kind == NodeKind::Schema => catalog::relations(&self.client, &p.database, &p.name).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(&self.client, p).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&self.client, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.info.read_only && split_statements(sql, SqlFlavor::Bigquery).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        let started = Instant::now();
        let mut cursor = self.cursor.lock().await;
        *cursor = None;
        let (params, named) = parameters(sql, params, true)?;
        let spec = JobSpec {
            sql,
            params,
            named,
            timeout_secs: options.timeout_secs,
            default_dataset: true,
            location: self.client.location.as_deref(),
            ..JobSpec::default()
        };
        let (job, page) = self.run_tracked(&spec, options.row_limit).await?;
        let notices = explain::notice(&self.statistics(&job).await).into_iter().collect();
        let command = command_tag(sql, SqlFlavor::Bigquery);
        let returns_rows = page.schema.as_ref().is_some_and(|s| !s.fields.is_empty());
        if !returns_rows {
            return Ok(StatementResult {
                columns: Vec::new(),
                rows: Vec::new(),
                has_more: false,
                command,
                affected: page.affected(),
                duration_ms: started.elapsed().as_millis() as u64,
                notices,
            });
        }
        let mut result = ResultSet::new(job, page, CellLimits::GRID);
        let columns = result.columns();
        let (rows, has_more) = result.take(&self.client, options.row_limit as usize).await?;
        if has_more {
            *cursor = Some(result);
        }
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

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut cursor = self.cursor.lock().await;
        let mut result =
            cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let (rows, has_more) = result.take(&self.client, limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize).await?;
        if has_more {
            *cursor = Some(result);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let Some(job) = self.running.lock().ok().and_then(|r| r.clone()) else { return Ok(false) };
        // Stop waiting at once, and ask BigQuery to stop the job.
        self.cancelled.notify_waiters();
        let _ = self.client.cancel(&job).await;
        Ok(true)
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

    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let project = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&self.client, project, schema.or(self.client.dataset.as_deref())).await
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
        catalog::row_estimate(&self.client, object).await
    }

    /// One transaction across jobs needs a `BigQuery` session: `BEGIN TRANSACTION`
    /// opens both (where the first statement's tables are), each statement runs
    /// in it, then `COMMIT` (or `ROLLBACK`), and the session is ended. A server
    /// without sessions (an emulator) runs the statements one by one.
    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.info.read_only {
            return Err(read_only_error());
        }
        let Some(first) = statements.next().transpose()? else { return Ok(Vec::new()) };
        let mut tx = Transaction { session: None, location: self.locate(&first).await };
        let begin = JobSpec { create_session: true, ..tx.spec("BEGIN TRANSACTION") };
        let (job, _) = self.client.run(&begin, &new_job_id(), 0, |_| {}).await?;
        let statistics = self.statistics(&job).await;
        tx.session = statistics.pointer("/sessionInfo/sessionId").and_then(Value::as_str).map(str::to_owned);
        let outcome: DbResult<Vec<u64>> = async {
            let mut counts = vec![self.run_bound(&first, &tx).await?];
            for statement in statements {
                counts.push(self.run_bound(&statement?, &tx).await?);
            }
            if tx.session.is_some() {
                self.in_session("COMMIT TRANSACTION", &tx).await?;
            }
            Ok(counts)
        }
        .await;
        if tx.session.is_some() {
            if outcome.is_err() {
                let _ = self.in_session("ROLLBACK TRANSACTION", &tx).await;
            }
            let _ = self.in_session("CALL BQ.ABORT_SESSION()", &tx).await;
        }
        outcome
    }

    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        let (params, named) = parameters(sql, params, true)?;
        let spec = JobSpec {
            sql,
            params,
            named,
            dry_run: !analyze,
            timeout_secs,
            default_dataset: true,
            location: self.client.location.as_deref(),
            ..JobSpec::default()
        };
        if !analyze {
            return Ok(explain::dry_run(&self.client.insert_job(&spec, &new_job_id()).await?));
        }
        let (job, _) = self.run_tracked(&spec, 0).await?;
        Ok(explain::analyzed(&self.client.job(&job).await?))
    }

    fn monitor(&self) -> Option<&dyn Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
    }
}
