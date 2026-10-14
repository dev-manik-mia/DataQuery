//! Commands for the opt-in AI assistant. Nothing here runs SQL.

use dq_core::DbError;
use tauri::State;

use super::CmdResult;
use crate::ai::{self, AiAnswer, AiDisclosure, AiRequest, Prepared};
use crate::state::{AppState, blocking};

async fn prepared(
    state: &AppState,
    session_id: &str,
    request: &AiRequest,
) -> CmdResult<(dq_store::Settings, Prepared)> {
    let settings = state.settings().await?;
    if !settings.ai_enabled {
        return Err(DbError::invalid("The AI assistant is off. Turn it on in Settings → AI assistant."));
    }
    let open = state.session(session_id).await?;
    let info = open.session.info();
    let model = open.session.schema_model(&info.database, None).await?;
    let prepared = ai::prepare(&settings, open.session.dialect().flavor(), &info.version, &model, request)?;
    Ok((settings, prepared))
}

/// What a request would send, for the disclosure panel.
#[tauri::command]
pub async fn ai_disclosure(
    state: State<'_, AppState>,
    session_id: String,
    request: AiRequest,
) -> CmdResult<AiDisclosure> {
    Ok(prepared(&state, &session_id, &request).await?.1.disclosure)
}

#[tauri::command]
pub async fn ai_ask(state: State<'_, AppState>, session_id: String, request: AiRequest) -> CmdResult<AiAnswer> {
    let (settings, prepared) = prepared(&state, &session_id, &request).await?;
    let secrets = state.secrets.clone();
    let key = blocking(move || secrets.get(ai::ANTHROPIC_KEY_ID)).await?;
    ai::ask(&settings, key, prepared).await
}

/// Whether an API key is saved (never the key itself).
#[tauri::command]
pub async fn ai_key_status(state: State<'_, AppState>) -> CmdResult<bool> {
    let secrets = state.secrets.clone();
    Ok(blocking(move || secrets.get(ai::ANTHROPIC_KEY_ID)).await?.is_some())
}

/// Saves the API key in the keychain, or removes it when `key` is empty.
#[tauri::command]
pub async fn ai_set_key(state: State<'_, AppState>, key: Option<String>) -> CmdResult<()> {
    let secrets = state.secrets.clone();
    blocking(move || match key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) {
        Some(k) => secrets.set(ai::ANTHROPIC_KEY_ID, &k),
        None => secrets.delete(ai::ANTHROPIC_KEY_ID),
    })
    .await
}
