//! Backup and restore of a connected database with the engine's dump tools
//! (or `SQLite`'s own backup API). They run as transfer jobs: progress arrives
//! as `transfer-progress` events and `cancel_transfer` stops them.

use dq_core::adapter::ConnectParams;
use dq_core::backup::{self, BackupOptions, BackupPlan, BackupReport, RestoreOptions, ToolStatus};
use dq_ssh::Tunnel;
use dq_store::SavedConnection;
use tauri::{AppHandle, State};

use super::transfer::{end_job, file_path, start_job};
use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, OpenSession};
use crate::{cloud_auth, connector};

/// What a tool needs to reach the session's server: the saved settings, the
/// password from the keychain (or a fresh cloud IAM token) and the local end
/// of the session's SSH tunnel.
async fn tool_params(state: &AppState, open: &OpenSession) -> CmdResult<(SavedConnection, ConnectParams)> {
    let connection = state.connection(&open.connection_id).await?;
    let secrets = connector::saved_secrets(state, &connection).await?;
    let password = cloud_auth::password(&connection.config, secrets.password).await?;
    let params = ConnectParams {
        config: connection.config.clone(),
        password,
        endpoint: open.tunnel.as_ref().map(Tunnel::local_addr),
    };
    Ok((connection, params))
}

/// Every backup tool with the path and version found, for Settings.
#[tauri::command]
pub async fn backup_tools(state: State<'_, AppState>) -> CmdResult<Vec<ToolStatus>> {
    let settings = state.settings().await?;
    Ok(backup::tools::all(&settings.backup_tools).await)
}

/// Formats, tools and version warnings for the session's server.
#[tauri::command]
pub async fn backup_plan(state: State<'_, AppState>, session_id: String) -> CmdResult<BackupPlan> {
    let open = state.session(&session_id).await?;
    let settings = state.settings().await?;
    let info = open.session.info();
    backup::plan(info.engine, &info.version, &settings.backup_tools).await
}

/// Backs up a database to `path` (a file, or a folder for `pg_dump`'s directory format).
#[tauri::command]
pub async fn backup_database(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    options: BackupOptions,
    path: String,
    job_id: String,
) -> CmdResult<BackupReport> {
    let path = file_path(&path)?;
    let open = state.session(&session_id).await?;
    let (settings, (_, params)) = tokio::try_join!(state.settings(), tool_params(&state, &open))?;
    let control = start_job(&app, &state, &job_id);
    let version = &open.session.info().version;
    let result = backup::backup(
        &params,
        version,
        open.session.native_backup(),
        &options,
        &path,
        &settings.backup_tools,
        &control,
    )
    .await;
    end_job(&state, &job_id);
    result
}

/// Restores a backup into a database. Destructive: read-only profiles refuse
/// it and it is confirmed like a destructive statement (typed name on production).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn restore_database(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    options: RestoreOptions,
    path: String,
    job_id: String,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<BackupReport>> {
    let path = file_path(&path)?;
    let open = state.session(&session_id).await?;
    let (settings, (connection, params)) = tokio::try_join!(state.settings(), tool_params(&state, &open))?;
    let file = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let database = if options.database.is_empty() { "main" } else { options.database.as_str() };
    let intent = Intent {
        writes: true,
        destructive: vec![format!(
            "Restore {file} into {database}: objects with the same names are replaced or changed, and this cannot be undone."
        )],
    };
    if let Some(mut request) = safety::check(&connection, &settings, &intent, confirmation.as_ref())? {
        request.title = format!("Restore into {database}?");
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let control = start_job(&app, &state, &job_id);
    let version = &open.session.info().version;
    let result = backup::restore(
        &params,
        version,
        open.session.native_backup(),
        &options,
        &path,
        &settings.backup_tools,
        &control,
    )
    .await;
    end_job(&state, &job_id);
    // Even a failed or cancelled restore may have changed tables.
    open.structures.invalidate(None);
    Ok(Guarded::Done { value: result? })
}
