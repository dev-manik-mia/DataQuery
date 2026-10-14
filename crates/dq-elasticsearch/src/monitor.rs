//! Monitoring: running tasks (`_tasks`), HTTP connections, cluster health,
//! settings and the largest indices. Task ids (`node:number`) map to process
//! ids by their number; the full id is kept so a task can be cancelled.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::monitor::{Gauge, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query};
use dq_core::node::{NodeKind, ObjectRef};
use serde_json::Value;

use crate::catalog;
use crate::client::Call;
use crate::session::EsSession;

/// Task actions shown as processes.
const ACTIONS: &str = "*search*,*bulk*,*reindex*,*by_query*,*msearch*";

/// Last seen task ids by process id.
#[derive(Default)]
pub struct Tasks(Mutex<HashMap<i64, String>>);

impl Tasks {
    fn remember(&self, pid: i64, id: &str) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(pid, id.to_owned());
        }
    }

    fn get(&self, pid: i64) -> Option<String> {
        self.0.lock().ok().and_then(|m| m.get(&pid).cloned())
    }
}

/// The number of `node:123` (stable within a node; unique enough for one cluster view).
pub fn pid_of(task_id: &str) -> i64 {
    task_id.rsplit(':').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

pub fn processes(tasks: &Value, long_running_secs: u32) -> (Vec<ServerProcess>, Vec<(i64, String)>, i64) {
    let mut out = Vec::new();
    let mut ids = Vec::new();
    let mut long = 0;
    for (node_id, node) in tasks["nodes"].as_object().into_iter().flatten() {
        let node_name = node["name"].as_str().unwrap_or(node_id).to_owned();
        for (task_id, task) in node["tasks"].as_object().into_iter().flatten() {
            let pid = pid_of(task_id);
            ids.push((pid, task_id.clone()));
            let duration_ms = task["running_time_in_nanos"].as_i64().map(|n| n / 1_000_000);
            if duration_ms.is_some_and(|d| d >= i64::from(long_running_secs) * 1000) {
                long += 1;
            }
            let description = task["description"].as_str().map(str::to_owned);
            let index = description
                .as_deref()
                .and_then(|d| d.split("indices[").nth(1))
                .and_then(|r| r.split(']').next())
                .map(str::to_owned);
            out.push(ServerProcess {
                pid,
                user: task["headers"]["X-Opaque-Id"].as_str().map(str::to_owned),
                database: index,
                client: Some(node_name.clone()),
                state: task["action"].as_str().map(str::to_owned),
                duration_ms,
                query: short_query(description),
                wait: None,
                blocked_by: Vec::new(),
                active: true,
            });
        }
    }
    (out, ids, long)
}

#[async_trait]
impl Monitor for EsSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let client = &self.client;
        let mut unavailable = Vec::new();
        let (tasks, http, health) = tokio::join!(
            client.get(&format!("/_tasks?detailed=true&actions={ACTIONS}")),
            client.get("/_nodes/stats/http"),
            client.get("/_cluster/health"),
        );
        let (processes, active, long_running) = match tasks {
            Ok(t) => {
                let (p, ids, long) = processes(&t, long_running_secs);
                for (pid, id) in ids {
                    self.tasks.remember(pid, &id);
                }
                let n = p.len() as i64;
                (p, Some(n), Some(long))
            }
            Err(e) => {
                unavailable.push(format!("Tasks: {}", e.message));
                (Vec::new(), None, None)
            }
        };
        let connections = match http {
            Ok(h) => {
                let open: i64 = h["nodes"].as_object().into_iter().flatten().filter_map(|(_, n)| n["http"]["current_open"].as_i64()).sum();
                Some(Gauge { used: open, max: 0 })
            }
            Err(e) => {
                unavailable.push(format!("Connections: {}", e.message));
                None
            }
        };
        let replication = match health {
            Ok(h) => Some(Replication {
                role: "cluster".into(),
                lag_secs: None,
                replicas: h["number_of_nodes"].as_i64().map_or(0, |n| (n - 1).max(0)),
                state: h["status"].as_str().map(str::to_owned),
            }),
            Err(e) => {
                unavailable.push(format!("Cluster health: {}", e.message));
                None
            }
        };
        Ok(MonitorSnapshot {
            version: self.info.version.clone(),
            connections,
            active_queries: active,
            long_running,
            blocked: None,
            replication,
            processes,
            locks: Vec::new(),
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let all = self.client.get("/_cluster/settings?include_defaults=true&flat_settings=true").await?;
        let needle = search.map(str::to_lowercase).unwrap_or_default();
        let mut seen = HashMap::new();
        // Transient settings override persistent ones, which override defaults.
        for section in ["transient", "persistent", "defaults"] {
            for (name, value) in all[section].as_object().into_iter().flatten() {
                if !name.to_lowercase().contains(&needle) || seen.contains_key(name) {
                    continue;
                }
                let value = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                seen.insert(name.clone(), ServerSetting {
                    name: name.clone(),
                    value,
                    unit: None,
                    category: Some(section.to_owned()),
                    description: None,
                });
            }
        }
        let mut out: Vec<ServerSetting> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.truncate(2_000);
        Ok(out)
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let mut rows = catalog::indices(&self.client, "*").await?;
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes.unwrap_or(0)));
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|r| ObjectSize {
                object: ObjectRef::new(NodeKind::Table, database, None, r.name),
                total_bytes: r.bytes.unwrap_or(0),
                table_bytes: r.bytes.unwrap_or(0),
                index_bytes: 0,
                rows: r.docs,
            })
            .collect())
    }

    /// Cancels the task (Elasticsearch cannot end a client's connection; terminate is the same).
    async fn kill(&self, pid: i64, _terminate: bool) -> DbResult<bool> {
        let Some(id) = self.tasks.get(pid) else { return Ok(false) };
        let path = format!("/_tasks/{}/_cancel", catalog::segment(&id));
        let response = match self.client.call(&Call { method: "POST", path: &path, ..Call::default() }).await {
            Ok(body) => body.json().clone(),
            Err(e) if e.kind == dq_core::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let cancelled = response["nodes"].as_object().is_some_and(|n| !n.is_empty());
        Ok(cancelled && response["node_failures"].as_array().is_none_or(Vec::is_empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tasks_become_processes() {
        let tasks = json!({"nodes": {"n1": {"name": "es-1", "tasks": {"n1:42": {
            "action": "indices:data/read/search", "running_time_in_nanos": 7_000_000_000_i64,
            "description": "indices[orders], search_type[QUERY_THEN_FETCH]", "headers": {"X-Opaque-Id": "dataquery-1"}}}}}});
        let (p, ids, long) = processes(&tasks, 5);
        assert_eq!((p[0].pid, p[0].database.as_deref(), p[0].duration_ms), (42, Some("orders"), Some(7000)));
        assert_eq!(p[0].user.as_deref(), Some("dataquery-1"));
        assert_eq!(ids, vec![(42, "n1:42".to_owned())]);
        assert_eq!(long, 1);
    }
}
