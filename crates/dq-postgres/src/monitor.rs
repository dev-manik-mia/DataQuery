//! Monitoring from `pg_stat_activity`, `pg_locks`, `pg_settings` and the
//! replication views. Rows other roles own show no query text unless the
//! account has `pg_read_all_stats`.

use async_trait::async_trait;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};
use tokio_postgres::Client;

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
            unavailable.push(
                "Query text of other roles' sessions needs the pg_read_all_stats role.".to_owned(),
            );
        }
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version: self.info_version(),
            connections: Some(Gauge { used: gauge.get(0), max: gauge.get(1) }),
            active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
            long_running: Some(processes.iter().filter(|p| p.active && p.duration_ms.unwrap_or(0) > long_ms).count() as i64),
            blocked: Some(processes.iter().filter(|p| !p.blocked_by.is_empty()).count() as i64),
            replication,
            processes,
            locks,
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let client = self.catalog_client("").await?;
        let pattern = search.map(|s| format!("%{}%", dq_core::sql::builder::like_contains(s).trim_matches('%')));
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
        let pid = i32::try_from(pid).map_err(|_| DbError::invalid("Not a PostgreSQL process id."))?;
        let sql = if terminate { "SELECT pg_terminate_backend($1)" } else { "SELECT pg_cancel_backend($1)" };
        Ok(client.query_one(sql, &[&pid]).await.map_err(pg)?.get(0))
    }
}
