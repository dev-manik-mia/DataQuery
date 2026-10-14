//! Monitoring from the dynamic management views. Seeing other sessions needs
//! `VIEW SERVER STATE`; ending them needs `ALTER ANY CONNECTION`.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::monitor::{
    Gauge, LockInfo, Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::{DbError, DbResult};

use crate::rows::{first_row, text_rows};
use crate::session::MsSession;

const PROCESSES: &str = "SELECT TOP 500 s.session_id, s.login_name, DB_NAME(COALESCE(r.database_id, s.database_id)), \
       s.host_name + COALESCE(' · ' + NULLIF(s.program_name, ''), ''), COALESCE(r.status, s.status), \
       DATEDIFF_BIG(millisecond, COALESCE(r.start_time, s.last_request_start_time), SYSDATETIME()), \
       t.text, r.wait_type, NULLIF(r.blocking_session_id, 0), \
       CASE WHEN r.session_id IS NOT NULL AND r.status NOT IN ('sleeping', 'background') THEN 1 ELSE 0 END \
FROM sys.dm_exec_sessions s \
LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t \
WHERE s.is_user_process = 1 AND s.session_id <> @@SPID \
ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, 6 DESC";

#[async_trait]
impl Monitor for MsSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let mut pooled = self.pool.get("").await?;
        let conn = pooled.conn();
        let mut unavailable = Vec::new();
        let can_see_all = first_row(conn, "SELECT HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE')", &[], None)
            .await?
            .is_some_and(|r| r.flag(0));
        if !can_see_all {
            unavailable.push("Other sessions and locks need the VIEW SERVER STATE permission.".to_owned());
        }
        let rows = text_rows(conn, PROCESSES, &[], None).await?;
        let processes: Vec<ServerProcess> = rows
            .iter()
            .filter(|r| r.number(0) != Some(self.spid))
            .map(|r| ServerProcess {
                pid: r.number(0).unwrap_or_default(),
                user: r.text(1),
                database: r.text(2),
                client: r.text(3),
                state: r.text(4),
                duration_ms: r.number(5),
                query: short_query(r.text(6)),
                wait: r.text(7),
                blocked_by: r.number(8).into_iter().collect(),
                active: r.flag(9),
            })
            .collect();
        let active = processes.iter().filter(|p| p.active).count() as i64;
        let threshold = i64::from(long_running_secs) * 1000;
        let long_running =
            processes.iter().filter(|p| p.active && p.duration_ms.unwrap_or(0) > threshold).count() as i64;
        let blocked = processes.iter().filter(|p| !p.blocked_by.is_empty()).count() as i64;
        let gauge = first_row(
            conn,
            "SELECT (SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE is_user_process = 1), \
                    CAST((SELECT value_in_use FROM sys.configurations WHERE name = 'user connections') AS int), @@MAX_CONNECTIONS",
            &[],
            None,
        )
        .await?;
        let connections = gauge.and_then(|g| {
            let configured = g.number(1).filter(|m| *m > 0);
            Some(Gauge { used: g.number(0)?, max: configured.or(g.number(2))? })
        });
        let locks = match text_rows(
            conn,
            "SELECT TOP 200 l.request_session_id, l.resource_type, l.request_mode, \
                    CASE WHEN l.request_status = 'GRANT' THEN 1 ELSE 0 END, \
                    CASE WHEN l.resource_type = 'OBJECT' THEN \
                      OBJECT_SCHEMA_NAME(l.resource_associated_entity_id, l.resource_database_id) + '.' + \
                      OBJECT_NAME(l.resource_associated_entity_id, l.resource_database_id) END \
             FROM sys.dm_tran_locks l WHERE l.resource_type <> 'DATABASE' AND l.request_session_id <> @@SPID \
             ORDER BY l.request_status DESC, l.request_session_id",
            &[],
            None,
        )
        .await
        {
            Ok(rows) => rows
                .iter()
                .map(|r| LockInfo {
                    pid: r.number(0).unwrap_or_default(),
                    lock_type: r.string(1),
                    mode: r.string(2),
                    granted: r.flag(3),
                    object: r.text(4),
                })
                .collect(),
            Err(e) => {
                unavailable.push(format!("Locks: {}", e.message));
                Vec::new()
            }
        };
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections,
            active_queries: Some(active),
            long_running: Some(long_running),
            blocked: Some(blocked),
            replication: None,
            processes,
            locks,
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let mut pooled = self.pool.get("").await?;
        let pattern = dq_core::sql::builder::like_contains(search.unwrap_or_default());
        let rows = text_rows(
            pooled.conn(),
            "SELECT name, CAST(value_in_use AS nvarchar(64)), CAST(description AS nvarchar(4000)), is_advanced \
             FROM sys.configurations WHERE name LIKE @P1 ESCAPE '\\' ORDER BY name",
            &[&pattern],
            None,
        )
        .await?;
        Ok(rows
            .iter()
            .map(|r| ServerSetting {
                name: r.string(0),
                value: r.string(1),
                unit: None,
                category: Some(if r.flag(3) { "Advanced" } else { "General" }.to_owned()),
                description: r.text(2),
            })
            .collect())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let database = if database.is_empty() { self.info().database.clone() } else { database.to_owned() };
        let mut pooled = self.pool.get(&database).await?;
        let rows = text_rows(
            pooled.conn(),
            "SELECT TOP (CAST(@P1 AS int)) OBJECT_SCHEMA_NAME(s.object_id), OBJECT_NAME(s.object_id), \
                    SUM(s.used_page_count) * 8192, \
                    SUM(CASE WHEN s.index_id IN (0, 1) THEN s.used_page_count ELSE 0 END) * 8192, \
                    SUM(CASE WHEN s.index_id > 1 THEN s.used_page_count ELSE 0 END) * 8192, \
                    SUM(CASE WHEN s.index_id IN (0, 1) THEN s.row_count ELSE 0 END) \
             FROM sys.dm_db_partition_stats s JOIN sys.objects o ON o.object_id = s.object_id \
             WHERE o.type = 'U' AND o.is_ms_shipped = 0 \
             GROUP BY s.object_id ORDER BY 3 DESC",
            &[&limit.to_string()],
            None,
        )
        .await?;
        Ok(rows
            .iter()
            .map(|r| ObjectSize {
                object: ObjectRef::new(NodeKind::Table, database.clone(), r.text(0), r.string(1)),
                total_bytes: r.number(2).unwrap_or(0),
                table_bytes: r.number(3).unwrap_or(0),
                index_bytes: r.number(4).unwrap_or(0),
                rows: r.number(5),
            })
            .collect())
    }

    /// SQL Server can only end a whole session (`KILL`); it cannot cancel another session's statement.
    async fn kill(&self, pid: i64, terminate: bool) -> DbResult<bool> {
        if !terminate {
            return Err(DbError::unsupported(
                "SQL Server cannot cancel another session's statement. End the session instead.",
            ));
        }
        if pid == self.spid {
            return Err(DbError::invalid("This is DataQuery's own query connection; use Cancel in the editor."));
        }
        let mut pooled = self.pool.get("").await?;
        text_rows(pooled.conn(), &format!("KILL {pid}"), &[], None).await?;
        Ok(true)
    }
}
