use dq_core::edit::{ApplyReport, RowChange};
use dq_core::node::ObjectRef;
use dq_core::ops;
use dq_core::schema::TableStructure;
use dq_core::table::{RowCount, TablePage, TablePageRequest};
use tauri::State;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::AppState;

#[tauri::command]
pub async fn table_structure(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
) -> CmdResult<TableStructure> {
    ops::structure(&*state.session(&session_id).await?.session, &object).await
}

#[tauri::command]
pub async fn table_ddl(state: State<'_, AppState>, session_id: String, object: ObjectRef) -> CmdResult<String> {
    state.session(&session_id).await?.session.ddl(&object).await
}

/// One page of table data, read on a read-only connection.
#[tauri::command]
pub async fn table_page(
    state: State<'_, AppState>,
    session_id: String,
    request: TablePageRequest,
) -> CmdResult<TablePage> {
    ops::table_page(&*state.session(&session_id).await?.session, &request).await
}

#[tauri::command]
pub async fn row_count(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    filter: Option<String>,
) -> CmdResult<RowCount> {
    ops::row_count(&*state.session(&session_id).await?.session, &object, filter.as_deref()).await
}

#[tauri::command]
pub async fn preview_changes(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    changes: Vec<RowChange>,
    full_row: Option<bool>,
) -> CmdResult<String> {
    ops::preview_changes(&*state.session(&session_id).await?.session, &object, &changes, full_row.unwrap_or(false))
        .await
}

/// Saves grid changes in one transaction. Production asks first. `full_row`
/// matches rows of a table without a key on every column (the UI asks first).
#[tauri::command]
pub async fn apply_changes(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    changes: Vec<RowChange>,
    full_row: Option<bool>,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<ApplyReport>> {
    let open = state.session(&session_id).await?;
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) = safety::check(&connection, &settings, &Intent::write(), confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let value = ops::apply_changes(&*open.session, &object, &changes, full_row.unwrap_or(false)).await?;
    Ok(Guarded::Done { value })
}
