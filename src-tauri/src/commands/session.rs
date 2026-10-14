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
use crate::connector;
use crate::state::{AppState, OpenSession};

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct SessionInfo {
    pub session_id: String,
    pub connection: SavedConnection,
    pub server: ServerInfo,
    pub capabilities: Capabilities,
    pub flavor: SqlFlavor,
    /// The connection goes through an SSH tunnel.
    pub tunneled: bool,
    /// Why the connection is not fully secure, if it is not.
    pub insecure: Option<String>,
}

/// Opens a session for a saved connection and registers it under `session_id`,
/// replacing (and closing) any session already registered there.
async fn open(state: &AppState, connection_id: &str, session_id: String) -> DbResult<SessionInfo> {
    let connection = state.connection(connection_id).await?;
    let secrets = connector::saved_secrets(state, &connection).await?;
    let opened = connector::open(state, &connection.config, &secrets).await?;
    let adapter = state.registry.get(connection.config.engine)?;
    let info = SessionInfo {
        session_id: session_id.clone(),
        server: opened.session.info().clone(),
        capabilities: adapter.capabilities(),
        flavor: opened.session.dialect().flavor(),
        tunneled: opened.tunnel.is_some(),
        insecure: connection.config.insecure_reason().map(str::to_owned),
        connection,
    };
    let open = OpenSession::new(
        connection_id.to_owned(),
        opened.session,
        opened.tunnel,
        info.connection.config.options.idle_timeout_mins,
    );
    let replaced = state.sessions.write().await.insert(session_id, Arc::new(open));
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

/// Opens fresh connections for a session, keeping its id so open tabs stay
/// attached. Works after the session was closed (idle timeout, network loss).
/// Unsaved results of the old session are lost.
#[tauri::command]
pub async fn reconnect(state: State<'_, AppState>, session_id: String, connection_id: String) -> CmdResult<SessionInfo> {
    let existing = state.sessions.read().await.get(&session_id).map(|s| s.connection_id.clone());
    if existing.as_ref().is_some_and(|id| *id != connection_id) {
        return Err(dq_core::DbError::invalid("This session belongs to another connection."));
    }
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
