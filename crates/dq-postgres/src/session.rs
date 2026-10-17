//! An open PostgreSQL session.
//!
//! User statements run on a dedicated query connection that holds at most one
//! open cursor. Catalog work uses separate read-only connections (one per
//! database, opened lazily) so the explorer keeps working while a query runs.
//! Writes from the grid and imports use a short-lived connection per transaction.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{Dialect, PostgresDialect, SqlFlavor, command_tag};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use futures_util::{StreamExt, TryStreamExt};
use tokio::sync::{Mutex, mpsc};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client, RowStream};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::catalog;
use crate::connect::{self, Flavor, Role};
use crate::error::map_error;
use crate::values::{TextParam, column_meta, decode_row};

const STREAM_BATCH: usize = 1_000;

fn text_params(params: Params<'_>) -> impl ExactSizeIterator<Item = TextParam<'_>> {
    params.iter().map(|p| TextParam(p.as_deref()))
}

fn check_param_count(expected: usize, given: usize) -> DbResult<()> {
    if expected == given {
        return Ok(());
    }
    Err(DbError::invalid(format!("The statement has {expected} parameter(s) but {given} value(s) were given."))
        .with_action(Some("Fill in a value for every $n placeholder.")))
}

pub struct PgSession {
    /// Kept in memory only, to open catalog and edit connections.
    params: ConnectParams,
    info: ServerInfo,
    pub(crate) flavor: Flavor,
    tls: MakeRustlsConnect,
    query: Mutex<QueryConnection>,
    cancel_token: CancelToken,
    running: AtomicBool,
    /// Read-only catalog connections by database, most recently used last.
    catalogs: Mutex<Vec<(String, Arc<Client>)>>,
}

struct QueryConnection {
    client: Client,
    notices: mpsc::UnboundedReceiver<String>,
    cursor: Option<Cursor>,
    timeout_secs: Option<u32>,
}

struct Cursor {
    stream: Pin<Box<RowStream>>,
    /// A row read ahead to learn whether more rows exist.
    pending: Option<tokio_postgres::Row>,
}

impl PgSession {
    pub(crate) async fn open(params: &ConnectParams, tls: MakeRustlsConnect) -> DbResult<Self> {
        let started = Instant::now();
        let config = &params.config;
        let conn = connect::open(params, &config.database, Role::Query, &tls).await?;
        let (info, flavor) = connect::server_info(&conn.client, config, started).await?;
        Ok(Self {
            cancel_token: conn.client.cancel_token(),
            query: Mutex::new(QueryConnection {
                client: conn.client,
                notices: conn.notices,
                cursor: None,
                timeout_secs: None,
            }),
            params: params.clone(),
            info,
            flavor,
            tls,
            running: AtomicBool::new(false),
            catalogs: Mutex::new(Vec::new()),
        })
    }

    /// A read-only catalog connection, for monitoring.
    pub(crate) async fn catalog_client(&self, database: &str) -> DbResult<Arc<Client>> {
        self.catalog(database).await
    }

    pub(crate) fn info_version(&self) -> String {
        self.info.version.clone()
    }

    pub(crate) fn info_database(&self) -> String {
        self.info.database.clone()
    }

    fn err(&self, e: &tokio_postgres::Error) -> DbError {
        map_error(e, self.params.password())
    }

    /// Read-only catalog connection for `database`, opened on first use and
    /// reopened if the server closed it. At most `pool_size` stay open; the
    /// least recently used one is closed first. The lock is not held while
    /// connecting, so one slow handshake never blocks other databases.
    async fn catalog(&self, database: &str) -> DbResult<Arc<Client>> {
        let database = if database.is_empty() { &self.info.database } else { database };
        if let Some(client) = Self::cached_catalog(&mut *self.catalogs.lock().await, database) {
            return Ok(client);
        }
        let conn = connect::open(&self.params, database, Role::Catalog, &self.tls).await?;
        let mut catalogs = self.catalogs.lock().await;
        // Another task may have connected meanwhile; keep theirs and drop ours.
        if let Some(client) = Self::cached_catalog(&mut catalogs, database) {
            return Ok(client);
        }
        catalogs.retain(|(db, c)| db != database && !c.is_closed());
        let client = Arc::new(conn.client);
        let max = self.params.config.options.pool_size.max(1) as usize;
        while catalogs.len() >= max {
            catalogs.remove(0);
        }
        catalogs.push((database.to_owned(), client.clone()));
        Ok(client)
    }

    /// The open catalog connection for `database`, marked most recently used.
    fn cached_catalog(catalogs: &mut Vec<(String, Arc<Client>)>, database: &str) -> Option<Arc<Client>> {
        let i = catalogs.iter().position(|(db, c)| db == database && !c.is_closed())?;
        let entry = catalogs.remove(i);
        let client = entry.1.clone();
        catalogs.push(entry);
        Some(client)
    }

    /// Stops the open cursor, if any. The cancel request completes before we
    /// drain, so it can never hit the next statement.
    async fn close_cursor(&self, q: &mut QueryConnection) {
        if let Some(mut cursor) = q.cursor.take() {
            let _ = self.cancel_token.cancel_query(self.tls.clone()).await;
            while let Some(Ok(_)) = cursor.stream.next().await {}
        }
    }

    async fn set_timeout(&self, q: &mut QueryConnection, secs: u32) -> DbResult<()> {
        if q.timeout_secs != Some(secs) {
            q.client
                .batch_execute(&format!("SET statement_timeout = {}", u64::from(secs) * 1000))
                .await
                .map_err(|e| self.err(&e))?;
            q.timeout_secs = Some(secs);
        }
        Ok(())
    }

    /// Reads up to `limit` rows, then one more to learn whether more exist.
    async fn read_rows(&self, cursor: &mut Cursor, limit: u32) -> DbResult<(Vec<CellRow>, bool)> {
        let mut rows = Vec::with_capacity(limit.min(1024) as usize);
        if let Some(row) = cursor.pending.take() {
            rows.push(decode_row(&row, CellLimits::GRID));
        }
        while rows.len() < limit as usize {
            match cursor.stream.try_next().await.map_err(|e| self.err(&e))? {
                Some(row) => rows.push(decode_row(&row, CellLimits::GRID)),
                None => return Ok((rows, false)),
            }
        }
        cursor.pending = cursor.stream.try_next().await.map_err(|e| self.err(&e))?;
        Ok((rows, cursor.pending.is_some()))
    }

    async fn run(
        &self,
        q: &mut QueryConnection,
        sql: &str,
        params: Params<'_>,
        options: ExecOptions,
    ) -> DbResult<StatementResult> {
        let started = Instant::now();
        let statement = q.client.prepare(sql).await.map_err(|e| self.err(&e))?;
        check_param_count(statement.params().len(), params.len())?;
        let columns = column_meta(statement.columns());
        let stream = q.client.query_raw(&statement, text_params(params)).await.map_err(|e| self.err(&e))?;
        let mut cursor = Cursor { stream: Box::pin(stream), pending: None };
        let (rows, has_more) = self.read_rows(&mut cursor, options.row_limit).await?;
        let affected = if has_more { None } else { cursor.stream.rows_affected() };
        if has_more {
            q.cursor = Some(cursor);
        }
        let command = command_tag(sql, SqlFlavor::Postgres);
        Ok(StatementResult {
            affected: if columns.is_empty() { affected } else { None },
            columns,
            rows,
            has_more,
            command,
            duration_ms: started.elapsed().as_millis() as u64,
            notices: Vec::new(),
        })
    }
}

#[async_trait]
impl Session for PgSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &PostgresDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(&*self.catalog("").await?, &self.info.database).await,
            Some(p) if p.kind == NodeKind::Database => catalog::schemas(&*self.catalog(&p.name).await?, &p.name).await,
            Some(p) if p.kind == NodeKind::Schema => {
                catalog::relations(&*self.catalog(&p.database).await?, &p.database, &p.name).await
            }
            Some(p) if matches!(p.kind, NodeKind::Table | NodeKind::MaterializedView) => {
                catalog::table_children(&*self.catalog(&p.database).await?, p).await
            }
            Some(p) if p.kind == NodeKind::Group => {
                catalog::group_children(&*self.catalog(&p.database).await?, p).await
            }
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&*self.catalog("").await?, &self.info.database, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let options = options.clamped();
        let mut q = self.query.lock().await;
        self.close_cursor(&mut q).await;
        while q.notices.try_recv().is_ok() {}
        self.set_timeout(&mut q, options.timeout_secs).await?;
        self.running.store(true, Ordering::SeqCst);
        let result = self.run(&mut q, sql, params, options).await;
        self.running.store(false, Ordering::SeqCst);
        let mut notices = Vec::new();
        while let Ok(n) = q.notices.try_recv() {
            notices.push(n);
        }
        result.map(|r| StatementResult { notices, ..r })
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut q = self.query.lock().await;
        let mut cursor =
            q.cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        self.running.store(true, Ordering::SeqCst);
        let read = self.read_rows(&mut cursor, limit.clamp(1, ExecOptions::MAX_ROW_LIMIT)).await;
        self.running.store(false, Ordering::SeqCst);
        let (rows, has_more) = read?;
        if has_more {
            q.cursor = Some(cursor);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.cancel_token.cancel_query(self.tls.clone()).await.map_err(|e| self.err(&e))?;
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&*self.catalog(&object.database).await?, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&*self.catalog(&object.database).await?, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        catalog::dependencies(&*self.catalog(&object.database).await?, object).await
    }

    async fn schema_model(&self, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&*self.catalog(database).await?, database, schema).await
    }

    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<CellRow>)> {
        let client = self.catalog(database).await?;
        let statement = client.prepare(sql).await.map_err(|e| self.err(&e))?;
        check_param_count(statement.params().len(), params.len())?;
        let rows: Vec<_> = client
            .query_raw(&statement, text_params(params))
            .await
            .map_err(|e| self.err(&e))?
            .try_collect()
            .await
            .map_err(|e| self.err(&e))?;
        Ok((column_meta(statement.columns()), rows.iter().map(|r| decode_row(r, limits)).collect()))
    }

    async fn stream_query(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        // A connection of its own: a long export would otherwise hold up every
        // catalog call queued behind it on the shared catalog connection.
        let database = if database.is_empty() { &self.info.database } else { database };
        let client = connect::open(&self.params, database, Role::Export, &self.tls).await?.client;
        let statement = client.prepare(sql).await.map_err(|e| self.err(&e))?;
        check_param_count(statement.params().len(), params.len())?;
        let columns = column_meta(statement.columns());
        let stream = client.query_raw(&statement, text_params(params)).await.map_err(|e| self.err(&e))?;
        let mut stream = std::pin::pin!(stream);
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        while let Some(row) = stream.try_next().await.map_err(|e| self.err(&e))? {
            batch.push(decode_row(&row, CellLimits::FULL));
            if batch.len() == STREAM_BATCH {
                total += batch.len() as u64;
                on_batch(&columns, std::mem::take(&mut batch))?;
            }
        }
        total += batch.len() as u64;
        on_batch(&columns, batch)?;
        Ok(total)
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        catalog::row_estimate(&*self.catalog(&object.database).await?, object).await
    }

    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        let database = if database.is_empty() { self.info.database.clone() } else { database.to_owned() };
        let mut conn = connect::open(&self.params, &database, Role::Write, &self.tls).await?;
        let tx = conn.client.transaction().await.map_err(|e| self.err(&e))?;
        let mut counts = Vec::new();
        for statement in statements {
            let statement = statement?;
            let params: Vec<&(dyn ToSql + Sync)> = statement.params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
            let affected = tx.execute(statement.sql.as_str(), &params).await.map_err(|e| self.err(&e))?;
            check_affected(&statement, affected)?;
            counts.push(affected);
        }
        tx.commit().await.map_err(|e| self.err(&e))?;
        Ok(counts)
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
        self.catalogs.lock().await.clear();
    }
}
