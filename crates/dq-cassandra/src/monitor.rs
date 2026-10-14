//! Monitoring from system tables: nodes (`system.local`, `system.peers`),
//! client connections (`system_views.clients` on Cassandra 4+, `system.clients`
//! on `ScyllaDB`), settings and size estimates. Cassandra cannot end another
//! client's requests, so there is no kill.

use std::hash::{Hash, Hasher};

use async_trait::async_trait;
use dq_core::monitor::{Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};

use crate::client::text_rows;
use crate::session::CqlSession;

/// A stable number for a client connection (`address:port`), for the process list.
fn pid(address: &str) -> i64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    address.hash(&mut h);
    (h.finish() >> 1) as i64
}

#[async_trait]
impl Monitor for CqlSession {
    async fn snapshot(&self, _long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let mut unavailable = Vec::new();
        let peers = text_rows(&self.driver, "SELECT peer FROM system.peers", ()).await;
        let replication = match peers {
            Ok(p) => Some(Replication {
                role: "node".into(),
                lag_secs: None,
                replicas: p.len() as i64,
                state: Some(format!("{} node(s) in the cluster", p.len() + 1)),
            }),
            Err(e) => {
                unavailable.push(format!("Cluster nodes: {}", e.message));
                None
            }
        };
        let (sql, columns) = if self.scylla {
            ("SELECT address, port, username, connection_stage, client_type FROM system.clients", "system.clients")
        } else {
            (
                "SELECT address, port, username, connection_stage, driver_name, keyspace_name, request_count FROM system_views.clients",
                "system_views.clients",
            )
        };
        let processes = match text_rows(&self.driver, sql, ()).await {
            Ok(rows) => rows
                .iter()
                .map(|r| {
                    let client = format!("{}:{}", r.string(0), r.string(1));
                    ServerProcess {
                        pid: pid(&client),
                        user: r.text(2).filter(|u| !u.is_empty()),
                        database: if self.scylla { None } else { r.text(5) },
                        client: Some(client),
                        state: r.text(3),
                        duration_ms: None,
                        query: r.text(4).map(|d| if self.scylla { d } else { format!("{d} · {} requests", r.string(6)) }),
                        wait: None,
                        blocked_by: Vec::new(),
                        active: false,
                    }
                })
                .collect(),
            Err(e) => {
                unavailable.push(format!("Client connections ({columns}): {}", e.message));
                Vec::new()
            }
        };
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections: None,
            active_queries: None,
            long_running: None,
            blocked: None,
            replication,
            processes,
            locks: Vec::new(),
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let sql = if self.scylla { "SELECT name, value, source FROM system.config" } else { "SELECT name, value FROM system_views.settings" };
        let needle = search.unwrap_or_default().to_lowercase();
        let rows = text_rows(&self.driver, sql, ())
            .await
            .map_err(|e| DbError::unsupported(format!("Server settings are not available: {}", e.message)))?;
        Ok(rows
            .iter()
            .filter(|r| r.string(0).to_lowercase().contains(&needle))
            .map(|r| ServerSetting {
                name: r.string(0),
                value: r.string(1),
                unit: None,
                category: if self.scylla { r.text(2) } else { None },
                description: None,
            })
            .collect())
    }

    /// Estimated size per table from `system.size_estimates` (partitions × mean partition size).
    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let rows = text_rows(
            &self.driver,
            "SELECT table_name, partitions_count, mean_partition_size FROM system.size_estimates WHERE keyspace_name = ?",
            (database,),
        )
        .await?;
        let mut sizes: std::collections::BTreeMap<String, (i64, i64)> = std::collections::BTreeMap::new();
        for r in rows {
            let e = sizes.entry(r.string(0)).or_default();
            let partitions = r.number(1).unwrap_or(0);
            e.0 += partitions;
            e.1 += partitions.saturating_mul(r.number(2).unwrap_or(0));
        }
        let mut out: Vec<ObjectSize> = sizes
            .into_iter()
            .map(|(table, (partitions, bytes))| ObjectSize {
                object: ObjectRef::new(NodeKind::Table, database, None, table),
                total_bytes: bytes,
                table_bytes: bytes,
                index_bytes: 0,
                rows: Some(partitions),
            })
            .collect();
        out.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn kill(&self, _pid: i64, _terminate: bool) -> DbResult<bool> {
        Err(DbError::unsupported("Cassandra cannot end another client's requests."))
    }
}

use dq_core::adapter::Session as _;

#[cfg(test)]
mod tests {
    #[test]
    fn pids_are_stable() {
        assert_eq!(super::pid("10.0.0.1:5000"), super::pid("10.0.0.1:5000"));
        assert!(super::pid("10.0.0.1:5000") >= 0);
    }
}
