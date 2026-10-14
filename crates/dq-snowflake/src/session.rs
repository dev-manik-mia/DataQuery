//! An open Snowflake session.
//!
//! Statements run over HTTPS on one Snowflake session; catalog reads share
//! it (they name every object fully). Grid edits and imports use a second
//! session, so their transaction never mixes with one the user opened in
//! the editor. Snowflake has no read-only session mode, so read-only
//! profiles are enforced here.

use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, SnowflakeDialect, SqlFlavor, command_tag, split_statements};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult, ErrorKind};
use tokio::sync::{Mutex, Notify};

use crate::catalog;
use crate::client::{Client, QueryData};
use crate::results::ResultSet;

pub struct SfSession {
    client: Arc<Client>,
    params: ConnectParams,
    info: ServerInfo,
    cursor: Mutex<Option<ResultSet>>,
    /// The running statement's request id and text, for `abort-request`.
    running: SyncMutex<Option<(String, String)>>,
    cancelled: Notify,
    writer: Mutex<Option<Arc<Client>>>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// Rows a DML statement changed: the sum of its `number of rows …` values.
fn affected(data: &QueryData) -> Option<u64> {
    if !data.is_dml() {
        return None;
    }
    let row = data.rowset.as_ref()?.first()?;
    Some(row.iter().filter_map(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64())).sum())
}

impl SfSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let client = Arc::new(Client::new(params)?);
        let login = client.login().await?;
        let (_, found) =
            catalog::query(&client, "SELECT CURRENT_VERSION(), CURRENT_USER(), CURRENT_DATABASE()", &[]).await?;
        let row = found.into_iter().next().unwrap_or_default();
        let config = &params.config;
        let tls = config.ssl_mode != dq_core::config::SslMode::Disable;
        let database = row
            .text(2)
            .or_else(|| login.pointer("/sessionInfo/databaseName").and_then(|v| v.as_str()).map(str::to_owned));
        let info = ServerInfo {
            engine: config.engine,
            version: format!("Snowflake {}", row.string(0).trim_matches('\'')),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: tls.then(|| dq_core::result::TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user: row.string(1),
            database: database.unwrap_or_default(),
            read_only: config.read_only,
        };
        Ok(Self {
            client,
            params: params.clone(),
            info,
            cursor: Mutex::new(None),
            running: SyncMutex::new(None),
            cancelled: Notify::new(),
            writer: Mutex::new(None),
        })
    }

    /// Runs a statement; `cancel` and the timeout abort it on the server and return at once.
    async fn run_query(&self, sql: &str, binds: &[Option<String>], timeout_secs: u32) -> DbResult<QueryData> {
        let request_id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut r) = self.running.lock() {
            *r = Some((request_id.clone(), sql.to_owned()));
        }
        let limit = if timeout_secs == 0 {
            Duration::from_secs(u64::MAX / 4)
        } else {
            Duration::from_secs(u64::from(timeout_secs))
        };
        let outcome = tokio::select! {
            result = self.client.query(sql, binds, &request_id) => result,
            () = self.cancelled.notified() => Err(DbError::new(ErrorKind::Cancelled, "The statement was cancelled.")),
            () = tokio::time::sleep(limit) => {
                let _ = self.client.abort(sql, &request_id).await;
                Err(DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {timeout_secs} s and was stopped."))
                    .with_action(Some("Narrow the query or raise the statement timeout in Settings.")))
            }
        };
        if let Ok(mut r) = self.running.lock() {
            *r = None;
        }
        outcome
    }

    async fn writer(&self) -> DbResult<Arc<Client>> {
        let mut writer = self.writer.lock().await;
        if let Some(w) = writer.as_ref() {
            return Ok(w.clone());
        }
        let client = Arc::new(Client::new(&self.params)?);
        client.login().await?;
        *writer = Some(client.clone());
        Ok(client)
    }

    async fn read_batches(
        &self,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let data = self.client.query(sql, params, &uuid::Uuid::new_v4().to_string()).await?;
        let mut result = ResultSet::new(data, limits)?;
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
}

#[async_trait]
impl Session for SfSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SnowflakeDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(&self.client, &self.info.database).await,
            Some(p) if p.kind == NodeKind::Database => catalog::schemas(&self.client, &p.name).await,
            Some(p) if p.kind == NodeKind::Schema => catalog::relations(&self.client, &p.database, &p.name).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(&self.client, p).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        if self.info.database.is_empty() {
            return Ok(Vec::new());
        }
        catalog::search(&self.client, &self.info.database, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.info.read_only && split_statements(sql, SqlFlavor::Snowflake).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        let started = Instant::now();
        let mut cursor = self.cursor.lock().await;
        *cursor = None;
        let data = self.run_query(sql, params, options.timeout_secs).await?;
        let command = command_tag(sql, SqlFlavor::Snowflake);
        if let Some(affected) = affected(&data) {
            return Ok(StatementResult {
                columns: Vec::new(),
                rows: Vec::new(),
                has_more: false,
                command,
                affected: Some(affected),
                duration_ms: started.elapsed().as_millis() as u64,
                notices: Vec::new(),
            });
        }
        let mut result = ResultSet::new(data, CellLimits::GRID)?;
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
            notices: Vec::new(),
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
        let Some((request_id, sql)) = self.running.lock().ok().and_then(|r| r.clone()) else { return Ok(false) };
        // Stop waiting at once, and ask the server to stop the statement.
        self.cancelled.notify_waiters();
        let _ = self.client.abort(&sql, &request_id).await;
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
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&self.client, database, schema.unwrap_or("PUBLIC")).await
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

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.info.read_only {
            return Err(read_only_error());
        }
        let writer = self.writer().await?;
        let id = || uuid::Uuid::new_v4().to_string();
        writer.query("BEGIN TRANSACTION", &[], &id()).await?;
        let outcome: DbResult<Vec<u64>> = async {
            let mut counts = Vec::new();
            for statement in statements {
                let statement = statement?;
                let data = writer.query(&statement.sql, &statement.params, &id()).await?;
                let changed = affected(&data).unwrap_or(0);
                check_affected(&statement, changed)?;
                counts.push(changed);
            }
            writer.query("COMMIT", &[], &id()).await?;
            Ok(counts)
        }
        .await;
        if outcome.is_err() {
            let _ = writer.query("ROLLBACK", &[], &id()).await;
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
        if analyze {
            return Err(DbError::unsupported(
                "Snowflake shows measured plans in the Query Profile of Snowsight, not through EXPLAIN.",
            ));
        }
        let statement = format!("EXPLAIN USING JSON {}", sql.trim().trim_end_matches(';'));
        let result = self.execute(&statement, params, ExecOptions { row_limit: 100, timeout_secs }).await?;
        let raw = result.rows.iter().filter_map(|r| r.first()).filter_map(Cell::to_text).collect::<Vec<_>>().join("\n");
        crate::explain::parse(&raw)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        self.client.logout().await;
        if let Some(writer) = self.writer.lock().await.take() {
            writer.logout().await;
        }
    }
}
