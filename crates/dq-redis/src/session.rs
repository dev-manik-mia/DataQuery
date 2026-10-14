//! An open Redis or Valkey connection.
//!
//! The editor and the console share one query connection, so `SELECT`,
//! `MULTI` and blocking commands behave like `redis-cli`. The key browser,
//! monitoring and exports use catalog connections, one per database.
//! Cancel and timeouts end the query connection with `CLIENT KILL` (which
//! also stops blocking commands) and open a new one.

use std::collections::HashMap;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::config::SslMode;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{Dependencies, SchemaModel, TableStructure};
use dq_core::sql::redis::split_args;
use dq_core::sql::{Dialect, RedisDialect, SqlFlavor, split_statements};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use redis::Value;
use redis::aio::MultiplexedConnection;
use tokio::sync::{Mutex, Notify};

use crate::client::{Connector, map_error};
use crate::reply::to_grid;

const STREAM_BATCH: usize = 1_000;

pub struct RedisSession {
    pub(crate) connector: Connector,
    pub(crate) info: ServerInfo,
    pub(crate) read_only: bool,
    pub(crate) databases: u32,
    pub(crate) memory_usage: bool,
    query: Mutex<QueryConn>,
    /// Client id of the query connection, for `CLIENT KILL`.
    query_client: AtomicI64,
    running: AtomicBool,
    cancel: Notify,
    /// Rows of the last editor result not shown yet.
    pending: SyncMutex<Vec<Row>>,
    catalog: SyncMutex<HashMap<u32, MultiplexedConnection>>,
}

pub(crate) struct QueryConn {
    conn: MultiplexedConnection,
    /// The database selected on the query connection.
    pub(crate) db: u32,
}

/// `key:value` lines of an `INFO` reply.
pub(crate) fn info_fields(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

pub(crate) fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

/// Builds a command from split arguments.
pub(crate) fn command(args: &[Vec<u8>]) -> redis::Cmd {
    let mut cmd = redis::Cmd::new();
    for a in args {
        cmd.arg(a.as_slice());
    }
    cmd
}

/// Parses one command line; refuses empty lines.
pub(crate) fn parse_line(line: &str) -> DbResult<Vec<Vec<u8>>> {
    let args = split_args(line.trim())?;
    if args.is_empty() {
        return Err(DbError::invalid("Type a command, e.g. GET key."));
    }
    Ok(args)
}

fn is_server_error(e: &redis::RedisError) -> bool {
    matches!(e.kind(), redis::ErrorKind::Server(_) | redis::ErrorKind::Extension) && e.code().is_some()
}

impl RedisSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let config = &params.config;
        let db = config.database_index().ok_or_else(|| DbError::invalid("The database is a number such as 0."))?;
        let connector = Connector::new(params)?;
        let mut conn = connector.connect(db, None).await?;
        let server: String = redis::cmd("INFO").arg("server").query_async(&mut conn).await.map_err(map_error)?;
        let server = info_fields(&server);
        let valkey = server.contains_key("valkey_version") || server.get("server_name").is_some_and(|n| n == "valkey");
        let version = if valkey {
            format!(
                "Valkey {}",
                server.get("valkey_version").or_else(|| server.get("redis_version")).cloned().unwrap_or_default()
            )
        } else {
            format!("Redis {}", server.get("redis_version").cloned().unwrap_or_default())
        };
        let user: String =
            redis::cmd("ACL").arg("WHOAMI").query_async(&mut conn).await.unwrap_or_else(|_| {
                if config.username.is_empty() { "default".into() } else { config.username.clone() }
            });
        let cluster: String = redis::cmd("INFO").arg("cluster").query_async(&mut conn).await.unwrap_or_default();
        let databases = if info_fields(&cluster).get("cluster_enabled").is_some_and(|v| v == "1") {
            1
        } else {
            let reply: Vec<String> =
                redis::cmd("CONFIG").arg("GET").arg("databases").query_async(&mut conn).await.unwrap_or_default();
            reply.get(1).and_then(|n| n.parse().ok()).unwrap_or(16)
        };
        let memory_usage =
            redis::cmd("MEMORY").arg("USAGE").arg("__dataquery_probe__").query_async::<Value>(&mut conn).await.is_ok();
        let _ = redis::cmd("CLIENT").arg("SETNAME").arg("DataQuery-editor").query_async::<()>(&mut conn).await;
        let client_id: i64 = redis::cmd("CLIENT").arg("ID").query_async(&mut conn).await.unwrap_or(-1);
        let tls = config.ssl_mode != SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version,
            latency_ms: started.elapsed().as_millis() as u64,
            tls: tls.then(|| TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user,
            database: db.to_string(),
            read_only: config.read_only,
        };
        Ok(Self {
            connector,
            read_only: config.read_only,
            info,
            databases,
            memory_usage,
            query: Mutex::new(QueryConn { conn, db }),
            query_client: AtomicI64::new(client_id),
            running: AtomicBool::new(false),
            cancel: Notify::new(),
            pending: SyncMutex::new(Vec::new()),
            catalog: SyncMutex::new(HashMap::new()),
        })
    }

    /// The session's own database (from the connection settings).
    pub(crate) fn home_db(&self) -> u32 {
        self.info.database.parse().unwrap_or(0)
    }

    /// A catalog connection on `database`, opened on first use.
    pub(crate) async fn catalog(&self, database: u32) -> DbResult<MultiplexedConnection> {
        if database >= self.databases {
            return Err(DbError::invalid(format!("This server has databases 0–{}.", self.databases.saturating_sub(1))));
        }
        if let Some(conn) = self.catalog.lock().ok().and_then(|c| c.get(&database).cloned()) {
            return Ok(conn);
        }
        let mut conn = self.connector.catalog(database).await?;
        let _ = redis::cmd("CLIENT").arg("SETNAME").arg("DataQuery").query_async::<()>(&mut conn).await;
        if let Ok(mut c) = self.catalog.lock() {
            c.insert(database, conn.clone());
        }
        Ok(conn)
    }

    /// Runs `cmd` on a catalog connection; a broken connection is replaced next time.
    pub(crate) async fn query_catalog<T: redis::FromRedisValue>(&self, database: u32, cmd: &redis::Cmd) -> DbResult<T> {
        let mut conn = self.catalog(database).await?;
        let result = cmd.query_async(&mut conn).await;
        self.forget_if_broken(database, result.as_ref().err());
        result.map_err(map_error)
    }

    pub(crate) async fn pipe_catalog<T: redis::FromRedisValue>(
        &self,
        database: u32,
        pipe: &redis::Pipeline,
    ) -> DbResult<T> {
        let mut conn = self.catalog(database).await?;
        let result = pipe.query_async(&mut conn).await;
        self.forget_if_broken(database, result.as_ref().err());
        result.map_err(map_error)
    }

    fn forget_if_broken(&self, database: u32, error: Option<&redis::RedisError>) {
        if error.is_some_and(|e| e.is_io_error() || e.is_connection_dropped() || e.is_timeout())
            && let Ok(mut c) = self.catalog.lock()
        {
            c.remove(&database);
        }
    }

    /// Runs one command on the query connection. The outer error is a cancel,
    /// a timeout or a lost connection; the inner one is what the server said.
    pub(crate) async fn run_query(
        &self,
        args: &[Vec<u8>],
        timeout_secs: u32,
    ) -> DbResult<(Result<Value, redis::RedisError>, u32)> {
        let mut q = self.query.lock().await;
        let cancelled = self.cancel.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        self.running.store(true, Ordering::SeqCst);
        let cmd = command(args);
        let limit = async {
            if timeout_secs == 0 {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep(Duration::from_secs(u64::from(timeout_secs))).await;
            }
        };
        let outcome = tokio::select! {
            r = cmd.query_async::<Value>(&mut q.conn) => Ok(r),
            () = &mut cancelled => Err(DbError::new(ErrorKind::Cancelled, "The command was cancelled.")),
            () = limit => Err(DbError::new(
                ErrorKind::Timeout,
                format!("The command ran longer than {timeout_secs} s and was stopped."),
            )),
        };
        self.running.store(false, Ordering::SeqCst);
        match outcome {
            Err(e) => {
                // The server may still be serving the command: end that connection.
                self.kill_query_client().await;
                self.reconnect_query(&mut q).await?;
                Err(e)
            }
            Ok(Err(e)) if !is_server_error(&e) => {
                self.reconnect_query(&mut q).await?;
                Err(map_error(e))
            }
            Ok(reply) => {
                if reply.is_ok()
                    && args[0].eq_ignore_ascii_case(b"SELECT")
                    && let Some(db) = args.get(1).and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| d.parse().ok())
                {
                    q.db = db;
                }
                Ok((reply, q.db))
            }
        }
    }

    async fn kill_query_client(&self) {
        let id = self.query_client.load(Ordering::SeqCst);
        if id < 0 {
            return;
        }
        let mut kill = redis::cmd("CLIENT");
        kill.arg("KILL").arg("ID").arg(id);
        let _ = self.query_catalog::<Value>(self.home_db(), &kill).await;
    }

    /// A fresh query connection on the same database (a `MULTI` in progress is lost).
    async fn reconnect_query(&self, q: &mut QueryConn) -> DbResult<()> {
        let mut conn = self.connector.connect(q.db, None).await?;
        let _ = redis::cmd("CLIENT").arg("SETNAME").arg("DataQuery-editor").query_async::<()>(&mut conn).await;
        let id: i64 = redis::cmd("CLIENT").arg("ID").query_async(&mut conn).await.unwrap_or(-1);
        self.query_client.store(id, Ordering::SeqCst);
        q.conn = conn;
        Ok(())
    }

    /// Refuses writing commands on read-only connections.
    pub(crate) fn guard(&self, line: &str) -> DbResult<()> {
        if self.read_only && split_statements(line, SqlFlavor::Redis).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        Ok(())
    }

    fn database_arg(&self, database: &str) -> u32 {
        database.trim().parse().unwrap_or_else(|_| self.home_db())
    }

    /// Runs a command on a catalog connection and streams its grid rows.
    async fn read_batches(
        &self,
        database: &str,
        sql: &str,
        limits: CellLimits,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        self.guard(sql)?;
        let args = parse_line(sql)?;
        let value: Value = self.query_catalog(self.database_arg(database), &command(&args)).await?;
        let (columns, rows) = to_grid(&args, &value, limits);
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
}

#[async_trait]
impl Session for RedisSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &RedisDialect
    }

    /// Databases `0…n-1` with their key counts; keys are browsed with `SCAN`, not listed here.
    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        if parent.is_some() {
            return Ok(Vec::new());
        }
        let keyspace: String = self.query_catalog(self.home_db(), redis::cmd("INFO").arg("keyspace")).await?;
        let keyspace = info_fields(&keyspace);
        Ok((0..self.databases)
            .map(|db| {
                let keys = keyspace
                    .get(&format!("db{db}"))
                    .and_then(|v| v.split(',').find_map(|p| p.strip_prefix("keys=")))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let mut node = Node::leaf(ObjectRef::new(NodeKind::Database, db.to_string(), None, db.to_string()));
                node.row_estimate = Some(keys);
                node.current = db == self.home_db();
                node
            })
            .collect())
    }

    /// Keys of the session's database whose name contains `query` (a bounded `SCAN`).
    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let db = self.home_db();
        let mut pattern = String::from("*");
        for c in query.chars() {
            if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        pattern.push('*');
        let mut cursor = "0".to_owned();
        let mut out = Vec::new();
        for _ in 0..10 {
            let (next, keys): (String, Vec<Vec<u8>>) = self
                .query_catalog(db, redis::cmd("SCAN").arg(&cursor).arg("MATCH").arg(&pattern).arg("COUNT").arg(1000))
                .await?;
            for key in keys {
                if let Ok(name) = String::from_utf8(key) {
                    out.push(Node::leaf(ObjectRef::new(NodeKind::Key, db.to_string(), None, name)));
                    if out.len() >= limit as usize {
                        return Ok(out);
                    }
                }
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        Ok(out)
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let started = Instant::now();
        let options = options.clamped();
        if !params.is_empty() {
            return Err(DbError::invalid("Redis commands take no parameters; write the values in the command."));
        }
        self.guard(sql)?;
        let args = parse_line(sql)?;
        if let Ok(mut p) = self.pending.lock() {
            p.clear();
        }
        let (reply, _) = self.run_query(&args, options.timeout_secs).await?;
        let value = reply.map_err(map_error)?;
        let (columns, mut rows) = to_grid(&args, &value, CellLimits::GRID);
        let has_more = rows.len() > options.row_limit as usize;
        if has_more {
            let rest = rows.split_off(options.row_limit as usize);
            if let Ok(mut p) = self.pending.lock() {
                *p = rest.into_iter().take(ExecOptions::MAX_ROW_LIMIT as usize).collect();
            }
        }
        let writes = split_statements(sql, SqlFlavor::Redis).iter().any(|s| s.writes);
        let affected = match value {
            Value::Int(n) if writes && n >= 0 => Some(n as u64),
            _ => None,
        };
        Ok(StatementResult {
            columns,
            rows,
            has_more,
            command: String::from_utf8_lossy(&args[0]).to_ascii_uppercase(),
            affected,
            duration_ms: started.elapsed().as_millis() as u64,
            notices: Vec::new(),
        })
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut pending = self.pending.lock().map_err(|_| DbError::internal("pending rows lock"))?;
        if pending.is_empty() {
            return Err(DbError::invalid("No more rows: the result was closed by a newer command."));
        }
        let take = (limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize).min(pending.len());
        let rows: Vec<Row> = pending.drain(..take).collect();
        Ok(RowsPage { rows, has_more: !pending.is_empty(), duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.cancel.notify_waiters();
        Ok(true)
    }

    async fn structure(&self, _object: &ObjectRef) -> DbResult<TableStructure> {
        Err(DbError::unsupported("Redis keys have no table structure. Open the key in the key browser."))
    }

    async fn ddl(&self, _object: &ObjectRef) -> DbResult<String> {
        Err(DbError::unsupported("Redis keys have no definition."))
    }

    async fn dependencies(&self, _object: &ObjectRef) -> DbResult<Dependencies> {
        Ok(Dependencies { uses: Vec::new(), used_by: Vec::new() })
    }

    async fn schema_model(&self, _database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        Ok(SchemaModel { tables: Vec::new(), foreign_keys: Vec::new(), truncated: false })
    }

    async fn read_query(
        &self,
        database: &str,
        sql: &str,
        _params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        self.read_batches(database, sql, limits, &mut |meta, batch| {
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
        _params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        self.read_batches(database, sql, CellLimits::FULL, on_batch).await
    }

    async fn row_estimate(&self, _object: &ObjectRef) -> DbResult<Option<i64>> {
        Ok(None)
    }

    /// Runs the commands in one `MULTI`/`EXEC` on a catalog connection.
    async fn run_in_transaction(&self, database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        if self.read_only {
            return Err(read_only_error());
        }
        let mut pipe = redis::pipe();
        pipe.atomic();
        let mut bound = Vec::new();
        for statement in statements {
            let statement = statement?;
            pipe.add_command(command(&parse_line(&statement.sql)?));
            bound.push(statement);
        }
        if bound.is_empty() {
            return Ok(Vec::new());
        }
        // Atomic pipelines need their own connection: WATCH/MULTI state is per connection.
        let mut conn = self.connector.catalog(self.database_arg(database)).await?;
        let replies: Vec<Value> = pipe.query_async(&mut conn).await.map_err(map_error)?;
        let mut counts = Vec::with_capacity(replies.len());
        for (statement, reply) in bound.iter().zip(&replies) {
            let n = match reply {
                Value::Int(n) if *n >= 0 => *n as u64,
                _ => 1,
            };
            check_affected(statement, n)?;
            counts.push(n);
        }
        Ok(counts)
    }

    fn monitor(&self) -> Option<&dyn dq_core::monitor::Monitor> {
        Some(self)
    }

    fn keys(&self) -> Option<&dyn dq_core::keyvalue::KeyValueStore> {
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
    fn info_lines() {
        let f = info_fields("# Server\r\nredis_version:8.0.2\r\nvalkey_version:8.1.0\r\n");
        assert_eq!(f.get("redis_version").map(String::as_str), Some("8.0.2"));
        assert_eq!(f.get("valkey_version").map(String::as_str), Some("8.1.0"));
    }

    #[test]
    fn lines_must_hold_a_command() {
        assert!(parse_line("  ").is_err());
        assert_eq!(parse_line("GET a").unwrap().len(), 2);
    }
}
