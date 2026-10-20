//! Monitoring (screen 07). Everything reads through catalog connections, so
//! it never waits behind the user's running query.

use dq_core::DbError;
use dq_core::adapter::Session;
use dq_core::monitor::{Monitor, MonitorSnapshot, ObjectSize, ServerSetting};
use tauri::State;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation};
use crate::state::AppState;

fn monitor(session: &dyn Session) -> CmdResult<&dyn Monitor> {
    session.monitor().ok_or_else(|| DbError::unsupported("Monitoring is not available for this engine."))
}

#[tauri::command]
pub async fn monitor_snapshot(
    state: State<'_, AppState>,
    session_id: String,
    long_running_secs: u32,
) -> CmdResult<MonitorSnapshot> {
    super::license::require_pro(&state, "Monitoring").await?;
    let open = state.session(&session_id).await?;
    monitor(&*open.session)?.snapshot(long_running_secs.clamp(1, 86_400)).await
}

#[tauri::command]
pub async fn server_settings(
    state: State<'_, AppState>,
    session_id: String,
    search: Option<String>,
) -> CmdResult<Vec<ServerSetting>> {
    super::license::require_pro(&state, "Monitoring").await?;
    let open = state.session(&session_id).await?;
    let search = search.map(|s| s.trim().chars().take(100).collect::<String>()).filter(|s| !s.is_empty());
    monitor(&*open.session)?.server_settings(search.as_deref()).await
}

#[tauri::command]
pub async fn largest_tables(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    limit: u32,
) -> CmdResult<Vec<ObjectSize>> {
    super::license::require_pro(&state, "Monitoring").await?;
    let open = state.session(&session_id).await?;
    monitor(&*open.session)?.largest_tables(&database, limit).await
}

/// Cancels a process's statement or terminates its session. Always asks first,
/// naming the pid, user and connection; terminating on production also needs
/// the connection name typed.
#[tauri::command]
pub async fn kill_process(
    state: State<'_, AppState>,
    session_id: String,
    pid: i64,
    user: Option<String>,
    terminate: bool,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<bool>> {
    super::license::require_pro(&state, "Monitoring").await?;
    let open = state.session(&session_id).await?;
    let connection = state.connection(&open.connection_id).await?;
    let request = safety::kill_request(&connection, pid, user.as_deref(), terminate);
    if let Some(request) = safety::confirm(request, confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let value = monitor(&*open.session)?.kill(pid, terminate).await?;
    Ok(Guarded::Done { value })
}
