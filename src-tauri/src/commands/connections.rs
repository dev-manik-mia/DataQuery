use rb_core::DbError;
use rb_core::result::ServerInfo;
use rb_store::{ConnectionDraft, Folder, PasswordChange, SavedConnection};
use serde::Serialize;
use tauri::State;
use ts_rs::TS;

use super::CmdResult;
use crate::state::{AppState, blocking};

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ConnectionList {
    pub connections: Vec<SavedConnection>,
    pub folders: Vec<Folder>,
}

#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> CmdResult<ConnectionList> {
    state.with_store(|s| Ok(ConnectionList { connections: s.connections()?, folders: s.folders()? })).await
}

/// Saves metadata to SQLite and the password to the keychain.
#[tauri::command]
pub async fn save_connection(state: State<'_, AppState>, draft: ConnectionDraft) -> CmdResult<SavedConnection> {
    draft.config.validate()?;
    let existing = match &draft.id {
        Some(id) => Some(state.connection(id).await?),
        None => None,
    };
    let id = draft.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let secrets = state.secrets.clone();
    let has_password = match &draft.password {
        PasswordChange::Keep => existing.is_some_and(|c| c.has_password),
        PasswordChange::Set { value } if !value.is_empty() => {
            let (id, value) = (id.clone(), value.clone());
            blocking(move || secrets.set(&id, &value)).await?;
            true
        }
        PasswordChange::Set { .. } | PasswordChange::Clear => {
            let id = id.clone();
            blocking(move || secrets.delete(&id)).await?;
            false
        }
    };
    let draft = ConnectionDraft { id: Some(id), ..draft };
    state.with_store(move |s| s.save_connection(&draft, has_password)).await
}

#[tauri::command]
pub async fn delete_connection(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let secrets = state.secrets.clone();
    let key = id.clone();
    blocking(move || secrets.delete(&key)).await?;
    state.with_store(move |s| s.delete_connection(&id)).await
}

/// Copies metadata and the stored password under a new id.
#[tauri::command]
pub async fn duplicate_connection(state: State<'_, AppState>, id: String) -> CmdResult<SavedConnection> {
    let source = state.connection(&id).await?;
    let password = if source.has_password { state.password(&id).await? } else { None };
    let draft = ConnectionDraft {
        id: None,
        name: format!("{} copy", source.name),
        folder_id: source.folder_id,
        environment: source.environment,
        favorite: false,
        config: source.config,
        password: password.map_or(PasswordChange::Clear, |value| PasswordChange::Set { value }),
    };
    save_connection(state, draft).await
}

#[tauri::command]
pub async fn set_favorite(state: State<'_, AppState>, id: String, favorite: bool) -> CmdResult<()> {
    state.with_store(move |s| s.set_favorite(&id, favorite)).await
}

#[tauri::command]
pub async fn create_folder(state: State<'_, AppState>, name: String) -> CmdResult<Folder> {
    state.with_store(move |s| s.create_folder(&name)).await
}

#[tauri::command]
pub async fn rename_folder(state: State<'_, AppState>, id: String, name: String) -> CmdResult<()> {
    state.with_store(move |s| s.rename_folder(&id, &name)).await
}

#[tauri::command]
pub async fn delete_folder(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.with_store(move |s| s.delete_folder(&id)).await
}

/// Tests a connection from the form. A `Keep` password reads the saved one.
#[tauri::command]
pub async fn test_connection(state: State<'_, AppState>, draft: ConnectionDraft) -> CmdResult<ServerInfo> {
    let password = match (&draft.password, &draft.id) {
        (PasswordChange::Set { value }, _) => Some(value.clone()).filter(|v| !v.is_empty()),
        (PasswordChange::Keep, Some(id)) => state.password(id).await?,
        _ => None,
    };
    let adapter = state.registry.get(draft.config.engine)?;
    adapter.test(&draft.config, password.as_deref()).await.map_err(|e: DbError| e)
}
