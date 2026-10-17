use dq_core::edit::{ApplyReport, ColumnValue, RowChange};
use dq_core::node::ObjectRef;
use dq_core::ops;
use dq_core::schema::TableStructure;
use dq_core::table::{RowCount, RowFilter, TablePage, TablePageRequest};
use dq_core::value::{Cell, CellLimits};
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
    // Always read fresh (the structure tab and edit forms), and refresh the cache with it.
    let open = state.session(&session_id).await?;
    let structure = ops::structure(&*open.session, &object).await?;
    open.structures.insert(&object, structure.clone());
    Ok(structure)
}

/// Partition and clustering columns (CQL), for the `PK` / `CK` header markers.
#[tauri::command]
pub async fn key_roles(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
) -> CmdResult<Vec<dq_core::schema::KeyRole>> {
    state.session(&session_id).await?.session.key_roles(&object).await
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
    ops::table_page(state.session(&session_id).await?.ops(), &request).await
}

#[tauri::command]
pub async fn row_count(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    rows: RowFilter,
) -> CmdResult<RowCount> {
    ops::row_count(state.session(&session_id).await?.ops(), &object, &rows).await
}

#[tauri::command]
pub async fn preview_changes(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    changes: Vec<RowChange>,
    full_row: Option<bool>,
) -> CmdResult<String> {
    ops::preview_changes(state.session(&session_id).await?.ops(), &object, &changes, full_row.unwrap_or(false)).await
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
    let value = ops::apply_changes(open.ops(), &object, &changes, full_row.unwrap_or(false)).await?;
    Ok(Guarded::Done { value })
}

/// Largest value shown in the cell viewer; bigger ones can be saved to a file.
const VIEWER_LIMITS: CellLimits = CellLimits { max_text_bytes: 4 * 1024 * 1024, binary_head_bytes: 64 * 1024 };

/// A cell's value for the viewer: text up to 4 MB, binary up to its first 64 KB.
#[tauri::command]
pub async fn cell_value(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    key: Vec<ColumnValue>,
    column: String,
) -> CmdResult<Cell> {
    ops::cell_value(state.session(&session_id).await?.ops(), &object, &key, &column, VIEWER_LIMITS).await
}

/// Writes a cell's complete value (binary as bytes, text as UTF-8) to a file chosen in the save dialog.
#[tauri::command]
pub async fn save_cell(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    key: Vec<ColumnValue>,
    column: String,
    path: String,
) -> CmdResult<u64> {
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() {
        return Err(dq_core::DbError::invalid("Choose a file with the file dialog."));
    }
    let cell =
        ops::cell_value(state.session(&session_id).await?.ops(), &object, &key, &column, CellLimits::FULL).await?;
    let bytes = match &cell {
        Cell::Binary { head_hex, .. } => (0..head_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&head_hex[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| dq_core::DbError::internal("invalid hex"))?,
        other => other.to_text().unwrap_or_default().into_bytes(),
    };
    let len = bytes.len() as u64;
    crate::state::blocking(move || {
        std::fs::write(&path, bytes).map_err(|e| {
            dq_core::DbError::new(dq_core::ErrorKind::Storage, format!("Cannot write {}: {e}", path.display()))
        })
    })
    .await?;
    Ok(len)
}
