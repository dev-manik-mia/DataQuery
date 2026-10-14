use std::path::PathBuf;

use dq_core::node::ObjectRef;
use dq_core::transfer::{CsvOptions, CsvPreview, ExportFormat, ExportReport, ExportSource, preview_csv};
use dq_core::{DbError, ops};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, blocking};

const PREVIEW_ROWS: usize = 20;

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ImportReport {
    #[ts(type = "number")]
    pub rows: u64,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct ColumnMapping {
    pub column: String,
    /// Zero-based CSV field index.
    pub field: u32,
}

/// Paths come from the native file dialog; they must be absolute.
fn file_path(path: &str) -> CmdResult<PathBuf> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DbError::invalid("Choose a file with the file dialog."));
    }
    Ok(path)
}

/// Streams a table or read-only query to CSV or JSON on disk.
#[tauri::command]
pub async fn export_data(
    state: State<'_, AppState>,
    session_id: String,
    source: ExportSource,
    format: ExportFormat,
    path: String,
) -> CmdResult<ExportReport> {
    let path = file_path(&path)?;
    ops::export(&*state.session(&session_id).await?.session, &source, format, &path).await
}

#[tauri::command]
pub async fn csv_preview(path: String, options: CsvOptions) -> CmdResult<CsvPreview> {
    let path = file_path(&path)?;
    blocking(move || preview_csv(&path, &options, PREVIEW_ROWS)).await
}

/// Imports a CSV file into a table in one transaction.
#[tauri::command]
pub async fn import_csv(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    path: String,
    options: CsvOptions,
    mapping: Vec<ColumnMapping>,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<ImportReport>> {
    let path = file_path(&path)?;
    let open = state.session(&session_id).await?;
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) = safety::check(&connection, &settings, &Intent::write(), confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let mapping: Vec<(String, usize)> = mapping.into_iter().map(|m| (m.column, m.field as usize)).collect();
    let started = std::time::Instant::now();
    let rows = ops::import_csv(&*open.session, &object, &path, &options, &mapping).await?;
    Ok(Guarded::Done { value: ImportReport { rows, duration_ms: started.elapsed().as_millis() as u64 } })
}
