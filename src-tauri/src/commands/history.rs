use rb_store::{HistoryPage, HistoryQuery, SavedScript, ScriptDraft};
use tauri::State;

use super::CmdResult;
use crate::state::AppState;

#[tauri::command]
pub async fn list_history(state: State<'_, AppState>, query: HistoryQuery) -> CmdResult<HistoryPage> {
    state.with_store(move |s| s.history(&query)).await
}

#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>) -> CmdResult<()> {
    state.with_store(rb_store::Store::clear_history).await
}

#[tauri::command]
pub async fn list_scripts(state: State<'_, AppState>) -> CmdResult<Vec<SavedScript>> {
    state.with_store(rb_store::Store::scripts).await
}

#[tauri::command]
pub async fn save_script(state: State<'_, AppState>, draft: ScriptDraft) -> CmdResult<SavedScript> {
    state.with_store(move |s| s.save_script(&draft)).await
}

#[tauri::command]
pub async fn delete_script(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.with_store(move |s| s.delete_script(&id)).await
}
