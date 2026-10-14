//! Monitoring from `V$SESSION`, `V$LOCK` and `V$PARAMETER` (the account needs
//! `SELECT_CATALOG_ROLE` or `SELECT ANY DICTIONARY`). Cancelling and ending
//! sessions needs `ALTER SYSTEM`.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult, ErrorKind};

use crate::rows::{first_row, text_rows};
use crate::session::OraSession;

const PROCESSES: &str = "SELECT s.sid, s.username, s.schemaname, s.machine || NVL2(s.program, ' · ' || s.program, ''), \
       s.status, s.last_call_et * 1000, q.sql_text, s.event, s.blocking_session, s.wait_class \
FROM v$session s LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number \
WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID') \
ORDER BY CASE s.status WHEN 'ACTIVE' THEN 0 ELSE 1 END, s.last_call_et DESC FETCH FIRST 500 ROWS ONLY";

/// `V$LOCK` lock modes.
fn lock_mode(mode: i64) -> &'static str {
    match mode {
        1 => "NULL",
        2 => "ROW SHARE",
        3 => "ROW EXCLUSIVE",
        4 => "SHARE",
        5 => "SHARE ROW EXCLUSIVE",
        6 => "EXCLUSIVE",
        _ => "NONE",
    }
}

fn needs_dictionary(e: DbError) -> DbError {
    if e.kind == ErrorKind::NotFound || e.kind == ErrorKind::Permission {
        DbError::new(ErrorKind::Permission, "Monitoring reads the V$ views, which need SELECT_CATALOG_ROLE.")
            .with_action(Some("Ask a DBA to grant SELECT_CATALOG_ROLE to this user."))
    } else {
        e
    }
}

#[async_trait]
impl Monitor for OraSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let version = self.info().version.clone();
        let own_sid = self.sid;
        self.pool
            .run(move |conn| {
                let rows = text_rows(conn, PROCESSES, &[]).map_err(needs_dictionary)?;
                let processes: Vec<ServerProcess> = rows
                    .iter()
                    .filter(|r| r.number(0) != Some(own_sid))
                    .map(|r| {
                        let state = r.text(4).map(|s| s.to_ascii_lowercase());
                        ServerProcess {
                            pid: r.number(0).unwrap_or_default(),
                            user: r.text(1),
                            database: r.text(2),
                            client: r.text(3),
                            active: state.as_deref() == Some("active") && r.text(9).as_deref() != Some("Idle"),
                            state,
                            duration_ms: r.number(5),
                            query: short_query(r.text(6)),
                            wait: r.text(7).filter(|_| r.text(9).as_deref() != Some("Idle")),
                            blocked_by: r.number(8).into_iter().collect(),
                        }
                    })
                    .collect();
                let gauge = first_row(
                    conn,
                    "SELECT (SELECT COUNT(*) FROM v$session WHERE type = 'USER'), \
                            (SELECT TO_NUMBER(value) FROM v$parameter WHERE name = 'sessions') FROM DUAL",
                    &[],
                )?;
                let mut unavailable = Vec::new();
                let locks = match text_rows(
                    conn,
                    "SELECT l.sid, l.type, l.lmode, l.request, o.owner || '.' || o.object_name \
                     FROM v$lock l LEFT JOIN all_objects o ON l.type = 'TM' AND o.object_id = l.id1 \
                     WHERE l.type IN ('TM', 'TX') AND l.sid <> SYS_CONTEXT('USERENV', 'SID') \
                     ORDER BY l.request DESC, l.sid FETCH FIRST 500 ROWS ONLY",
                    &[],
                ) {
                    Ok(rows) => rows
                        .iter()
                        .map(|r| {
                            let request = r.number(3).unwrap_or(0);
                            LockInfo {
                                pid: r.number(0).unwrap_or_default(),
                                lock_type: r.string(1),
                                mode: lock_mode(if request > 0 { request } else { r.number(2).unwrap_or(0) })
                                    .to_owned(),
                                granted: request == 0,
                                object: r.text(4),
                            }
                        })
                        .collect(),
                    Err(e) => {
                        unavailable.push(format!("Locks: {}", e.message));
                        Vec::new()
                    }
                };
                let long_ms = i64::from(long_running_secs) * 1000;
                Ok(MonitorSnapshot {
                    version,
                    connections: gauge.and_then(|g| Some(Gauge { used: g.number(0)?, max: g.number(1)? })),
                    active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
                    long_running: Some(
                        processes.iter().filter(|p| p.active && p.duration_ms.unwrap_or(0) > long_ms).count() as i64,
                    ),
                    blocked: Some(processes.iter().filter(|p| !p.blocked_by.is_empty()).count() as i64),
                    replication: None,
                    processes,
                    locks,
                    unavailable,
                })
            })
            .await
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let pattern = dq_core::sql::builder::like_contains(&search.unwrap_or_default().to_lowercase());
        self.pool
            .run(move |conn| {
                let rows = text_rows(
                    conn,
                    "SELECT name, value, description, CASE isdefault WHEN 'TRUE' THEN 'Default' ELSE 'Changed' END \
                     FROM v$parameter WHERE name LIKE :1 ESCAPE '\\' ORDER BY name",
                    &[&pattern],
                )
                .map_err(needs_dictionary)?;
                Ok(rows
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

    async fn largest_tables(&self, _database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = self.info().database.clone();
        self.pool
            .run(move |conn| {
                let sql = |view: &str, owner: &str| {
                    format!(
                        "SELECT t.owner, t.table_name, \
                                NVL((SELECT SUM(s.bytes) FROM {view} s WHERE {owner} s.segment_name = t.table_name), 0), \
                                NVL((SELECT SUM(s.bytes) FROM {view} s JOIN all_indexes i ON i.index_name = s.segment_name \
                                     WHERE {owner} i.table_owner = t.owner AND i.table_name = t.table_name), 0), \
                                t.num_rows \
                         FROM all_tables t WHERE t.owner = USER \
                         ORDER BY 3 + 4 DESC FETCH FIRST :1 ROWS ONLY"
                    )
                };
                let rows = text_rows(conn, &sql("user_segments", ""), &[&i64::from(limit.clamp(1, 1000))])?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let (table, index) = (r.number(2).unwrap_or(0), r.number(3).unwrap_or(0));
                        ObjectSize {
                            object: ObjectRef::new(NodeKind::Table, database.clone(), r.text(0), r.string(1)),
                            total_bytes: table + index,
                            table_bytes: table,
                            index_bytes: index,
                            rows: r.number(4),
                        }
                    })
                    .collect())
            })
            .await
    }

    /// Cancel runs `ALTER SYSTEM CANCEL SQL`; terminate ends the session with `KILL SESSION`.
    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        if pid == self.sid {
            return Err(DbError::invalid("This is DataQuery's own query connection; use Cancel in the editor."));
        }
        self.pool
            .run(move |conn| {
                let serial = first_row(conn, "SELECT serial# FROM v$session WHERE sid = :1", &[&pid])
                    .map_err(needs_dictionary)?
                    .and_then(|r| r.number(0))
                    .ok_or_else(|| DbError::not_found("The session has already ended."))?;
                // Both values are integers read from the server, never user text.
                let sql = if terminate {
                    format!("ALTER SYSTEM KILL SESSION '{pid},{serial}' IMMEDIATE")
                } else {
                    format!("ALTER SYSTEM CANCEL SQL '{pid},{serial}'")
                };
                conn.execute(&sql, &[]).map_err(|e| crate::error::map_error(&e, None))?;
                Ok(true)
            })
            .await
    }
}
