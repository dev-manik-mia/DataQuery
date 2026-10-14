use dq_core::node::{Node, ObjectRef};
use dq_core::schema::{Dependencies, SchemaModel};
use dq_core::sql::builder::{delete_template, insert_template, select_template, update_template};
use serde::Deserialize;
use tauri::State;
use ts_rs::TS;

use super::CmdResult;
use crate::state::AppState;

const SEARCH_LIMIT: u32 = 50;

#[tauri::command]
pub async fn list_children(
    state: State<'_, AppState>,
    session_id: String,
    parent: Option<ObjectRef>,
) -> CmdResult<Vec<Node>> {
    state.session(&session_id).await?.session.list_children(parent.as_ref()).await
}

#[tauri::command]
pub async fn search_objects(state: State<'_, AppState>, session_id: String, query: String) -> CmdResult<Vec<Node>> {
    let query = query.trim();
    if query.is_empty() || query.len() > 200 {
        return Ok(Vec::new());
    }
    state.session(&session_id).await?.session.search(query, SEARCH_LIMIT).await
}

/// The object's name quoted for this engine, for "Copy qualified name".
#[tauri::command]
pub async fn qualified_name(state: State<'_, AppState>, session_id: String, object: ObjectRef) -> CmdResult<String> {
    Ok(state.session(&session_id).await?.session.dialect().display_name(&object))
}

#[derive(Debug, Clone, Copy, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TemplateKind {
    Select,
    Insert,
    Update,
    Delete,
}

/// SQL templates for the context menu. They open in an editor tab and never run on their own.
#[tauri::command]
pub async fn generate_sql(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
    kind: TemplateKind,
) -> CmdResult<String> {
    let open = state.session(&session_id).await?;
    let structure = dq_core::ops::structure(&*open.session, &object).await?;
    let d = open.session.dialect();
    Ok(match kind {
        TemplateKind::Select => {
            let columns: Vec<String> = structure.columns.into_iter().map(|c| c.name).collect();
            select_template(d, &object, &columns)
        }
        TemplateKind::Insert => insert_template(d, &structure),
        TemplateKind::Update => update_template(d, &structure),
        TemplateKind::Delete => delete_template(d, &structure),
    })
}

#[tauri::command]
pub async fn object_dependencies(
    state: State<'_, AppState>,
    session_id: String,
    object: ObjectRef,
) -> CmdResult<Dependencies> {
    state.session(&session_id).await?.session.dependencies(&object).await
}

/// Tables, columns and foreign keys, for autocomplete and the ER diagram.
#[tauri::command]
pub async fn schema_model(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    schema: Option<String>,
) -> CmdResult<SchemaModel> {
    state.session(&session_id).await?.session.schema_model(&database, schema.as_deref()).await
}
