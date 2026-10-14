//! Monitoring from `SHOW TRANSACTIONS` and `SHOW SETTINGS`; `TERMINATE TRANSACTION` to stop one.

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::monitor::{Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query};
use dq_core::node::{NodeKind, ObjectRef};
use serde_json::{Map, Value};

use crate::catalog;
use crate::client::Query;
use crate::session::Neo4jSession;
use crate::values::{duration_ms, text};

/// The numeric part of `neo4j-transaction-123`.
pub fn pid_of(transaction_id: &str) -> Option<i64> {
    transaction_id.rsplit('-').next()?.parse().ok()
}

#[async_trait]
impl Monitor for Neo4jSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let database = self.current_database();
        let reply = self
            .read(
                &database,
                "SHOW TRANSACTIONS YIELD transactionId, username, database, clientAddress, status, elapsedTime, \
                 currentQuery, waitTime, activeLockCount",
                Map::new(),
            )
            .await?;
        let mut processes = Vec::new();
        let mut ids = std::collections::HashMap::new();
        for row in &reply.values {
            let col = |i: usize| row.get(i).and_then(text);
            let Some(id) = col(0) else { continue };
            let Some(pid) = pid_of(&id) else { continue };
            let query = col(6).filter(|q| !q.is_empty());
            // Leave out this snapshot's own statement.
            if query.as_deref().is_some_and(|q| q.starts_with("SHOW TRANSACTIONS YIELD transactionId, username")) {
                continue;
            }
            ids.insert(pid, id);
            let status = col(4);
            let locks = col(8).filter(|l| l != "0").map(|l| format!("{l} locks"));
            let wait = col(7).and_then(|w| duration_ms(&w)).filter(|ms| *ms > 0).map(|ms| format!("waiting {ms} ms"));
            processes.push(ServerProcess {
                pid,
                user: col(1),
                database: col(2),
                client: col(3),
                active: status.as_deref() == Some("Running") && query.is_some(),
                state: status,
                duration_ms: col(5).and_then(|d| duration_ms(&d)),
                query: short_query(query),
                wait: [wait, locks].into_iter().flatten().reduce(|a, b| format!("{a} · {b}")),
                blocked_by: Vec::new(),
            });
        }
        *self.monitored.lock().expect("monitored lock") = ids;
        let active = processes.iter().filter(|p| p.active).count() as i64;
        let long = processes
            .iter()
            .filter(|p| p.active && p.duration_ms.is_some_and(|d| d >= i64::from(long_running_secs) * 1000))
            .count() as i64;
        let blocked =
            processes.iter().filter(|p| p.wait.as_deref().is_some_and(|w| w.starts_with("waiting"))).count() as i64;
        Ok(MonitorSnapshot {
            version: self.info_version(),
            connections: None,
            active_queries: Some(active),
            long_running: Some(long),
            blocked: Some(blocked),
            replication: None,
            processes,
            locks: Vec::new(),
            unavailable: vec!["Connection counts are not available through the Query API.".to_owned()],
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let mut params = Map::new();
        params.insert(
            "s".into(),
            crate::values::param(Some(&format!("\"{}\"", search.unwrap_or("").to_lowercase().replace('"', "")))),
        );
        let reply = self
            .read(
                &self.current_database(),
                "SHOW SETTINGS YIELD name, value, description WHERE toLower(name) CONTAINS $s RETURN name, value, description ORDER BY name",
                params,
            )
            .await?;
        Ok(reply
            .values
            .iter()
            .map(|r| {
                let name = r.first().and_then(text).unwrap_or_default();
                ServerSetting {
                    category: name.split('.').next().map(str::to_owned),
                    value: r.get(1).and_then(text).unwrap_or_default(),
                    description: r.get(2).and_then(text),
                    unit: None,
                    name,
                }
            })
            .collect())
    }

    /// Labels by node count (from the count store); the Query API reports no sizes.
    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let mut sizes = Vec::new();
        for label in catalog::labels(&self.client, database).await? {
            let object = ObjectRef::new(NodeKind::Table, database, None, label);
            let rows = catalog::count(&self.client, &object).await.ok().flatten();
            sizes.push(ObjectSize { object, total_bytes: 0, table_bytes: 0, index_bytes: 0, rows });
        }
        sizes.sort_by_key(|s| std::cmp::Reverse(s.rows.unwrap_or(0)));
        sizes.truncate(limit as usize);
        Ok(sizes)
    }

    /// Terminates the transaction (cancel and terminate are the same in Neo4j).
    async fn kill(&self, pid: i64, _terminate: bool) -> DbResult<bool> {
        let database = self.current_database();
        let id = self
            .monitored
            .lock()
            .expect("monitored lock")
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| format!("{database}-transaction-{pid}"));
        let query = Query::write(&database, "TERMINATE TRANSACTION $id YIELD message RETURN message")
            .with("id", crate::values::param(Some(&Value::String(id).to_string())));
        let reply = self.client.run(&query, false).await?;
        let message = reply.values.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();
        Ok(message.to_lowercase().contains("terminated"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pids() {
        assert_eq!(super::pid_of("neo4j-transaction-123"), Some(123));
        assert_eq!(super::pid_of("my-db-transaction-7"), Some(7));
        assert_eq!(super::pid_of("x"), None);
    }
}
