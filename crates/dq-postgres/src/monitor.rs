//! Monitoring from `pg_stat_activity`, `pg_locks`, `pg_settings` and the
//! replication views. Rows other roles own show no query text unless the
//! account has `pg_read_all_stats`.
//!
//! `CockroachDB` keeps the same information in `crdb_internal` (sessions,
//! queries, locks) and cancels with `CANCEL QUERY` / `CANCEL SESSION`; see
//! [`cockroach`].

use async_trait::async_trait;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};
use tokio_postgres::Client;

use crate::connect::Flavor;
use crate::error::map_error;
use crate::session::PgSession;

const MAX_PROCESSES: i64 = 500;

fn pg(e: tokio_postgres::Error) -> DbError {
    map_error(&e, None)
}

async fn processes(client: &Client) -> DbResult<Vec<ServerProcess>> {
    let rows = client
        .query(
            "SELECT pid::int8, usename::text, datname::text, client_addr::text, state, \
                    (extract(epoch FROM now() - coalesce(query_start, backend_start)) * 1000)::int8, \
                    query, wait_event_type || ': ' || wait_event, \
                    (SELECT array_agg(b::int8) FROM unnest(pg_blocking_pids(pid)) b) \
             FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid() \
             ORDER BY state = 'active' DESC, query_start NULLS LAST LIMIT $1",
            &[&MAX_PROCESSES],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let state: Option<String> = r.get(4);
            let query: Option<String> = r.get(6);
            ServerProcess {
                pid: r.get(0),
                user: r.get(1),
                database: r.get(2),
                client: r.get(3),
                active: state.as_deref() == Some("active"),
                state,
                duration_ms: r.get(5),
                query: short_query(query.filter(|q| q != "<insufficient privilege>")),
                wait: r.get(7),
                blocked_by: r.get::<_, Option<Vec<i64>>>(8).unwrap_or_default(),
            }
        })
        .collect())
}

async fn locks(client: &Client) -> DbResult<Vec<LockInfo>> {
    let rows = client
        .query(
            "SELECT l.pid::int8, l.locktype, l.mode, l.granted, \
                    CASE WHEN l.relation IS NOT NULL THEN l.relation::regclass::text END \
             FROM pg_locks l \
             WHERE l.pid IS NOT NULL AND l.pid <> pg_backend_pid() \
               AND (l.database IS NULL OR l.database = (SELECT oid FROM pg_database WHERE datname = current_database())) \
               AND (NOT l.granted OR l.locktype IN ('relation', 'transactionid', 'tuple')) \
               AND NOT (l.locktype = 'relation' AND l.mode = 'AccessShareLock' AND l.granted \
                        AND l.relation::regclass::text LIKE 'pg\\_%') \
             ORDER BY l.granted, l.pid LIMIT 500",
            &[],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| LockInfo { pid: r.get(0), lock_type: r.get(1), mode: r.get(2), granted: r.get(3), object: r.get(4) })
        .collect())
}

async fn replication(client: &Client) -> DbResult<Replication> {
    let r = client
        .query_one(
            "SELECT pg_is_in_recovery(), \
                    CASE WHEN pg_is_in_recovery() THEN extract(epoch FROM now() - pg_last_xact_replay_timestamp())::float8 END, \
                    (SELECT count(*) FROM pg_stat_replication), \
                    (SELECT max(extract(epoch FROM replay_lag))::float8 FROM pg_stat_replication), \
                    (SELECT string_agg(DISTINCT state, ', ') FROM pg_stat_replication)",
            &[],
        )
        .await
        .map_err(pg)?;
    let replica: bool = r.get(0);
    Ok(Replication {
        role: if replica { "replica" } else { "primary" }.into(),
        lag_secs: if replica { r.get(1) } else { r.get(3) },
        replicas: r.get(2),
        state: if replica { Some("recovering".into()) } else { r.get(4) },
    })
}

#[async_trait]
impl Monitor for PgSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let client = self.catalog_client("").await?;
        if self.flavor == Flavor::Cockroach {
            return cockroach::snapshot(&client, self.info_version(), long_running_secs).await;
        }
        let gauge = client
            .query_one(
                "SELECT (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend'), \
                        current_setting('max_connections')::int8",
                &[],
            )
            .await
            .map_err(pg)?;
        let mut unavailable = Vec::new();
        let processes = processes(&client).await?;
        let locks = match locks(&client).await {
            Ok(l) => l,
            Err(e) => {
                unavailable.push(format!("Locks: {}", e.message));
                Vec::new()
            }
        };
        let replication = match replication(&client).await {
            Ok(r) => Some(r),
            Err(e) => {
                unavailable.push(format!("Replication: {}", e.message));
                None
            }
        };
        if processes.iter().any(|p| p.query.is_none() && p.state.is_some()) {
            unavailable.push("Query text of other roles' sessions needs the pg_read_all_stats role.".to_owned());
        }
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version: self.info_version(),
            connections: Some(Gauge { used: gauge.get(0), max: gauge.get(1) }),
            active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
            long_running: Some(
                processes.iter().filter(|p| p.active && p.duration_ms.unwrap_or(0) > long_ms).count() as i64
            ),
            blocked: Some(processes.iter().filter(|p| !p.blocked_by.is_empty()).count() as i64),
            replication,
            processes,
            locks,
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let client = self.catalog_client("").await?;
        if self.flavor == Flavor::Cockroach {
            return cockroach::settings(&client, search).await;
        }
        let pattern = search.map(dq_core::sql::builder::like_contains);
        let rows = client
            .query(
                "SELECT name, setting, unit, category, short_desc FROM pg_settings \
                 WHERE $1::text IS NULL OR name ILIKE $1 ORDER BY category, name",
                &[&pattern],
            )
            .await
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| ServerSetting {
                name: r.get(0),
                value: r.get(1),
                unit: r.get(2),
                category: r.get(3),
                description: r.get(4),
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let client = self.catalog_client(database).await?;
        let database = if database.is_empty() { self.info_database() } else { database.to_owned() };
        if self.flavor == Flavor::Cockroach {
            return cockroach::largest_tables(&client, &database, limit).await;
        }
        let rows = client
            .query(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text, pg_total_relation_size(c.oid), \
                        pg_relation_size(c.oid), pg_indexes_size(c.oid), c.reltuples::int8 \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r', 'm', 'p') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
                 ORDER BY 4 DESC LIMIT $1",
                &[&i64::from(limit.clamp(1, 1000))],
            )
            .await
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| {
                let kind = if r.get::<_, String>(2) == "m" { NodeKind::MaterializedView } else { NodeKind::Table };
                ObjectSize {
                    object: ObjectRef::new(kind, database.clone(), Some(r.get(0)), r.get::<_, String>(1)),
                    total_bytes: r.get(3),
                    table_bytes: r.get(4),
                    index_bytes: r.get(5),
                    rows: Some(r.get::<_, i64>(6)).filter(|n| *n >= 0),
                }
            })
            .collect())
    }

    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        let client = self.catalog_client("").await?;
        if self.flavor == Flavor::Cockroach {
            return cockroach::kill(&client, pid, terminate).await;
        }
        let pid = i32::try_from(pid).map_err(|_| DbError::invalid("Not a PostgreSQL process id."))?;
        let sql = if terminate { "SELECT pg_terminate_backend($1)" } else { "SELECT pg_cancel_backend($1)" };
        Ok(client.query_one(sql, &[&pid]).await.map_err(pg)?.get(0))
    }
}

/// `CockroachDB`: sessions, queries and locks from `crdb_internal`. Process ids
/// are the sessions' `pg_backend_pid`.
mod cockroach {
    use std::collections::HashMap;

    use dq_core::monitor::{LockInfo, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query};
    use dq_core::node::{NodeKind, ObjectRef};
    use dq_core::sql::{Dialect, PostgresDialect};
    use dq_core::{DbError, DbResult};
    use tokio_postgres::Client;

    use super::pg;

    pub async fn snapshot(client: &Client, version: String, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let rows = client
            .query(
                "SELECT s.pg_backend_pid::int8, s.user_name, q.database, s.client_address, \
                        CASE WHEN q.query_id IS NULL THEN 'idle' ELSE coalesce(q.phase, 'executing') END, \
                        (extract(epoch FROM now() - coalesce(q.start, s.session_start)) * 1000)::int8, \
                        q.query, s.kv_txn::text \
                 FROM crdb_internal.cluster_sessions s \
                 LEFT JOIN crdb_internal.cluster_queries q ON q.session_id = s.session_id \
                 WHERE s.status = 'ACTIVE' AND s.pg_backend_pid <> pg_backend_pid() \
                   AND s.application_name NOT LIKE '$ internal%' \
                 ORDER BY q.query_id IS NULL, q.start LIMIT 500",
                &[],
            )
            .await
            .map_err(pg)?;
        // Transaction → session, to name lock holders and waiters by process.
        let mut by_txn: HashMap<String, i64> = HashMap::new();
        let mut processes: Vec<ServerProcess> = rows
            .iter()
            .map(|r| {
                let pid: i64 = r.get(0);
                if let Some(txn) = r.get::<_, Option<String>>(7) {
                    by_txn.insert(txn, pid);
                }
                let state: Option<String> = r.get(4);
                ServerProcess {
                    pid,
                    user: r.get(1),
                    database: r.get(2),
                    client: r.get(3),
                    active: state.as_deref() != Some("idle"),
                    state,
                    duration_ms: r.get(5),
                    query: short_query(r.get(6)),
                    wait: None,
                    blocked_by: Vec::new(),
                }
            })
            .collect();
        let mut unavailable = Vec::new();
        let lock_rows = client
            .query(
                "SELECT txn_id::text, lock_key_pretty, lock_strength, granted, \
                        schema_name || '.' || table_name, durability \
                 FROM crdb_internal.cluster_locks WHERE txn_id IS NOT NULL ORDER BY granted, lock_key LIMIT 500",
                &[],
            )
            .await;
        let mut locks = Vec::new();
        match lock_rows {
            Ok(rows) => {
                let mut holders: HashMap<String, i64> = HashMap::new();
                let mut waiters: Vec<(String, i64)> = Vec::new();
                for r in &rows {
                    let Some(pid) = r.get::<_, Option<String>>(0).and_then(|t| by_txn.get(&t).copied()) else {
                        continue;
                    };
                    let key: String = r.get(1);
                    let granted: bool = r.get(3);
                    if granted {
                        holders.insert(key.clone(), pid);
                    } else {
                        waiters.push((key.clone(), pid));
                    }
                    locks.push(LockInfo {
                        pid,
                        lock_type: r.get::<_, Option<String>>(5).unwrap_or_else(|| "row".into()),
                        mode: r.get::<_, Option<String>>(2).unwrap_or_default(),
                        granted,
                        object: r.get(4),
                    });
                }
                for (key, pid) in waiters {
                    if let (Some(holder), Some(p)) = (holders.get(&key), processes.iter_mut().find(|p| p.pid == pid)) {
                        p.blocked_by.push(*holder);
                        p.wait = Some("lock".into());
                    }
                }
            }
            Err(e) => unavailable.push(format!("Locks: {}", pg(e).message)),
        }
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version,
            // CockroachDB has no server-wide connection limit by default.
            connections: None,
            active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
            long_running: Some(
                processes.iter().filter(|p| p.active && p.duration_ms.unwrap_or(0) > long_ms).count() as i64
            ),
            blocked: Some(processes.iter().filter(|p| !p.blocked_by.is_empty()).count() as i64),
            replication: None,
            processes,
            locks,
            unavailable,
        })
    }

    pub async fn settings(client: &Client, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let pattern = search.map(dq_core::sql::builder::like_contains);
        let rows = client
            .query(
                "SELECT variable, value, setting_type, description FROM [SHOW CLUSTER SETTINGS] \
                 WHERE $1::text IS NULL OR variable ILIKE $1 ORDER BY variable",
                &[&pattern],
            )
            .await
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| {
                let name: String = r.get(0);
                ServerSetting {
                    category: name.split('.').next().map(str::to_owned),
                    name,
                    value: r.get(1),
                    unit: None,
                    description: r.get(3),
                }
            })
            .collect())
    }

    pub async fn largest_tables(client: &Client, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        // Sizes are per range; a range shared by small tables counts for each of them.
        let sql = format!(
            "SELECT r.schema_name, r.table_name, sum(r.range_size)::int8, \
                    (SELECT max(s.estimated_row_count)::int8 FROM crdb_internal.table_row_statistics s \
                     WHERE s.table_id = r.table_id) \
             FROM [SHOW RANGES FROM DATABASE {} WITH TABLES, DETAILS] r \
             GROUP BY r.schema_name, r.table_name, r.table_id ORDER BY 3 DESC LIMIT $1",
            PostgresDialect.quote_ident(database)
        );
        let rows = client.query(&sql, &[&i64::from(limit.clamp(1, 1000))]).await.map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| {
                let bytes: i64 = r.get(2);
                ObjectSize {
                    object: ObjectRef::new(NodeKind::Table, database, Some(r.get(0)), r.get::<_, String>(1)),
                    total_bytes: bytes,
                    table_bytes: bytes,
                    index_bytes: 0,
                    rows: r.get(3),
                }
            })
            .collect())
    }

    pub async fn kill(client: &Client, pid: i64, terminate: bool) -> DbResult<bool> {
        let row = client
            .query_opt(
                "SELECT s.session_id, q.query_id FROM crdb_internal.cluster_sessions s \
                 LEFT JOIN crdb_internal.cluster_queries q ON q.session_id = s.session_id \
                 WHERE s.pg_backend_pid = $1 AND s.status = 'ACTIVE' LIMIT 1",
                &[&pid],
            )
            .await
            .map_err(pg)?
            .ok_or_else(|| DbError::not_found("The session has already ended."))?;
        let (session, query): (String, Option<String>) = (row.get(0), row.get(1));
        if terminate {
            client.execute("CANCEL SESSION $1", &[&session]).await.map_err(pg)?;
            return Ok(true);
        }
        let Some(query) = query else { return Ok(false) };
        client.execute("CANCEL QUERY IF EXISTS $1", &[&query]).await.map_err(pg)?;
        Ok(true)
    }
}
