//! The Documents view (screen 13) for document engines. Writes follow the same
//! rules as grid edits: read-only connections refuse them, production asks
//! first, and deleting a document asks like any destructive statement.

use dq_core::document::{DocIndex, DocumentPage, DocumentStore, FindRequest};
use dq_core::node::ObjectRef;
use dq_core::table::RowCount;
use dq_core::{DbError, DbResult};
use tauri::State;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, OpenSession};

/// Largest document text accepted from the editor.
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

fn store(open: &OpenSession) -> DbResult<&dyn DocumentStore> {
    open.session.documents().ok_or_else(|| DbError::unsupported("This connection has no document collections."))
}

#[tauri::command]
pub async fn documents_find(
    state: State<'_, AppState>,
    session_id: String,
    request: FindRequest,
) -> CmdResult<DocumentPage> {
    let open = state.session(&session_id).await?;
    store(&open)?.find(&request).await
}

#[tauri::command]
pub async fn documents_count(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    filter: String,
) -> CmdResult<Option<RowCount>> {
    let open = state.session(&session_id).await?;
    store(&open)?.count(&object, &filter).await
}

#[tauri::command]
pub async fn documents_aggregate(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    pipeline: String,
    limit: u32,
) -> CmdResult<DocumentPage> {
    let open = state.session(&session_id).await?;
    store(&open)?.aggregate(&object, &pipeline, limit).await
}

#[tauri::command]
pub async fn documents_indexes(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
) -> CmdResult<Vec<DocIndex>> {
    let open = state.session(&session_id).await?;
    store(&open)?.indexes(&object).await
}

#[tauri::command]
pub async fn documents_validation(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
) -> CmdResult<Option<String>> {
    let open = state.session(&session_id).await?;
    store(&open)?.validation(&object).await
}

/// The editor statement for a Documents-view request ("Open in editor").
#[tauri::command]
pub async fn documents_statement(
    state: State<'_, AppState>,
    session_id: String,
    request: FindRequest,
) -> CmdResult<String> {
    let open = state.session(&session_id).await?;
    store(&open)?.find_statement(&request)
}

/// One document change from the Documents view.
#[derive(Debug, Clone, serde::Deserialize, ts_rs::TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(export)]
pub enum DocumentChange {
    Insert {
        text: String,
    },
    Replace {
        key: String,
        text: String,
    },
    /// `label` names the document in the confirmation.
    Delete {
        key: String,
        label: String,
    },
}

/// Applies one change. Returns the key of the written document (empty for a delete).
#[tauri::command]
pub async fn documents_write(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    change: DocumentChange,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<String>> {
    if let DocumentChange::Insert { text } | DocumentChange::Replace { text, .. } = &change
        && text.len() > MAX_DOCUMENT_BYTES
    {
        return Err(DbError::invalid("Documents larger than 16 MB cannot be saved from the editor."));
    }
    let open = state.session(&session_id).await?;
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    let intent = match &change {
        DocumentChange::Delete { label, .. } => {
            Intent { writes: true, destructive: vec![format!("Deletes the document {label} from {}.", object.name)] }
        }
        _ => Intent::write(),
    };
    if let Some(request) = safety::check(&connection, &settings, &intent, confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let store = store(&open)?;
    let value = match &change {
        DocumentChange::Insert { text } => store.insert(&object, text).await?,
        DocumentChange::Replace { key, text } => {
            store.replace(&object, key, text).await?;
            key.clone()
        }
        DocumentChange::Delete { key, .. } => {
            store.delete(&object, key).await?;
            String::new()
        }
    };
    Ok(Guarded::Done { value })
}
