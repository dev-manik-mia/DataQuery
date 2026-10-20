//! Export and import. Both stream, report progress as `transfer-progress`
//! events and can be cancelled by job id.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use dq_core::DbError;
use dq_core::node::ObjectRef;
use dq_core::ops::{self, ImportMapping};
use dq_core::transfer::{
    ExportFormat, ExportOptions, ExportReport, ExportSource, FieldRef, ImportOptions, ImportPreview, ImportReport,
    ImportSource, TransferControl, preview,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use ts_rs::TS;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, blocking};

const PREVIEW_ROWS: usize = 20;
const PROGRESS_EVENT: &str = "transfer-progress";

#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct ColumnMapping {
    pub column: String,
    /// Zero-based CSV field index.
    pub field: Option<u32>,
    /// JSON key.
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct TransferProgress {
    pub job_id: String,
    #[ts(type = "number")]
    pub rows: u64,
    #[ts(type = "number")]
    pub bytes: u64,
}

/// Paths come from the native file dialog; they must be absolute.
pub(super) fn file_path(path: &str) -> CmdResult<PathBuf> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DbError::invalid("Choose a file with the file dialog."));
    }
    Ok(path)
}

/// Registers a job: its cancel flag and a progress callback that emits at most every 200 ms.
pub(super) fn start_job(app: &AppHandle, state: &AppState, job_id: &str) -> TransferControl {
    let cancelled = Arc::new(AtomicBool::new(false));
    state.transfers.lock().expect("transfers lock").insert(job_id.to_owned(), cancelled.clone());
    let app = app.clone();
    let job = job_id.to_owned();
    let last = std::sync::Mutex::new(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
    TransferControl::new(cancelled, move |rows, bytes| {
        let mut last = last.lock().expect("progress lock");
        if last.elapsed() >= Duration::from_millis(200) {
            *last = Instant::now();
            let _ = app.emit(PROGRESS_EVENT, TransferProgress { job_id: job.clone(), rows, bytes });
        }
    })
}

pub(super) fn end_job(state: &AppState, job_id: &str) {
    state.transfers.lock().expect("transfers lock").remove(job_id);
}

/// Streams a table, a read-only query or grid rows to CSV, JSON, XLSX or SQL on disk.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn export_data(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    source: ExportSource,
    format: ExportFormat,
    options: Option<ExportOptions>,
    path: String,
    job_id: String,
) -> CmdResult<ExportReport> {
    let path = file_path(&path)?;
    let open = state.session(&session_id).await?;
    let control = start_job(&app, &state, &job_id);
    let result = ops::export(open.ops(), &source, format, &options.unwrap_or_default(), &path, &control).await;
    end_job(&state, &job_id);
    result
}

#[tauri::command]
pub async fn import_preview(source: ImportSource) -> CmdResult<ImportPreview> {
    file_path(source.path())?;
    blocking(move || preview(&source, PREVIEW_ROWS)).await
}

/// Validates, then imports a CSV or JSON file into a table. Production asks first.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn import_data(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    source: ImportSource,
    mapping: Vec<ColumnMapping>,
    options: Option<ImportOptions>,
    job_id: String,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<ImportReport>> {
    file_path(source.path())?;
    let open = state.session(&session_id).await?;
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) = safety::check(&connection, &settings, &Intent::write(), confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let mapping = mapping
        .into_iter()
        .map(|m| {
            let field = match (m.field, m.key) {
                (Some(i), _) => FieldRef::Index(i as usize),
                (None, Some(k)) => FieldRef::Key(k),
                (None, None) => return Err(DbError::invalid(format!("Choose a source field for `{}`.", m.column))),
            };
            Ok(ImportMapping { column: m.column, field })
        })
        .collect::<CmdResult<Vec<_>>>()?;
    let control = start_job(&app, &state, &job_id);
    let result = ops::import(open.ops(), &object, &source, &mapping, &options.unwrap_or_default(), &control).await;
    end_job(&state, &job_id);
    Ok(Guarded::Done { value: result? })
}

/// Cancels a running export or import. Imports roll back the open transaction.
#[tauri::command]
pub async fn cancel_transfer(state: State<'_, AppState>, job_id: String) -> CmdResult<bool> {
    let flag = state.transfers.lock().expect("transfers lock").get(&job_id).cloned();
    Ok(flag.is_some_and(|f| {
        f.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }))
}
