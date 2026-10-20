//! An open `ClickHouse` connection.
//!
//! The HTTP interface is stateless; user statements run in a server-side
//! session so `SET`, `USE` and temporary tables last between them. Catalog
//! reads, table pages and exports are separate read-only requests. A result
//! is read as it streams, and the open stream is kept for "fetch more".

use std::sync::Mutex as SyncMutex;
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::explain::{ExplainDialect, ExplainResult};
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row as CellRow, RowsPage, ServerInfo, StatementResult};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::{ClickhouseDialect, Dialect, SqlFlavor, StatementKind, analyze, bind_named, command_tag};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult, ErrorKind};
use tokio::sync::Mutex;

use crate::catalog;
use crate::client::{Client, Request, Rows};
use crate::values::{column_meta, decode_row};

const STREAM_BATCH: usize = 1_000;

pub struct ChSession {
    pub(crate) client: Client,
    info: ServerInfo,
    read_only: bool,
    session_id: String,
    query: Mutex<Option<Cursor>>,
    /// The query id of the running user statement, for `KILL QUERY`.
    running: SyncMutex<Option<String>>,
}

struct Cursor {
    rows: Rows,
    pending: Option<Vec<serde_json::Value>>,
    query_id: String,
}

/// Binds values to `{name:Type}` parameters: `p1`, `p2`, … by number (as
/// generated SQL names them), any other names in order of first appearance.
pub fn bind(sql: &str, values: Params<'_>) -> DbResult<Vec<(String, Option<String>)>> {
    bind_named(sql, SqlFlavor::Clickhouse, values)
}

/// Reads up to `limit` rows, then one more to learn whether more exist.
async fn read_rows(cursor: &mut Cursor, limit: u32) -> DbResult<(Vec<CellRow>, bool)> {
    let mut rows = Vec::with_capacity(limit.min(1024) as usize);
    if let Some(row) = cursor.pending.take() {
        rows.push(decode_row(&row, &cursor.rows.types, CellLimits::GRID));
    }
    while rows.len() < limit as usize {
        match cursor.rows.next().await? {
            Some(row) => rows.push(decode_row(&row, &cursor.rows.types, CellLimits::GRID)),
            None => return Ok((rows, false)),
        }
    }
    cursor.pending = cursor.rows.next().await?;
    Ok((rows, cursor.pending.is_some()))
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

impl ChSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let client = Client::new(params)?;
        let config = &params.config;
        let session_id = uuid::Uuid::new_v4().to_string();
        let database = config.database.trim();
        let row = client
            .text_rows(
                "SELECT version(), currentUser(), currentDatabase(), toString(getSetting('readonly'))",
                &Request {
                    database: Some(database),
                    session_id: Some(&session_id),
                    timeout_secs: 30,
                    ..Request::default()
                },
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DbError::internal("the server returned no version"))?;
        let field = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        // The session keeps this database until the user runs `USE`.
        if !database.is_empty() {
            let use_db = format!("USE {}", ClickhouseDialect.quote_ident(database));
            client.text_rows(&use_db, &Request { session_id: Some(&session_id), ..Request::default() }).await?;
        }
        let tls = config.ssl_mode != dq_core::config::SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version: format!("ClickHouse {}", field(0)),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: tls.then(|| dq_core::result::TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user: field(1),
            database: field(2),
            read_only: config.read_only || field(3) != "0",
        };
        Ok(Self {
            client,
            read_only: info.read_only,
            info,
            session_id,
            query: Mutex::new(None),
            running: SyncMutex::new(None),
        })
    }

    fn set_running(&self, id: Option<String>) {
        if let Ok(mut r) = self.running.lock() {
            *r = id;
        }
    }

    async fn run(
        &self,
        cursor: &mut Option<Cursor>,
        sql: &str,
        params: Params<'_>,
        options: ExecOptions,
    ) -> DbResult<StatementResult> {
        let started = Instant::now();
        let bound = bind(sql, params)?;
        let query_id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            session_id: Some(&self.session_id),
            query_id: Some(&query_id),
            read_only: self.read_only,
            timeout_secs: options.timeout_secs,
            params: bound,
            ..Request::default()
        };
        self.set_running(Some(query_id.clone()));
        let outcome = async {
            let mut rows = Rows::open(self.send_in_session(sql, &request).await?, None).await?;
            let command = command_tag(sql, SqlFlavor::Clickhouse);
            if rows.names.is_empty() {
                while rows.next().await?.is_some() {}
                let insert = analyze(sql, SqlFlavor::Clickhouse).is_some_and(|s| s.kind == StatementKind::Insert);
                return Ok((
                    StatementResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        has_more: false,
                        command,
                        affected: if insert { rows.written } else { None },
                        duration_ms: started.elapsed().as_millis() as u64,
                        notices: Vec::new(),
                    },
                    None,
                ));
            }
            let columns = column_meta(&rows.names, &rows.types);
            let mut open = Cursor { rows, pending: None, query_id: query_id.clone() };
            let (first, has_more) = read_rows(&mut open, options.row_limit).await?;
            let result = StatementResult {
                columns,
                rows: first,
                has_more,
                command,
                affected: None,
                duration_ms: started.elapsed().as_millis() as u64,
                notices: Vec::new(),
            };
            Ok((result, has_more.then_some(open)))
        }
        .await;
        self.set_running(None);
        let (result, open) = outcome?;
        *cursor = open;
        Ok(result)
    }

    /// The session serves one statement at a time; a statement that was just
    /// stopped can hold it for a moment, so retry briefly.
    async fn send_in_session(&self, sql: &str, request: &Request<'_>) -> DbResult<reqwest::Response> {
        let mut delay = std::time::Duration::from_millis(50);
        for _ in 0..8 {
            match self.client.send(sql, request).await {
                Err(e) if e.code.as_deref() == Some("373") => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
        }
        self.client.send(sql, request).await
    }

    /// Stops a result that was not read to the end, so the session is free.
    async fn close_cursor(&self, cursor: &mut Option<Cursor>) {
        if let Some(open) = cursor.take() {
            drop(open.rows);
            let request =
                Request { params: vec![("id".into(), Some(open.query_id))], timeout_secs: 30, ..Request::default() };
            let _ = self.client.text_rows("KILL QUERY WHERE query_id = {id:String} SYNC", &request).await;
        }
    }

    async fn read_batches(
        &self,
        database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        let request =
            Request { database: Some(database), read_only: true, params: bind(sql, params)?, ..Request::default() };
        let mut rows = Rows::open(self.client.send(sql, &request).await?, None).await?;
        let meta = column_meta(&rows.names, &rows.types);
        let mut batch = Vec::with_capacity(STREAM_BATCH);
        let mut total = 0u64;
        while let Some(row) = rows.next().await? {
            batch.push(decode_row(&row, &rows.types, limits));
            if batch.len() == STREAM_BATCH {
                total += batch.len() as u64;
                on_batch(&meta, std::mem::take(&mut batch))?;
            }
        }
        total += batch.len() as u64;
        on_batch(&meta, batch)?;
        Ok(total)
    }
}

#[async_trait]
impl Session for ChSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &ClickhouseDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        match parent {
            None => catalog::databases(&self.client, &self.info.database).await,
            Some(p) if p.kind == NodeKind::Database => catalog::relations(&self.client, &p.name).await,
            Some(p) if p.kind == NodeKind::Table => catalog::table_children(&self.client, p).await,
            Some(p) if p.kind == NodeKind::Group => catalog::group_children(&self.client, p).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        catalog::search(&self.client, query, limit).await
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let options = options.clamped();
        let mut cursor = self.query.lock().await;
        self.close_cursor(&mut cursor).await;
        self.run(&mut cursor, sql, params, options).await
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut cursor = self.query.lock().await;
        let mut open =
            cursor.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let (rows, has_more) = read_rows(&mut open, limit.clamp(1, ExecOptions::MAX_ROW_LIMIT)).await?;
        if has_more {
            *cursor = Some(open);
        }
        Ok(RowsPage { rows, has_more, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        let Some(query_id) = self.running.lock().ok().and_then(|r| r.clone()) else { return Ok(false) };
        let request = Request { params: vec![("id".into(), Some(query_id))], timeout_secs: 30, ..Request::default() };
        self.client.text_rows("KILL QUERY WHERE query_id = {id:String} ASYNC", &request).await?;
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

    /// `ClickHouse` has no schemas: the database is the container, so `schema` is ignored.
    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        catalog::schema_model(&self.client, database).await
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
        catalog::row_estimate(&self.client, object).await
    }

    /// `ClickHouse` has no transactions: statements run one after another,
    /// and those before a failure stay applied.
    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        let mut counts = Vec::new();
        for statement in statements {
            let statement = statement?;
            let request = Request {
                database: Some(database),
                params: bind(&statement.sql, &statement.params)?,
                ..Request::default()
            };
            let mut rows = Rows::open(self.client.send(&statement.sql, &request).await?, None).await?;
            while rows.next().await?.is_some() {}
            let affected = rows.written.unwrap_or(0);
            if statement.expect_rows.is_some() {
                check_affected(&statement, affected)?;
            }
            counts.push(affected);
        }
        Ok(counts)
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
                "ClickHouse has no EXPLAIN ANALYZE. Use Explain, or check system.query_log after running the query.",
            ));
        }
        let sql = format!("EXPLAIN json = 1, indexes = 1, description = 1 {}", sql.trim().trim_end_matches(';'));
        let result = self.execute(&sql, params, ExecOptions { row_limit: 10_000, timeout_secs }).await?;
        let raw = result.rows.iter().filter_map(|r| r.first()).filter_map(Cell::to_text).collect::<Vec<_>>().join("\n");
        // The JSON plan uses PostgreSQL's layout: `[{"Plan": {"Node Type": …, "Plans": […]}}]`.
        ExplainDialect::Postgres.parse(raw, false)
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
    use super::bind;

    #[test]
    fn binds_numbered_and_named_parameters() {
        let generated =
            bind("SELECT * FROM t WHERE b = {p2:String} AND a = {p1:String}", &[Some("1".into()), Some("2".into())])
                .unwrap();
        assert_eq!(generated, [("p2".to_owned(), Some("2".to_owned())), ("p1".to_owned(), Some("1".to_owned()))]);
        let named = bind("SELECT {id:UInt64}, {name:String}, {id:UInt64}", &[Some("7".into()), None]).unwrap();
        assert_eq!(named, [("id".to_owned(), Some("7".to_owned())), ("name".to_owned(), None)]);
        assert!(bind("SELECT {a:UInt8}", &[]).is_err());
    }
}
