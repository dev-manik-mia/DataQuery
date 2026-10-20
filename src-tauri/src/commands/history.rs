use dq_store::{HistoryPage, HistoryQuery, Retention, SavedScript, ScriptDraft, Snippet, SnippetDraft};
use tauri::{AppHandle, State};

use super::CmdResult;
use crate::folder_sync::scripts_changed;
use crate::state::AppState;

#[tauri::command]
pub async fn list_history(state: State<'_, AppState>, query: HistoryQuery) -> CmdResult<HistoryPage> {
    state.with_store(move |s| s.history(&query)).await
}

/// Clears all history, or one connection's.
#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>, connection_id: Option<String>) -> CmdResult<()> {
    state.with_store(move |s| s.clear_history(connection_id.as_deref())).await
}

/// Prunes history to the retention limits in Settings.
#[tauri::command]
pub async fn apply_history_retention(state: State<'_, AppState>) -> CmdResult<()> {
    let settings = state.settings().await?;
    let retention = Retention { max_entries: settings.history_max_entries, max_days: settings.history_max_days };
    state.with_store(move |s| s.apply_retention(retention)).await
}

#[tauri::command]
pub async fn list_scripts(state: State<'_, AppState>) -> CmdResult<Vec<SavedScript>> {
    state.with_store(dq_store::Store::scripts).await
}

#[tauri::command]
pub async fn save_script(app: AppHandle, state: State<'_, AppState>, draft: ScriptDraft) -> CmdResult<SavedScript> {
    let saved = state.with_store(move |s| s.save_script(&draft)).await?;
    scripts_changed(&app);
    Ok(saved)
}

/// Autosave: writes a linked tab's SQL back to its script. With `base` (the
/// SQL the tab last loaded or saved) it fails with `script_conflict` instead
/// of overwriting a script that changed meanwhile, e.g. in the scripts folder.
#[tauri::command]
pub async fn update_script_sql(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    sql: String,
    base: Option<String>,
) -> CmdResult<()> {
    state.with_store(move |s| s.update_script_sql(&id, &sql, base.as_deref())).await?;
    scripts_changed(&app);
    Ok(())
}

#[tauri::command]
pub async fn delete_script(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.with_store(move |s| s.delete_script(&id)).await?;
    scripts_changed(&app);
    Ok(())
}

#[tauri::command]
pub async fn list_snippets(state: State<'_, AppState>) -> CmdResult<Vec<Snippet>> {
    state.with_store(dq_store::Store::snippets).await
}

#[tauri::command]
pub async fn save_snippet(state: State<'_, AppState>, draft: SnippetDraft) -> CmdResult<Snippet> {
    state.with_store(move |s| s.save_snippet(&draft)).await
}

#[tauri::command]
pub async fn delete_snippet(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.with_store(move |s| s.delete_snippet(&id)).await
}
