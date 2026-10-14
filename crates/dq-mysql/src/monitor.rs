//! Monitoring from the process list, status variables, InnoDB lock views and
//! replication status. Each source needs its own privileges (PROCESS,
//! REPLICATION CLIENT, performance_schema access); missing ones are reported.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

use crate::connect::Flavor;
use crate::error::map_error;
use crate::session::MySession;
use crate::values::text;

fn my(e: mysql_async::Error) -> DbError {
    map_error(&e, None)
}

fn number(row: &Row, i: usize) -> Option<i64> {
    text(row, i).and_then(|v| v.parse().ok())
}

/// A column of a `SHOW … STATUS` row, by name.
fn named(row: &Row, name: &str) -> Option<String> {
    let index = row.columns_ref().iter().position(|c| c.name_str().eq_ignore_ascii_case(name))?;
    text(row, index)
}

/// Commands that are background work rather than client queries.
const IDLE_COMMANDS: &[&str] = &["Sleep", "Daemon", "Binlog Dump", "Binlog Dump GTID"];

async fn processes(conn: &mut Conn) -> DbResult<Vec<ServerProcess>> {
    let rows: Vec<Row> = conn
        .query(
            "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO FROM information_schema.PROCESSLIST \
             WHERE ID <> CONNECTION_ID() ORDER BY COMMAND = 'Sleep', TIME DESC LIMIT 500",
        )
        .await
        .map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| {
            let command = text(r, 4).unwrap_or_default();
            let state = text(r, 6).filter(|s| !s.is_empty());
            ServerProcess {
                pid: number(r, 0).unwrap_or_default(),
                user: text(r, 1),
                client: text(r, 2),
                database: text(r, 3),
                active: !IDLE_COMMANDS.contains(&command.as_str()),
                state: Some(match state {
                    Some(s) => format!("{command} · {s}"),
                    None => command,
                }),
                duration_ms: number(r, 5).map(|s| s * 1000),
                query: short_query(text(r, 7)),
                wait: None,
                blocked_by: Vec::new(),
            }
        })
        .collect())
}

/// Whether the account sees every session (global PROCESS privilege).
async fn has_process_privilege(conn: &mut Conn) -> bool {
    let grants: Vec<Row> = conn.query("SHOW GRANTS").await.unwrap_or_default();
    grants.iter().filter_map(|r| text(r, 0)).any(|g| {
        let g = g.to_ascii_uppercase();
        g.contains(" ON *.*") && (g.contains("ALL PRIVILEGES") || g.contains("PROCESS"))
    })
}

/// (waiting thread, blocking thread) pairs from InnoDB.
async fn lock_waits(conn: &mut Conn, flavor: Flavor) -> DbResult<Vec<(i64, i64)>> {
    let sql = match flavor {
        Flavor::Mysql => {
            "SELECT r.PROCESSLIST_ID, b.PROCESSLIST_ID FROM performance_schema.data_lock_waits w \
             JOIN performance_schema.threads r ON r.THREAD_ID = w.REQUESTING_THREAD_ID \
             JOIN performance_schema.threads b ON b.THREAD_ID = w.BLOCKING_THREAD_ID"
        }
        Flavor::Mariadb => {
            "SELECT r.trx_mysql_thread_id, b.trx_mysql_thread_id FROM information_schema.INNODB_LOCK_WAITS w \
             JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id \
             JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id"
        }
    };
    let rows: Vec<Row> = conn.query(sql).await.map_err(my)?;
    Ok(rows.iter().filter_map(|r| Some((number(r, 0)?, number(r, 1)?))).collect())
}

async fn locks(conn: &mut Conn, flavor: Flavor) -> DbResult<Vec<LockInfo>> {
    let sql = match flavor {
        Flavor::Mysql => {
            "SELECT t.PROCESSLIST_ID, l.LOCK_TYPE, l.LOCK_MODE, l.LOCK_STATUS, \
                    CONCAT(l.OBJECT_SCHEMA, '.', l.OBJECT_NAME) \
             FROM performance_schema.data_locks l JOIN performance_schema.threads t ON t.THREAD_ID = l.THREAD_ID \
             ORDER BY l.LOCK_STATUS = 'GRANTED', t.PROCESSLIST_ID LIMIT 500"
        }
        Flavor::Mariadb => {
            "SELECT x.trx_mysql_thread_id, l.lock_type, l.lock_mode, \
                    IF(EXISTS (SELECT 1 FROM information_schema.INNODB_LOCK_WAITS w WHERE w.requested_lock_id = l.lock_id), 'WAITING', 'GRANTED'), \
                    l.lock_table \
             FROM information_schema.INNODB_LOCKS l JOIN information_schema.INNODB_TRX x ON x.trx_id = l.lock_trx_id LIMIT 500"
        }
    };
    let rows: Vec<Row> = conn.query(sql).await.map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| LockInfo {
            pid: number(r, 0).unwrap_or_default(),
            lock_type: text(r, 1).unwrap_or_default(),
            mode: text(r, 2).unwrap_or_default(),
            granted: text(r, 3).as_deref() == Some("GRANTED"),
            object: text(r, 4),
        })
        .collect())
}

async fn replication(conn: &mut Conn) -> DbResult<Replication> {
    let status: Option<Row> = match conn.query_first("SHOW REPLICA STATUS").await {
        Ok(row) => row,
        Err(_) => conn.query_first("SHOW SLAVE STATUS").await.map_err(my)?,
    };
    if let Some(row) = status {
        let lag = named(&row, "Seconds_Behind_Source").or_else(|| named(&row, "Seconds_Behind_Master"));
        let io = named(&row, "Replica_IO_Running").or_else(|| named(&row, "Slave_IO_Running"));
        return Ok(Replication {
            role: "replica".into(),
            lag_secs: lag.and_then(|l| l.parse().ok()),
            replicas: 0,
            state: io.map(|s| format!("IO thread {s}")),
        });
    }
    let replicas: Vec<Row> = match conn.query("SHOW REPLICAS").await {
        Ok(rows) => rows,
        Err(_) => conn.query("SHOW SLAVE HOSTS").await.map_err(my)?,
    };
    Ok(Replication { role: "primary".into(), lag_secs: None, replicas: replicas.len() as i64, state: None })
}

#[async_trait]
impl Monitor for MySession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let mut conn = self.catalog_conn().await?;
        let used: Option<Row> = conn.query_first("SHOW GLOBAL STATUS LIKE 'Threads_connected'").await.map_err(my)?;
        let max: Option<Row> = conn.query_first("SELECT @@max_connections").await.map_err(my)?;
        let connections = used
            .and_then(|u| number(&u, 1))
            .zip(max.and_then(|m| number(&m, 0)))
            .map(|(used, max)| Gauge { used, max });
        let mut unavailable = Vec::new();
        let mut processes = processes(&mut conn).await?;
        if !has_process_privilege(&mut conn).await {
            unavailable.push("Other users' sessions need the PROCESS privilege.".to_owned());
        }
        let flavor = self.flavor();
        match lock_waits(&mut conn, flavor).await {
            Ok(waits) => {
                for (waiting, blocking) in waits {
                    if let Some(p) = processes.iter_mut().find(|p| p.pid == waiting) {
                        p.blocked_by.push(blocking);
                    }
                }
            }
            Err(e) => unavailable.push(format!("Lock waits: {}", e.message)),
        }
        let locks = locks(&mut conn, flavor).await.unwrap_or_else(|e| {
            unavailable.push(format!("Locks: {}", e.message));
            Vec::new()
        });
        let replication = match replication(&mut conn).await {
            Ok(r) => Some(r),
            Err(e) => {
                unavailable.push(format!("Replication: {}", e.message));
                None
            }
        };
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections,
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
        let mut conn = self.catalog_conn().await?;
        let pattern = search.map_or_else(|| "%".to_owned(), dq_core::sql::builder::like_contains);
        // `SHOW … LIKE ?` cannot be prepared; MySQL 8 moved the table to performance_schema.
        let table = match self.flavor() {
            Flavor::Mysql => "performance_schema.global_variables",
            Flavor::Mariadb => "information_schema.GLOBAL_VARIABLES",
        };
        let sql = format!(
            "SELECT LOWER(VARIABLE_NAME), VARIABLE_VALUE FROM {table} WHERE LOWER(VARIABLE_NAME) LIKE LOWER(?) ORDER BY 1"
        );
        let rows: Vec<Row> = conn.exec(sql, (pattern,)).await.map_err(my)?;
        Ok(rows
            .iter()
            .map(|r| ServerSetting {
                name: text(r, 0).unwrap_or_default(),
                value: text(r, 1).unwrap_or_default(),
                unit: None,
                category: None,
                description: None,
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let mut conn = self.catalog_conn().await?;
        let database = if database.is_empty() { self.info().database.clone() } else { database.to_owned() };
        let rows: Vec<Row> = conn
            .exec(
                "SELECT TABLE_NAME, DATA_LENGTH, INDEX_LENGTH, TABLE_ROWS FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
                 ORDER BY DATA_LENGTH + INDEX_LENGTH DESC LIMIT ?",
                (&database, limit.clamp(1, 1000)),
            )
            .await
            .map_err(my)?;
        Ok(rows
            .iter()
            .map(|r| {
                let (data, index) = (number(r, 1).unwrap_or(0), number(r, 2).unwrap_or(0));
                ObjectSize {
                    object: ObjectRef::new(NodeKind::Table, database.clone(), None, text(r, 0).unwrap_or_default()),
                    total_bytes: data + index,
                    table_bytes: data,
                    index_bytes: index,
                    rows: number(r, 3),
                }
            })
            .collect())
    }

    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        let id = u64::try_from(pid).map_err(|_| DbError::invalid("Not a MySQL process id."))?;
        let mut conn = self.catalog_conn().await?;
        let sql = if terminate { format!("KILL CONNECTION {id}") } else { format!("KILL QUERY {id}") };
        match conn.query_drop(sql).await {
            // 1094: unknown thread id — it already ended.
            Err(mysql_async::Error::Server(e)) if e.code == 1094 => Ok(false),
            other => other.map(|()| true).map_err(my),
        }
    }
}
