//! Connection sync through a folder and the scripts folder (see `folder_sync`).

use std::path::PathBuf;

use dq_core::DbError;
use dq_store::Settings;
use dq_store::sync::PASSPHRASE_KEY;
use tauri::{AppHandle, State};

use super::CmdResult;
use crate::folder_sync::{self, FolderSync, ScriptsFolderStatus, SyncStatus};
use crate::state::{AppState, blocking};

/// An existing folder chosen with the folder dialog.
fn folder(path: &str) -> CmdResult<String> {
    let dir = PathBuf::from(path.trim());
    if path.len() > 4096 || path.chars().any(char::is_control) || !dir.is_absolute() || dir.parent().is_none() {
        return Err(DbError::invalid("Choose a folder with the folder dialog."));
    }
    if !dir.is_dir() {
        return Err(DbError::invalid(format!("{} is not a folder.", dir.display())));
    }
    Ok(dir.to_string_lossy().into_owned())
}

async fn update_settings(state: &AppState, change: impl FnOnce(&mut Settings) + Send + 'static) -> CmdResult<Settings> {
    state
        .with_store(move |s| {
            let mut settings = s.settings()?;
            change(&mut settings);
            s.save_settings(settings)
        })
        .await
}

#[tauri::command]
pub async fn sync_status(sync: State<'_, FolderSync>) -> CmdResult<SyncStatus> {
    Ok(sync.sync_status())
}

/// Turns on connection sync with `folder`. A new `passphrase` replaces the
/// one in the keychain; without one, the saved passphrase is kept.
#[tauri::command]
pub async fn sync_configure(
    app: AppHandle,
    state: State<'_, AppState>,
    sync: State<'_, FolderSync>,
    folder: String,
    passphrase: Option<String>,
) -> CmdResult<Settings> {
    let dir = self::folder(&folder)?;
    let secrets = state.secrets.clone();
    match passphrase {
        Some(p) if !(8..=1024).contains(&p.chars().count()) => {
            return Err(DbError::invalid("The sync passphrase must be 8–1024 characters."));
        }
        Some(p) => blocking(move || secrets.set(PASSPHRASE_KEY, &p)).await?,
        None => {
            if blocking(move || secrets.get(PASSPHRASE_KEY)).await?.is_none() {
                return Err(DbError::invalid("Enter the sync passphrase."));
            }
        }
    }
    sync.forget_key();
    let settings = update_settings(&state, move |s| s.sync_folder = dir).await?;
    folder_sync::rewatch(&app).await;
    folder_sync::run_sync(&app).await;
    Ok(settings)
}

/// Turns sync off and removes this device's passphrase. The file stays.
#[tauri::command]
pub async fn sync_disable(
    app: AppHandle,
    state: State<'_, AppState>,
    sync: State<'_, FolderSync>,
) -> CmdResult<Settings> {
    let secrets = state.secrets.clone();
    blocking(move || secrets.delete(PASSPHRASE_KEY)).await?;
    sync.forget_key();
    let settings = update_settings(&state, |s| s.sync_folder.clear()).await?;
    folder_sync::rewatch(&app).await;
    folder_sync::run_sync(&app).await;
    Ok(settings)
}

#[tauri::command]
pub async fn sync_now(app: AppHandle) -> CmdResult<SyncStatus> {
    Ok(folder_sync::run_sync(&app).await)
}

#[tauri::command]
pub async fn scripts_folder_status(sync: State<'_, FolderSync>) -> CmdResult<ScriptsFolderStatus> {
    Ok(sync.scripts_status())
}

/// Mirrors saved scripts into `folder` as `.sql` files, or stops with `None`.
/// Files already in the folder are kept and become scripts.
#[tauri::command]
pub async fn set_scripts_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    folder: Option<String>,
) -> CmdResult<Settings> {
    let dir = folder.as_deref().map(self::folder).transpose()?.unwrap_or_default();
    let off = dir.is_empty();
    let settings = update_settings(&state, move |s| s.scripts_folder = dir).await?;
    if off {
        state.with_store(dq_store::script_files::forget).await?;
    }
    folder_sync::rewatch(&app).await;
    folder_sync::run_scripts(&app).await;
    Ok(settings)
}

#[tauri::command]
pub async fn scripts_folder_sync(app: AppHandle) -> CmdResult<ScriptsFolderStatus> {
    Ok(folder_sync::run_scripts(&app).await)
}
