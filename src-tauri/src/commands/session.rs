use std::sync::Arc;

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

#[tauri::command]
pub async fn connect(state: State<'_, AppState>, connection_id: String) -> CmdResult<SessionInfo> {
    let connection = state.connection(&connection_id).await?;
    let adapter = state.registry.get(connection.config.engine)?;
    let password = if connection.has_password { state.password(&connection_id).await? } else { None };
    let session = adapter.connect(&connection.config, password.as_deref()).await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let info = SessionInfo {
        session_id: session_id.clone(),
        server: session.info().clone(),
        capabilities: adapter.capabilities(),
        flavor: session.dialect().flavor(),
        connection: connection.clone(),
    };
    state
        .sessions
        .write()
        .await
        .insert(session_id, Arc::new(OpenSession { connection_id: connection_id.clone(), session }));
    state.with_store(move |s| s.touch_connection(&connection_id)).await?;
    Ok(info)
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>, session_id: String) -> CmdResult<()> {
    let removed = state.sessions.write().await.remove(&session_id);
    if let Some(open) = removed {
        open.session.close().await;
    }
    Ok(())
}
