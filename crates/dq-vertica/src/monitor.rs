//! Monitoring from `v_monitor.sessions`, `locks`, `configuration_parameters`
//! and `projection_storage`. Other users' sessions need the SYSMONITOR role
//! (or superuser); statements stop with `INTERRUPT_STATEMENT` and sessions
//! end with `CLOSE_SESSION`.
//!
//! Vertica names sessions with text ids (`v_vdb_node0001-1234:0x5a`); the
//! process list shows a stable number derived from the id, and kill looks
//! the session up again by that number.

use async_trait::async_trait;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::{DbError, DbResult};

use crate::conn::Connection;
use crate::session::VtSession;

const MAX_PROCESSES: usize = 500;

/// A JavaScript-safe number for a session id (FNV-1a, 53 bits).
fn process_id(session_id: &str) -> i64 {
    let hash =
        session_id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    (hash & ((1 << 53) - 1)) as i64
}

/// Sessions other than this one: id, user, client, transaction, statement,
/// milliseconds in the statement, current statement.
async fn sessions(conn: &mut Connection) -> DbResult<Vec<TextRow>> {
    conn.text_rows(
        "SELECT session_id, user_name, client_hostname, transaction_id, statement_id, \
                DATEDIFF('millisecond', statement_start, CLOCK_TIMESTAMP()), current_statement \
         FROM v_monitor.sessions WHERE session_id <> CURRENT_SESSION() \
         ORDER BY current_statement = '', statement_start",
        &[],
    )
    .await
}

/// Locks: transaction, object, mode, scope, granted.
async fn locks(conn: &mut Connection) -> DbResult<Vec<TextRow>> {
    conn.text_rows(
        "SELECT transaction_id, object_name, lock_mode, lock_scope, grant_timestamp IS NOT NULL \
         FROM v_monitor.locks ORDER BY grant_timestamp IS NOT NULL, transaction_id LIMIT 500",
        &[],
    )
    .await
}

#[async_trait]
impl Monitor for VtSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let mut conn = self.catalog().await?;
        let mut unavailable = Vec::new();
        let found = sessions(&mut conn).await?;
        let lock_rows = locks(&mut conn).await.unwrap_or_else(|e| {
            unavailable.push(format!("Locks: {}", e.message));
            Vec::new()
        });
        let max = conn
            .text_rows(
                "SELECT MAX(current_value) FROM v_monitor.configuration_parameters WHERE parameter_name = 'MaxClientSessions'",
                &[],
            )
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| r.number(0)));
        drop(conn);
        let pid_of_transaction = |transaction: &str| {
            found.iter().find(|s| s.text(3).as_deref() == Some(transaction)).map(|s| process_id(&s.string(0)))
        };
        let locks: Vec<LockInfo> = lock_rows
            .iter()
            .filter_map(|l| {
                Some(LockInfo {
                    pid: pid_of_transaction(&l.string(0))?,
                    lock_type: l.string(3),
                    mode: l.string(2),
                    granted: l.flag(4),
                    object: l.text(1),
                })
            })
            .collect();
        let processes: Vec<ServerProcess> = found
            .iter()
            .take(MAX_PROCESSES)
            .map(|s| {
                let pid = process_id(&s.string(0));
                let query = s.text(6).filter(|q| !q.is_empty());
                let active = query.is_some();
                let waiting: Vec<&LockInfo> = locks.iter().filter(|l| l.pid == pid && !l.granted).collect();
                let blocked_by = locks
                    .iter()
                    .filter(|held| held.granted && held.pid != pid && waiting.iter().any(|w| w.object == held.object))
                    .map(|held| held.pid)
                    .collect::<Vec<_>>();
                let state = match (active, s.text(3).is_some()) {
                    (true, _) => "active",
                    (false, true) => "idle in transaction",
                    (false, false) => "idle",
                };
                ServerProcess {
                    pid,
                    user: s.text(1),
                    database: Some(self.info_database().to_owned()),
                    client: s.text(2),
                    state: Some(state.to_owned()),
                    duration_ms: if active { s.number(5) } else { None },
                    query: short_query(query),
                    wait: waiting.first().map(|w| format!("Lock: {} {}", w.mode, w.object.clone().unwrap_or_default())),
                    blocked_by,
                    active,
                }
            })
            .collect();
        let long_ms = i64::from(long_running_secs) * 1000;
        Ok(MonitorSnapshot {
            version: self.info_version().to_owned(),
            // This session counts too.
            connections: max.map(|max| Gauge { used: found.len() as i64 + 1, max }),
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

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let pattern = dq_core::sql::builder::like_contains(search.unwrap_or_default());
        let rows = self
            .catalog()
            .await?
            .text_rows(
                "SELECT parameter_name, MAX(current_value), MAX(groups), MAX(description) \
                 FROM v_monitor.configuration_parameters WHERE parameter_name ILIKE ? ESCAPE '\\' \
                 GROUP BY parameter_name ORDER BY parameter_name LIMIT 2000",
                &[&pattern],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ServerSetting {
                name: r.string(0),
                value: r.string(1),
                unit: None,
                category: r.text(2).filter(|c| !c.is_empty()),
                description: r.text(3).filter(|d| !d.is_empty()),
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = if database.is_empty() { self.info_database() } else { database };
        let rows = self
            .catalog()
            .await?
            .text_rows(
                &format!(
                    "SELECT anchor_table_schema, anchor_table_name, SUM(used_bytes)::INT, SUM(row_count)::INT \
                     FROM v_monitor.projection_storage GROUP BY 1, 2 ORDER BY 3 DESC LIMIT {}",
                    limit.clamp(1, 1_000)
                ),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let bytes = r.number(2).unwrap_or(0);
                ObjectSize {
                    object: ObjectRef::new(NodeKind::Table, database, Some(r.string(0)), r.string(1)),
                    total_bytes: bytes,
                    table_bytes: bytes,
                    index_bytes: 0,
                    rows: r.number(3),
                }
            })
            .collect())
    }

    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        let mut conn = self.catalog().await?;
        let found = sessions(&mut conn).await?;
        let session = found
            .iter()
            .find(|s| process_id(&s.string(0)) == pid)
            .ok_or_else(|| DbError::not_found("The session has ended."))?;
        let id = session.string(0);
        let done = if terminate {
            conn.text_rows("SELECT CLOSE_SESSION(?)", &[&id]).await?
        } else {
            let Some(statement) = session.text(4) else { return Ok(false) };
            conn.text_rows("SELECT INTERRUPT_STATEMENT(?, ?::INT)", &[&id, &statement]).await?
        };
        Ok(!done.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_ids_are_stable_and_safe_in_javascript() {
        let id = "v_vdb_node0001-1234:0x5a";
        assert_eq!(process_id(id), process_id(id));
        assert_ne!(process_id(id), process_id("v_vdb_node0001-1234:0x5b"));
        assert!(process_id(id) < (1 << 53));
    }
}
