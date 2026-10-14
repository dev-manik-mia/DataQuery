use std::sync::Arc;

use dq_core::DbResult;
use dq_core::engine::Capabilities;
use dq_core::result::ServerInfo;
use dq_core::sql::SqlFlavor;
use dq_store::SavedConnection;
use serde::Serialize;
use tauri::State;
use ts_rs::TS;

use super::CmdResult;
use crate::state::{AppState, OpenSession};

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct SessionInfo {
    pub session_id: String,
    pub connection: SavedConnection,
    pub server: ServerInfo,
    pub capabilities: Capabilities,
    pub flavor: SqlFlavor,
}

/// Opens a session for a saved connection and registers it under `session_id`,
/// replacing (and closing) any session already registered there.
async fn open(state: &AppState, connection_id: &str, session_id: String) -> DbResult<SessionInfo> {
    let connection = state.connection(connection_id).await?;
    let adapter = state.registry.get(connection.config.engine)?;
    let password = if connection.has_password { state.password(connection_id).await? } else { None };
    let session = adapter.connect(&connection.config, password.as_deref()).await?;
    let info = SessionInfo {
        session_id: session_id.clone(),
        server: session.info().clone(),
        capabilities: adapter.capabilities(),
        flavor: session.dialect().flavor(),
        connection,
    };
    let replaced = state
        .sessions
        .write()
        .await
        .insert(session_id, Arc::new(OpenSession { connection_id: connection_id.to_owned(), session }));
    if let Some(old) = replaced {
        old.session.close().await;
    }
    let id = connection_id.to_owned();
    state.with_store(move |s| s.touch_connection(&id)).await?;
    Ok(info)
}

#[tauri::command]
pub async fn connect(state: State<'_, AppState>, connection_id: String) -> CmdResult<SessionInfo> {
    open(&state, &connection_id, uuid::Uuid::new_v4().to_string()).await
}

/// Opens fresh connections for an existing session, keeping its id so open
/// tabs stay attached. Unsaved results of the old session are lost.
#[tauri::command]
pub async fn reconnect(state: State<'_, AppState>, session_id: String) -> CmdResult<SessionInfo> {
    let connection_id = state.session(&session_id).await?.connection_id.clone();
    open(&state, &connection_id, session_id).await
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>, session_id: String) -> CmdResult<()> {
    let removed = state.sessions.write().await.remove(&session_id);
    if let Some(open) = removed {
        open.session.close().await;
    }
    Ok(())
}
