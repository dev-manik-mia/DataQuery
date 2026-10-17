//! Monitoring for an embedded database. `DuckDB` runs inside DataQuery, so the
//! only "process" is the editor's query connection: its running statement comes
//! from the worker, and cancelling it interrupts that connection. Settings come
//! from `duckdb_settings()`, sizes from `duckdb_tables()` and the storage blocks
//! of each table.

use async_trait::async_trait;
use dq_core::adapter::Session as _;
use dq_core::monitor::{Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::sql::{Dialect, DuckdbDialect};
use dq_core::{DbError, DbResult};

use crate::catalog::rows;
use crate::session::{DuckSession, blocking};

/// The pid shown for the editor's query connection.
const QUERY_PID: i64 = 1;

#[async_trait]
impl Monitor for DuckSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let running = self.interrupter.current();
        let long_running = running.as_ref().is_some_and(|(_, d)| d.as_secs() >= u64::from(long_running_secs));
        let process = ServerProcess {
            pid: QUERY_PID,
            user: Some("Query editor".into()),
            database: Some(self.info().database.clone()),
            client: Some("DataQuery (embedded)".into()),
            state: Some(if running.is_some() { "active" } else { "idle" }.into()),
            duration_ms: running.as_ref().map(|(_, d)| d.as_millis() as i64),
            active: running.is_some(),
            query: short_query(running.map(|(sql, _)| sql)),
            wait: None,
            blocked_by: Vec::new(),
        };
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections: None,
            active_queries: Some(i64::from(process.active)),
            long_running: Some(i64::from(long_running)),
            blocked: None,
            replication: None,
            processes: vec![process],
            locks: Vec::new(),
            unavailable: Vec::new(),
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let needle = format!("%{}%", search.unwrap_or_default());
        blocking(&self.catalog, move |conn| {
            let found = rows(
                conn,
                "SELECT name, value, description, scope FROM duckdb_settings() \
                 WHERE name ILIKE ? ORDER BY name",
                &[&needle],
            )?;
            Ok(found
                .iter()
                .map(|r| ServerSetting {
                    name: r.string(0),
                    value: r.string(1),
                    unit: None,
                    category: r.text(3),
                    description: r.text(2),
                })
                .collect())
        })
        .await
    }

    /// Row estimates from `duckdb_tables()`; bytes are the table's storage
    /// blocks, so in-memory databases report rows only.
    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = if database.is_empty() { self.info().database.clone() } else { database.to_owned() };
        blocking(&self.catalog, move |conn| {
            let block_size = rows(
                conn,
                "SELECT block_size::VARCHAR FROM pragma_database_size() WHERE database_name = ?",
                &[&database],
            )?
            .first()
            .and_then(|r| r.number(0))
            .unwrap_or(0);
            let tables = rows(
                conn,
                "SELECT schema_name, table_name, estimated_size::VARCHAR FROM duckdb_tables() \
                 WHERE database_name = ? AND NOT temporary ORDER BY estimated_size DESC, table_name LIMIT ?",
                &[&database, &i64::from(limit)],
            )?;
            let mut out = Vec::with_capacity(tables.len());
            for t in &tables {
                let object = ObjectRef::new(NodeKind::Table, &database, Some(t.string(0)), t.string(1));
                let name = DuckdbDialect.quote_literal(&DuckdbDialect.quoted_name(&object));
                let blocks = rows(
                    conn,
                    &format!(
                        "SELECT count(DISTINCT block_id) FILTER (WHERE persistent)::VARCHAR FROM pragma_storage_info({name})"
                    ),
                    &[],
                )?
                .first()
                .and_then(|r| r.number(0))
                .unwrap_or(0);
                let bytes = blocks.saturating_mul(block_size);
                out.push(ObjectSize { object, total_bytes: bytes, table_bytes: bytes, index_bytes: 0, rows: t.number(2) });
            }
            out.sort_by_key(|o| std::cmp::Reverse((o.total_bytes, o.rows)));
            Ok(out)
        })
        .await
    }

    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        if pid != QUERY_PID {
            return Err(DbError::invalid(format!("There is no process {pid}.")));
        }
        if terminate {
            return Err(DbError::unsupported(
                "DuckDB runs inside DataQuery, so there is no connection to end. Cancel the statement instead.",
            ));
        }
        Ok(self.interrupter.cancel())
    }
}
