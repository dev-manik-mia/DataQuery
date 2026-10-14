//! Monitoring from `serverStatus`, `$currentOp`, `hello` /
//! `replSetGetStatus`, `getParameter` and `$collStats`. Operations are
//! stopped with `killOp`; a client connection cannot be ended from here.

use std::hash::{Hash, Hasher};

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::monitor::{
    Gauge, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::NodeKind;
use dq_core::{DbError, DbResult};
use futures_util::{StreamExt, stream};
use mongodb::bson::{Bson, Document, doc};

use crate::catalog;
use crate::client::int_field;
use crate::literal::compact;
use crate::session::{MongoSession, command, documents};

const COMMENT: &str = "dataquery-monitor";

/// A JavaScript-safe process id: the opid, or a hash of `shard:opid`.
fn pid_of(opid: &Bson) -> i64 {
    match opid {
        Bson::Int32(v) => i64::from(*v),
        Bson::Int64(v) => *v,
        other => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            compact(other).hash(&mut h);
            (h.finish() % 9_007_199_254_740_991) as i64
        }
    }
}

impl MongoSession {
    /// Operations of all users when allowed, else this user's own.
    async fn current_ops(&self) -> DbResult<(Vec<Document>, Option<String>)> {
        let pipeline = |all: bool| {
            doc! {
                "aggregate": 1,
                "pipeline": [
                    { "$currentOp": { "allUsers": all, "idleConnections": false } },
                    { "$match": { "command.comment": { "$ne": COMMENT } } },
                ],
                "cursor": {},
                "comment": COMMENT,
            }
        };
        if let Ok(ops) = documents(&self.client, "admin", pipeline(true), 500).await {
            return Ok((ops, None));
        }
        let ops = documents(&self.client, "admin", pipeline(false), 500).await?;
        Ok((ops, Some("Only this user's operations are shown (the account lacks the inprog privilege).".into())))
    }

    async fn replication(&self) -> Option<Replication> {
        let hello = command(&self.client, "admin", doc! { "hello": 1 }, None).await.ok()?;
        hello.get_str("setName").ok()?;
        let role = if hello.get_bool("isWritablePrimary").unwrap_or(false) { "primary" } else { "replica" };
        let status = command(&self.client, "admin", doc! { "replSetGetStatus": 1 }, None).await.ok();
        let members: Vec<Document> = status
            .as_ref()
            .and_then(|s| s.get_array("members").ok())
            .map(|m| m.iter().filter_map(|m| m.as_document().cloned()).collect())
            .unwrap_or_default();
        let optime = |m: &Document| m.get_datetime("optimeDate").ok().map(|d| d.timestamp_millis());
        let primary = members.iter().find(|m| m.get_str("stateStr").ok() == Some("PRIMARY")).and_then(optime);
        let lag = primary.map(|p| {
            members
                .iter()
                .filter(|m| m.get_str("stateStr").ok() == Some("SECONDARY"))
                .filter_map(optime)
                .map(|o| (p - o).max(0) as f64 / 1000.0)
                .fold(0.0, f64::max)
        });
        Some(Replication {
            role: role.into(),
            lag_secs: lag,
            replicas: members.len().saturating_sub(1) as i64,
            state: hello.get_str("setName").ok().map(|s| format!("replica set {s}")),
        })
    }
}

fn process(op: &Document) -> ServerProcess {
    let user = op
        .get_array("effectiveUsers")
        .ok()
        .and_then(|u| u.first()?.as_document()?.get_str("user").ok().map(str::to_owned));
    let ns = op.get_str("ns").ok().filter(|ns| !ns.is_empty());
    let micros = int_field(op, "microsecs_running");
    let active = op.get_bool("active").unwrap_or(false);
    let wait = if op.get_bool("waitingForLock").unwrap_or(false) {
        Some("lock".to_owned())
    } else {
        op.get_str("waitingForFlowControl").ok().map(|_| "flow control".to_owned())
    };
    ServerProcess {
        pid: op.get("opid").map_or(0, pid_of),
        user,
        database: ns.map(|ns| ns.split('.').next().unwrap_or(ns).to_owned()),
        client: op.get_str("client").ok().map(str::to_owned),
        state: Some(if active { "active" } else { "idle" }.to_owned()),
        duration_ms: micros.map(|m| m / 1000),
        query: short_query(op.get_document("command").ok().map(|c| compact(&Bson::Document(c.clone())))),
        wait,
        blocked_by: Vec::new(),
        active,
    }
}

#[async_trait]
impl Monitor for MongoSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let mut unavailable = Vec::new();
        let status = command(&self.client, "admin", doc! { "serverStatus": 1 }, None).await;
        let connections = match &status {
            Ok(s) => s.get_document("connections").ok().and_then(|c| {
                let used = int_field(c, "current")?;
                Some(Gauge { used, max: used + int_field(c, "available")? })
            }),
            Err(e) => {
                unavailable.push(format!("Server status: {}", e.message));
                None
            }
        };
        let processes = match self.current_ops().await {
            Ok((ops, note)) => {
                unavailable.extend(note);
                ops.iter().map(process).collect()
            }
            Err(e) => {
                unavailable.push(format!("Operations: {}", e.message));
                Vec::new()
            }
        };
        let long_ms = i64::from(long_running_secs) * 1000;
        let active: Vec<&ServerProcess> = processes.iter().filter(|p: &&ServerProcess| p.active).collect();
        unavailable.push("MongoDB locks are shown per operation (waiting for lock), not as a lock list.".into());
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections,
            active_queries: Some(active.len() as i64),
            long_running: Some(active.iter().filter(|p| p.duration_ms.unwrap_or(0) > long_ms).count() as i64),
            blocked: Some(processes.iter().filter(|p| p.wait.is_some()).count() as i64),
            replication: self.replication().await,
            processes,
            locks: Vec::new(),
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let reply = command(&self.client, "admin", doc! { "getParameter": "*" }, None).await?;
        let needle = search.unwrap_or_default().to_lowercase();
        Ok(reply
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "ok" | "$clusterTime" | "operationTime"))
            .filter(|(k, _)| k.to_lowercase().contains(&needle))
            .take(1000)
            .map(|(k, v)| ServerSetting {
                name: k.clone(),
                value: match v {
                    Bson::String(s) => s.clone(),
                    other => compact(other),
                },
                unit: None,
                category: Some("Parameter".into()),
                description: None,
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = self.database_or_current(database).to_owned();
        let specs = catalog::list_collections(self, &database, Document::new()).await?;
        let objects: Vec<_> = specs
            .iter()
            .filter(|s| !s.view)
            .take(500)
            .map(|s| dq_core::node::ObjectRef::new(NodeKind::Table, database.clone(), None, s.name.clone()))
            .collect();
        let mut sizes: Vec<ObjectSize> = stream::iter(objects)
            .map(|object| async move {
                let stats = catalog::storage(self, &object).await?;
                let table = int_field(&stats, "storageSize").unwrap_or(0);
                let index = int_field(&stats, "totalIndexSize").unwrap_or(0);
                Some(ObjectSize {
                    total_bytes: table + index,
                    table_bytes: table,
                    index_bytes: index,
                    rows: int_field(&stats, "count"),
                    object,
                })
            })
            .buffer_unordered(8)
            .filter_map(|s| async move { s })
            .collect()
            .await;
        sizes.sort_by_key(|s| std::cmp::Reverse(s.total_bytes));
        sizes.truncate(limit.clamp(1, 1000) as usize);
        Ok(sizes)
    }

    /// `killOp` stops the operation; `MongoDB` cannot end another client's connection.
    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        let _ = terminate;
        let (ops, _) = self.current_ops().await?;
        let opid = ops
            .iter()
            .filter_map(|op| op.get("opid"))
            .find(|opid| pid_of(opid) == pid)
            .cloned()
            .ok_or_else(|| DbError::not_found("The operation has already finished."))?;
        command(&self.client, "admin", doc! { "killOp": 1, "op": opid }, None).await?;
        Ok(true)
    }
}
