//! An open SQL Server session.
//!
//! User statements run on a query connection owned by a worker task (see
//! [`crate::worker`]). Catalog reads, table pages and exports use a small pool
//! of other connections, so the explorer keeps working while a query runs.
//! Grid edits and imports use a short-lived connection per transaction.

use std::sync::Arc;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::ExplainResult;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, SqlFlavor, SqlServerDialect, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use futures_util::TryStreamExt;
use tiberius::{QueryItem, ToSql};
use tokio::sync::{mpsc, oneshot};

use crate::catalog;
use crate::connect::{self, Conn};
use crate::error::map_error;
use crate::pool::Pool;
use crate::values::{column_meta, decode_row};
use crate::worker::{CancelSignal, Command, Worker};

const STREAM_BATCH: usize = 1_000;

pub struct MsSession {
    info: ServerInfo,
    /// Session id of the query connection, hidden from the process list.
    pub(crate) spid: i64,
    worker: mpsc::Sender<Command>,
    signal: Arc<CancelSignal>,
    pub(crate) pool: Pool,
}

fn read_only_error() -> DbError {
    DbError::new(dq_core::ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

impl MsSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let opened = connect::open_query(params).await?;
        let signal = Arc::new(CancelSignal::default());
        Ok(Self {
            worker: Worker::spawn(params.clone(), opened.conn, signal.clone()),
            signal,
            pool: Pool::new(params.clone(), opened.info.database.clone()),
            spid: opened.spid,
            info: opened.info,
        })
    }

    fn params(&self) -> &ConnectParams {
        self.pool.params()
    }

    fn err(&self, e: &tiberius::error::Error) -> DbError {
        map_error(e, self.params().password())
    }

    async fn request<T>(&self, make: impl FnOnce(oneshot::Sender<DbResult<T>>) -> Command) -> DbResult<T> {
        let (tx, rx) = oneshot::channel();
        self.worker.send(make(tx)).await.map_err(|_| DbError::invalid("The session is closed."))?;
        rx.await.map_err(|_| DbError::internal("the query connection stopped"))?
    }

    /// Streams a read-only query on a pooled connection in batches.
    async fn read_batches(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let mut pooled = self.pool.get(database).await?;
        let result = self.read_on(pooled.conn(), sql, params, limits, on_batch).await;
        pooled.check(result)
    }

    async fn read_on(
        &self,
        conn: &mut Conn,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let binds: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
        let mut stream = if binds.is_empty() { conn.simple_query(sql).await } else { conn.query(sql, &binds).await }
            .map_err(|e| self.err(&e))?;
        let mut meta: Vec<ColumnMeta> = Vec::new();
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        let mut total = 0u64;
        let mut started = false;
        while let Some(item) = stream.try_next().await.map_err(|e| self.err(&e))? {
            match item {
                QueryItem::Metadata(_) if started => break,
                QueryItem::Metadata(m) => {
                    meta = column_meta(m.columns());
                    started = true;
                }
                QueryItem::Row(row) => {
                    batch.push(decode_row(&row, limits));
                    if batch.len() == STREAM_BATCH {
                        total += batch.len() as u64;
                        on_batch(&meta, std::mem::take(&mut batch))?;
                    }
                }
            }
        }
        drop(stream);
        total += batch.len() as u64;
        on_batch(&meta, batch)?;
        Ok(total)
    }

    /// The plan XML that `setup` turns on, for `sql`.
    async fn plan_xml(&self, setup: &str, teardown: &str, sql: &str, params: Params<'_>, timeout_secs: u32) -> DbResult<String> {
        let values = self
            .request(|reply| Command::Batches {
                setup: setup.to_owned(),
                sql: sql.to_owned(),
                params: params.to_vec(),
                teardown: teardown.to_owned(),
                timeout_secs,
                reply,
            })
            .await?;
        values
            .into_iter()
            .flatten()
            .find(|v| v.contains("<ShowPlanXML"))
            .ok_or_else(|| DbError::internal("the server returned no plan"))
    }

    async fn write_conn(&self, database: &str) -> DbResult<Conn> {
        if self.params().config.read_only {
            return Err(read_only_error());
        }
        connect::open(self.params(), Some(database).filter(|d| !d.is_empty())).await
    }
}

#[async_trait]
impl Session for MsSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SqlServerDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let database = parent.map_or(self.info.database.as_str(), |p| p.database.as_str());
        let mut pooled = self.pool.get(database).await?;
        let conn = pooled.conn();
        let result = match parent {
            None => catalog::databases(conn, &self.info.database).await,
            Some(p) if p.kind == NodeKind::Database => catalog::schemas(conn, &p.name).await,
            Some(p) if p.kind == NodeKind::Schema => catalog::relations(conn, &p.database, &p.name).await,
            Some(p) if p.kind == NodeKind::Table => catalog::table_children(conn, p).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(conn, p).await,
            Some(_) => Ok(Vec::new()),
        };
        pooled.check(result)
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let mut pooled = self.pool.get(&self.info.database).await?;
        let result = catalog::search(pooled.conn(), &self.info.database, query, limit).await;
        pooled.check(result)
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        if self.params().config.read_only && split_statements(sql, SqlFlavor::Sqlserver).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        let options = options.clamped();
        self.request(|reply| Command::Execute { sql: sql.to_owned(), params: params.to_vec(), options, reply }).await
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        self.request(|reply| Command::FetchMore { limit, reply }).await
    }

    async fn cancel(&self) -> DbResult<bool> {
        Ok(self.signal.cancel())
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        let mut pooled = self.pool.get(&object.database).await?;
        let result = catalog::structure(pooled.conn(), object).await;
        pooled.check(result)
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        let mut pooled = self.pool.get(&object.database).await?;
        let result = catalog::ddl(pooled.conn(), object).await;
        pooled.check(result)
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        let mut pooled = self.pool.get(&object.database).await?;
        let result = catalog::dependencies(pooled.conn(), object).await;
        pooled.check(result)
    }

    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        let mut pooled = self.pool.get(database).await?;
        let result = catalog::schema_model(pooled.conn(), database, schema).await;
        pooled.check(result)
    }

    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        self.read_batches(database, sql, params, limits, &mut |meta, batch| {
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
        database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        self.read_batches(database, sql, params, CellLimits::FULL, on_batch).await
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        let mut pooled = self.pool.get(&object.database).await?;
        let result = catalog::row_estimate(pooled.conn(), object).await;
        pooled.check(result)
    }

    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        let mut conn = self.write_conn(database).await?;
        let err = |e: tiberius::error::Error| self.err(&e);
        // Any error rolls the whole transaction back.
        conn.simple_query("SET XACT_ABORT ON; BEGIN TRANSACTION").await.map_err(err)?.into_results().await.map_err(err)?;
        let mut counts = Vec::new();
        let outcome: DbResult<()> = async {
            for statement in statements {
                let statement = statement?;
                let binds: Vec<&dyn ToSql> = statement.params.iter().map(|p| p as &dyn ToSql).collect();
                let done = conn.execute(statement.sql.as_str(), &binds).await.map_err(err)?;
                // Trigger statements report first; the statement's own count is last.
                let affected = done.rows_affected().last().copied().unwrap_or(0);
                check_affected(&statement, affected)?;
                counts.push(affected);
            }
            conn.simple_query("COMMIT TRANSACTION").await.map_err(err)?.into_results().await.map_err(err)?;
            Ok(())
        }
        .await;
        if outcome.is_err() {
            if let Ok(s) = conn.simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await {
                let _ = s.into_results().await;
            }
        }
        let _ = conn.close().await;
        outcome.map(|()| counts)
    }

    async fn explain(&self, sql: &str, params: Params<'_>, analyze: bool, timeout_secs: u32) -> DbResult<ExplainResult> {
        let (setup, teardown) =
            if analyze { ("SET STATISTICS XML ON", "SET STATISTICS XML OFF") } else { ("SET SHOWPLAN_XML ON", "SET SHOWPLAN_XML OFF") };
        let xml = self.plan_xml(setup, teardown, sql, params, timeout_secs).await?;
        crate::explain::parse(xml, analyze)
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.signal.cancel();
        self.pool.close().await;
    }
}
