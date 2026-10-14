//! Server monitoring (screen 07). What an engine can show depends on its
//! version and the account's privileges: anything unavailable is reported in
//! `unavailable` with the reason, and the UI hides that panel.

use async_trait::async_trait;
use serde::Serialize;
use ts_rs::TS;

use crate::error::DbResult;
use crate::node::ObjectRef;

#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct Gauge {
    #[ts(type = "number")]
    pub used: i64,
    #[ts(type = "number")]
    pub max: i64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Replication {
    /// `primary` or `replica`.
    pub role: String,
    /// Seconds the replica is behind (on a replica), or the largest lag of the replicas (on a primary).
    pub lag_secs: Option<f64>,
    #[ts(type = "number")]
    pub replicas: i64,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ServerProcess {
    #[ts(type = "number")]
    pub pid: i64,
    pub user: Option<String>,
    pub database: Option<String>,
    pub client: Option<String>,
    /// e.g. `active`, `idle in transaction`, `Query`, `Sleep`.
    pub state: Option<String>,
    /// Time in the current statement or state.
    #[ts(type = "number | null")]
    pub duration_ms: Option<i64>,
    /// Statement text, shortened; `None` when the account may not see it.
    pub query: Option<String>,
    pub wait: Option<String>,
    /// Processes this one waits for.
    #[ts(type = "number[]")]
    pub blocked_by: Vec<i64>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct LockInfo {
    #[ts(type = "number")]
    pub pid: i64,
    pub lock_type: String,
    pub mode: String,
    pub granted: bool,
    /// Locked table, when the lock is on one.
    pub object: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct MonitorSnapshot {
    pub version: String,
    pub connections: Option<Gauge>,
    #[ts(type = "number | null")]
    pub active_queries: Option<i64>,
    #[ts(type = "number | null")]
    pub long_running: Option<i64>,
    #[ts(type = "number | null")]
    pub blocked: Option<i64>,
    pub replication: Option<Replication>,
    pub processes: Vec<ServerProcess>,
    pub locks: Vec<LockInfo>,
    /// Panels that could not be read, with the reason (privileges, version).
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ServerSetting {
    pub name: String,
    pub value: String,
    pub unit: Option<String>,
    pub category: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ObjectSize {
    pub object: ObjectRef,
    #[ts(type = "number")]
    pub total_bytes: i64,
    #[ts(type = "number")]
    pub table_bytes: i64,
    #[ts(type = "number")]
    pub index_bytes: i64,
    #[ts(type = "number | null")]
    pub rows: Option<i64>,
}

/// Shortened statement text for the process list.
pub fn short_query(query: Option<String>) -> Option<String> {
    const MAX: usize = 4_000;
    query.map(|mut q| {
        if q.len() > MAX {
            let mut cut = MAX;
            while !q.is_char_boundary(cut) {
                cut -= 1;
            }
            q.truncate(cut);
            q.push('…');
        }
        q
    })
}

/// Implemented by sessions of engines with monitoring support. Runs on
/// catalog connections, never on the connection that runs user queries.
#[async_trait]
pub trait Monitor: Send + Sync {
    /// Activity, processes and locks. `long_running_secs` sets the threshold for "long running".
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot>;

    /// Server settings whose name contains `search`.
    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>>;

    /// Largest tables of `database`.
    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>>;

    /// Cancels the process's running statement, or ends its connection when
    /// `terminate`. Returns whether the server accepted the request.
    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool>;
}
