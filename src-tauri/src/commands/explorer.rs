use rb_core::node::{Node, ObjectRef};
use rb_core::sql::builder::select_template;
use tauri::State;

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

#[tauri::command]
pub async fn generate_select(state: State<'_, AppState>, session_id: String, object: ObjectRef) -> CmdResult<String> {
    let open = state.session(&session_id).await?;
    let structure = open.session.structure(&object).await?;
    let columns: Vec<String> = structure.columns.into_iter().map(|c| c.name).collect();
    Ok(select_template(open.session.dialect(), &object, &columns))
}
