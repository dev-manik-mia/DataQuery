//! Commands for the opt-in AI assistant. Every provider is used with the
//! user's own API key, kept in the keychain per provider. Nothing here runs SQL.

use dq_core::DbError;
use dq_store::AiProvider;
use tauri::State;

use super::CmdResult;
use crate::ai::{self, AiAnswer, AiDisclosure, AiRequest, Prepared};
use crate::ai_client::{self, ProviderInfo};
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

async fn saved_key(state: &AppState, provider: AiProvider) -> CmdResult<Option<String>> {
    let secrets = state.secrets.clone();
    blocking(move || secrets.get(ai_client::key_id(provider))).await
}

/// The supported providers, for the settings page.
#[tauri::command]
pub fn ai_providers() -> Vec<ProviderInfo> {
    ai_client::PROVIDERS.to_vec()
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
    let key = saved_key(&state, settings.ai_provider).await?;
    ai::ask(&settings, key, prepared).await
}

/// Models the saved key can use with the current provider and endpoint; also a key test.
#[tauri::command]
pub async fn ai_models(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let settings = state.settings().await?;
    let key = saved_key(&state, settings.ai_provider).await?;
    ai_client::list_models(&ai_client::Target::new(&settings, key)?).await
}

/// Whether a key is saved for `provider` (never the key itself).
#[tauri::command]
pub async fn ai_key_status(state: State<'_, AppState>, provider: AiProvider) -> CmdResult<bool> {
    Ok(saved_key(&state, provider).await?.is_some())
}

/// Saves `provider`'s API key in the keychain, or removes it when `key` is empty.
#[tauri::command]
pub async fn ai_set_key(state: State<'_, AppState>, provider: AiProvider, key: Option<String>) -> CmdResult<()> {
    let secrets = state.secrets.clone();
    let id = ai_client::key_id(provider);
    blocking(move || match key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) {
        Some(k) => secrets.set(id, &k),
        None => secrets.delete(id),
    })
    .await
}
