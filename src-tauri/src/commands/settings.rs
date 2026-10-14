use rb_store::Settings;
use tauri::State;

use super::CmdResult;
use crate::state::AppState;

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CmdResult<Settings> {
    state.settings().await
}

#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    state.with_store(move |s| s.save_settings(settings)).await
}

/// UI state such as layout and open tabs. SQL text only; never results or secrets.
#[tauri::command]
pub async fn get_ui_state(state: State<'_, AppState>, key: String) -> CmdResult<Option<serde_json::Value>> {
    state.with_store(move |s| s.get_value(&format!("ui:{key}"))).await
}

#[tauri::command]
pub async fn set_ui_state(state: State<'_, AppState>, key: String, value: serde_json::Value) -> CmdResult<()> {
    state.with_store(move |s| s.set_value(&format!("ui:{key}"), &value)).await
}
