use std::path::PathBuf;

use dq_core::result::ServerInfo;
use dq_core::{DbError, ErrorKind};
use dq_store::import::{self, DetectedSource, ForeignPreview, SourceApp, UserDirs};
use dq_store::{ConnectionDraft, ConnectionImport, ConnectionImportReport, Folder, PasswordChange, SavedConnection};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use super::CmdResult;
use crate::connector;
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

/// Applies a secret change in the keychain and returns whether a secret is stored afterwards.
async fn apply_secret(state: &AppState, key: String, change: &PasswordChange, had: bool) -> CmdResult<bool> {
    let secrets = state.secrets.clone();
    match change {
        PasswordChange::Keep => Ok(had),
        PasswordChange::Set { value } if !value.is_empty() => {
            let value = value.clone();
            blocking(move || secrets.set(&key, &value)).await?;
            Ok(true)
        }
        PasswordChange::Set { .. } | PasswordChange::Clear => {
            blocking(move || secrets.delete(&key)).await?;
            Ok(false)
        }
    }
}

/// Saves metadata to SQLite and every secret (database password, SSH
/// passwords and key passphrases) to the keychain.
#[tauri::command]
pub async fn save_connection(state: State<'_, AppState>, draft: ConnectionDraft) -> CmdResult<SavedConnection> {
    draft.config.validate()?;
    let existing = match &draft.id {
        Some(id) => Some(state.connection(id).await?),
        None => None,
    };
    let id = draft.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let had_password = existing.as_ref().is_some_and(|c| c.has_password);
    let has_password = apply_secret(&state, SavedConnection::password_key(&id), &draft.password, had_password).await?;
    let hops = draft.config.ssh.as_ref().map_or(0, |t| t.hops().count());
    let had_ssh = existing.as_ref().map(|c| c.ssh_secrets.clone()).unwrap_or_default();
    let mut ssh_secrets = Vec::with_capacity(hops);
    for hop in 0..hops {
        let change = draft.ssh_secrets.get(hop).unwrap_or(&PasswordChange::Keep);
        let had = had_ssh.get(hop).copied().unwrap_or(false);
        ssh_secrets.push(apply_secret(&state, SavedConnection::ssh_secret_key(&id, hop), change, had).await?);
    }
    // Hops that were removed leave no secrets behind.
    for hop in hops..had_ssh.len() {
        apply_secret(&state, SavedConnection::ssh_secret_key(&id, hop), &PasswordChange::Clear, true).await?;
    }
    let draft = ConnectionDraft { id: Some(id), ..draft };
    state.with_store(move |s| s.save_connection(&draft, has_password, &ssh_secrets)).await
}

#[tauri::command]
pub async fn delete_connection(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let connection = state.connection(&id).await?;
    let secrets = state.secrets.clone();
    let mut keys = vec![SavedConnection::password_key(&id)];
    keys.extend((0..connection.ssh_secrets.len()).map(|hop| SavedConnection::ssh_secret_key(&id, hop)));
    blocking(move || keys.iter().try_for_each(|k| secrets.delete(k))).await?;
    state.with_store(move |s| s.delete_connection(&id)).await
}

/// Copies metadata and every stored secret under a new id.
#[tauri::command]
pub async fn duplicate_connection(state: State<'_, AppState>, id: String) -> CmdResult<SavedConnection> {
    let source = state.connection(&id).await?;
    let secrets = connector::saved_secrets(&state, &source).await?;
    let change = |s: Option<String>| s.map_or(PasswordChange::Clear, |value| PasswordChange::Set { value });
    let draft = ConnectionDraft {
        id: None,
        name: format!("{} copy", source.name),
        folder_id: source.folder_id,
        environment: source.environment,
        favorite: false,
        config: source.config,
        password: change(secrets.password),
        ssh_secrets: secrets.ssh.into_iter().map(change).collect(),
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

/// Tests a connection from the form. `Keep` secrets read the saved ones.
#[tauri::command]
pub async fn test_connection(state: State<'_, AppState>, draft: ConnectionDraft) -> CmdResult<ServerInfo> {
    let secrets = connector::draft_secrets(&state, &draft).await?;
    let opened = connector::open(&state, &draft.config, &secrets).await?;
    let info = opened.session.info().clone();
    opened.session.close().await;
    Ok(info)
}

/// Trusts the SSH host key that `host:port` just offered, if its fingerprint
/// is the one the user was shown.
#[tauri::command]
pub async fn trust_host_key(state: State<'_, AppState>, host: String, port: u16, fingerprint: String) -> CmdResult<()> {
    let known = state.known_hosts.clone();
    blocking(move || known.trust(&host, port, &fingerprint)).await
}

// ---------------------------------------------------------------------------
// Export / import without secrets
// ---------------------------------------------------------------------------

const EXPORT_FORMAT: &str = "dataquery-connections";
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
/// Most connections created by one import.
const MAX_IMPORT_CONNECTIONS: usize = 5000;

#[derive(Debug, Serialize, Deserialize)]
struct ExportFile {
    format: String,
    version: u32,
    connections: Vec<ConnectionImport>,
}

fn absolute(path: &str) -> CmdResult<PathBuf> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DbError::invalid("Choose a file with the file dialog."));
    }
    Ok(path)
}

/// Writes connections and folder names to a JSON file. No secret is included:
/// passwords, SSH secrets and tokens stay in the keychain.
#[tauri::command]
pub async fn export_connections(state: State<'_, AppState>, path: String, ids: Option<Vec<String>>) -> CmdResult<u32> {
    let path = absolute(&path)?;
    state
        .with_store(move |s| {
            let folders = s.folders()?;
            let connections: Vec<ConnectionImport> = s
                .connections()?
                .into_iter()
                .filter(|c| ids.as_ref().is_none_or(|ids| ids.contains(&c.id)))
                .map(|c| ConnectionImport {
                    folder: c.folder_id.and_then(|f| folders.iter().find(|x| x.id == f).map(|x| x.name.clone())),
                    name: c.name,
                    environment: c.environment,
                    favorite: c.favorite,
                    config: c.config,
                })
                .collect();
            let count = connections.len() as u32;
            let file = ExportFile { format: EXPORT_FORMAT.into(), version: 1, connections };
            let json = serde_json::to_vec_pretty(&file).map_err(|e| DbError::internal(e.to_string()))?;
            std::fs::write(&path, json)
                .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot write {}: {e}", path.display())))?;
            Ok(count)
        })
        .await
}

/// Imports connections from an export file as new connections without passwords.
#[tauri::command]
pub async fn import_connections(state: State<'_, AppState>, path: String) -> CmdResult<ConnectionImportReport> {
    let path = absolute(&path)?;
    state
        .with_store(move |s| {
            let io =
                |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot read {}: {e}", path.display()));
            if std::fs::metadata(&path).map_err(io)?.len() > MAX_IMPORT_BYTES {
                return Err(DbError::invalid("The file is too large to be a connection export."));
            }
            let file: ExportFile = serde_json::from_slice(&std::fs::read(&path).map_err(io)?)
                .map_err(|e| DbError::invalid(format!("This is not a DataQuery connection export: {e}")))?;
            if file.format != EXPORT_FORMAT || file.version != 1 {
                return Err(DbError::invalid("This is not a DataQuery connection export (version 1)."));
            }
            s.import_connections(file.connections)
        })
        .await
}

// ---------------------------------------------------------------------------
// Import from other apps (TablePlus, DBeaver, Sequel Ace / Pro, DataGrip)
// ---------------------------------------------------------------------------

/// Connection files of other database clients found at their usual places.
#[tauri::command]
pub async fn detect_connection_sources() -> CmdResult<Vec<DetectedSource>> {
    blocking(|| Ok(import::detect(&UserDirs::current()?))).await
}

/// Reads another app's connections for the preview. Secrets are never read;
/// connections matching a saved one are marked as duplicates.
#[tauri::command]
pub async fn read_connection_source(
    state: State<'_, AppState>,
    app: SourceApp,
    path: String,
) -> CmdResult<ForeignPreview> {
    let path = absolute(&path)?;
    state
        .with_store(move |s| {
            let mut preview = import::read(app, &path, &UserDirs::current()?)?;
            import::mark_duplicates(&mut preview, &s.connections()?);
            Ok(preview)
        })
        .await
}

/// Creates the connections chosen in a preview as new connections without passwords.
#[tauri::command]
pub async fn import_connection_list(
    state: State<'_, AppState>,
    connections: Vec<ConnectionImport>,
) -> CmdResult<ConnectionImportReport> {
    if connections.len() > MAX_IMPORT_CONNECTIONS {
        return Err(DbError::invalid(format!("At most {MAX_IMPORT_CONNECTIONS} connections can be imported at once.")));
    }
    state.with_store(move |s| s.import_connections(connections)).await
}
