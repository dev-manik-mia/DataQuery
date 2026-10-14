//! Monitoring from `system.processes`, `system.metrics`, the settings tables
//! and `system.parts`. Query ids are strings; the process id shown is a hash
//! of the id that `kill` maps back.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::monitor::{Gauge, Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};

use crate::client::Request;
use crate::session::ChSession;

/// A JavaScript-safe integer for a query id.
const PID: &str = "cityHash64(query_id) % 9007199254740991";

fn request(pairs: Vec<(String, Option<String>)>) -> Request<'static> {
    Request { read_only: true, timeout_secs: 30, params: pairs, ..Request::default() }
}

#[async_trait]
impl Monitor for ChSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let rows = self
            .client
            .text_rows(
                &format!(
                    "SELECT toString({PID}), user, current_database, toString(address), toString(round(elapsed * 1000)), query, \
                            toString(is_cancelled) FROM system.processes WHERE query_id <> queryID() \
                     ORDER BY elapsed DESC LIMIT 500"
                ),
                &request(Vec::new()),
            )
            .await?;
        let field = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
        let processes: Vec<ServerProcess> = rows
            .iter()
            .map(|r| ServerProcess {
                pid: field(r, 0).and_then(|v| v.parse().ok()).unwrap_or_default(),
                user: field(r, 1),
                database: field(r, 2),
                client: field(r, 3),
                state: Some(if field(r, 6).as_deref() == Some("1") { "cancelling" } else { "running" }.into()),
                duration_ms: field(r, 4).and_then(|v| v.parse().ok()),
                query: short_query(field(r, 5)),
                wait: None,
                blocked_by: Vec::new(),
                active: true,
            })
            .collect();
        let gauge = self
            .client
            .text_rows(
                "SELECT toString((SELECT sum(value) FROM system.metrics WHERE metric IN ('TCPConnection', 'HTTPConnection', 'MySQLConnection', 'PostgreSQLConnection'))), \
                        toString((SELECT value FROM system.server_settings WHERE name = 'max_connections'))",
                &request(Vec::new()),
            )
            .await
            .ok()
            .and_then(|r| r.into_iter().next());
        let connections =
            gauge.and_then(|g| Some(Gauge { used: field(&g, 0)?.parse().ok()?, max: field(&g, 1)?.parse().ok()? }));
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections,
            active_queries: Some(processes.len() as i64),
            long_running: Some(processes.iter().filter(|p| p.duration_ms.unwrap_or(0) > long_ms).count() as i64),
            blocked: None,
            replication: None,
            processes,
            locks: Vec::new(),
            unavailable: vec!["ClickHouse takes no row locks; there is no lock view.".into()],
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let pattern = dq_core::sql::builder::like_contains(search.unwrap_or_default());
        let rows = self
            .client
            .text_rows(
                "SELECT name, value, description, 'Server' FROM system.server_settings WHERE name ILIKE {p:String} \
                 UNION ALL SELECT name, value, description, 'Session' FROM system.settings WHERE name ILIKE {p:String} \
                 ORDER BY 1 LIMIT 1000",
                &request(vec![("p".into(), Some(pattern))]),
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ServerSetting {
                name: r.first().cloned().flatten().unwrap_or_default(),
                value: r.get(1).cloned().flatten().unwrap_or_default(),
                unit: None,
                category: r.get(3).cloned().flatten(),
                description: r.get(2).cloned().flatten().filter(|d| !d.is_empty()),
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = if database.is_empty() { self.info().database.clone() } else { database.to_owned() };
        let rows = self
            .client
            .text_rows(
                &format!(
                    "SELECT table, toString(sum(bytes_on_disk)), toString(sum(data_compressed_bytes)), \
                            toString(sum(primary_key_bytes_in_memory)), toString(sum(rows)) \
                     FROM system.parts WHERE active AND database = {{db:String}} GROUP BY table ORDER BY sum(bytes_on_disk) DESC LIMIT {}",
                    limit.clamp(1, 1000)
                ),
                &request(vec![("db".into(), Some(database.clone()))]),
            )
            .await?;
        let num = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().and_then(|v| v.parse().ok());
        Ok(rows
            .iter()
            .map(|r| ObjectSize {
                object: ObjectRef::new(
                    NodeKind::Table,
                    database.clone(),
                    None,
                    r.first().cloned().flatten().unwrap_or_default(),
                ),
                total_bytes: num(r, 1).unwrap_or(0),
                table_bytes: num(r, 2).unwrap_or(0),
                index_bytes: num(r, 3).unwrap_or(0),
                rows: num(r, 4),
            })
            .collect())
    }

    /// `KILL QUERY` stops a query; a `ClickHouse` HTTP request has no session to end.
    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        let _ = terminate;
        let found = self
            .client
            .text_rows(
                &format!("SELECT query_id FROM system.processes WHERE {PID} = {{pid:UInt64}}"),
                &request(vec![("pid".into(), Some(pid.to_string()))]),
            )
            .await?;
        let query_id = found
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .ok_or_else(|| DbError::not_found("The query has already finished."))?;
        let kill = Request { timeout_secs: 30, params: vec![("id".into(), Some(query_id))], ..Request::default() };
        self.client.text_rows("KILL QUERY WHERE query_id = {id:String} ASYNC", &kill).await?;
        Ok(true)
    }
}
