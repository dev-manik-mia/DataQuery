//! Licence status, activation and deactivation (see `crate::license`).

use std::time::Duration;

use dq_core::{DbError, DbResult, ErrorKind};
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::State;

use super::CmdResult;
use crate::license::{self, ACTIVATE_URL, DEACTIVATE_URL, LicenseClaims, LicenseStatus, TOKEN_PREFIX};
use crate::state::AppState;

/// Store key of the signed token (not a secret: it only proves a purchase).
const TOKEN_KEY: &str = "license:token";
/// Keychain id of the Lemon Squeezy licence key, kept to free the seat later.
const KEY_SECRET: &str = "license:key";
const DEVICE_KEY: &str = "license:device";
const MAX_KEY_LEN: usize = 200;

/// The current plan; a stored token that no longer verifies counts as none.
pub async fn current(state: &AppState) -> DbResult<LicenseStatus> {
    let token = state.with_store(|s| s.get_value(TOKEN_KEY)).await?;
    let claims = token.as_ref().and_then(Value::as_str).and_then(|t| license::verify(t).ok());
    Ok(LicenseStatus::new(claims))
}

/// Refuses a Pro-only command on the Free plan.
pub async fn require_pro(state: &AppState, feature: &str) -> DbResult<()> {
    current(state).await?.require_pro(feature)
}

#[tauri::command]
pub async fn license_status(state: State<'_, AppState>) -> CmdResult<LicenseStatus> {
    current(&state).await
}

/// Accepts a signed token (`DQ1.…`, checked offline) or a Lemon Squeezy licence
/// key, which is exchanged once for a token at the activation endpoint.
#[tauri::command]
pub async fn license_activate(state: State<'_, AppState>, key: String) -> CmdResult<LicenseStatus> {
    let key = key.trim().to_owned();
    if key.is_empty() || key.len() > 4096 {
        return Err(DbError::invalid("Paste the licence key from your purchase email."));
    }
    let token = if key.starts_with(TOKEN_PREFIX) {
        key
    } else {
        if key.len() > MAX_KEY_LEN || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(DbError::invalid("That doesn't look like a licence key. It has letters, digits and dashes."));
        }
        let device = device_name(&state).await?;
        let reply = post(ACTIVATE_URL, json!({ "license_key": key, "instance_name": device })).await?;
        let token = reply["token"].as_str().ok_or_else(|| server_error(&reply))?.to_owned();
        let secrets = state.secrets.clone();
        let secret = key.clone();
        crate::state::blocking(move || secrets.set(KEY_SECRET, &secret)).await?;
        token
    };
    license::verify(&token)?;
    state.with_store(move |s| s.set_value(TOKEN_KEY, &Value::String(token))).await?;
    current(&state).await
}

/// Removes the licence from this device and frees its seat when it was activated online.
#[tauri::command]
pub async fn license_deactivate(state: State<'_, AppState>) -> CmdResult<LicenseStatus> {
    let claims: Option<LicenseClaims> = current(&state).await?.claims;
    let secrets = state.secrets.clone();
    let key = crate::state::blocking(move || secrets.get(KEY_SECRET)).await?;
    if let (Some(key), Some(instance)) = (key, claims.and_then(|c| c.instance)) {
        post(DEACTIVATE_URL, json!({ "license_key": key, "instance_id": instance })).await?;
    }
    let secrets = state.secrets.clone();
    crate::state::blocking(move || secrets.delete(KEY_SECRET)).await?;
    state.with_store(|s| s.set_value(TOKEN_KEY, &Value::Null)).await?;
    current(&state).await
}

/// Opens the pricing page in the default browser. The URL is fixed, never taken from the caller.
#[tauri::command]
pub async fn open_pricing() -> CmdResult<()> {
    let (program, args): (&str, &[&str]) = match std::env::consts::OS {
        "macos" => ("open", &[]),
        "windows" => ("explorer", &[]),
        _ => ("xdg-open", &[]),
    };
    std::process::Command::new(program).args(args).arg(license::PRICING_URL).spawn().map(drop).map_err(|e| {
        DbError::new(ErrorKind::Internal, format!("Could not open the browser: {e}. Visit {}", license::PRICING_URL))
    })
}

/// "DataQuery on macOS · 3f2a9c1e": shown in the customer's Lemon Squeezy seat list.
async fn device_name(state: &AppState) -> DbResult<String> {
    let id = state
        .with_store(|s| {
            if let Some(id) = s.get_value(DEVICE_KEY)?.and_then(|v| v.as_str().map(str::to_owned)) {
                return Ok(id);
            }
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
            s.set_value(DEVICE_KEY, &Value::String(id.clone()))?;
            Ok(id)
        })
        .await?;
    Ok(format!("DataQuery on {} · {id}", std::env::consts::OS))
}

#[derive(Deserialize)]
struct ErrorReply {
    error: String,
}

async fn post(url: &str, body: Value) -> DbResult<Value> {
    let network = |e: reqwest::Error| {
        DbError::new(ErrorKind::Network, format!("Could not reach the licence server: {e}"))
            .with_action(Some("Check the internet connection, or paste the DQ1 token from your purchase page instead."))
    };
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().map_err(network)?;
    let response = client.post(url).json(&body).send().await.map_err(network)?;
    let ok = response.status().is_success();
    let reply: Value = response.json().await.map_err(network)?;
    if ok { Ok(reply) } else { Err(server_error(&reply)) }
}

fn server_error(reply: &Value) -> DbError {
    let message = serde_json::from_value::<ErrorReply>(reply.clone()).map_or_else(
        |_| "The licence server sent an unexpected answer.".into(),
        |e| e.error.chars().take(300).collect::<String>(),
    );
    DbError::invalid(message)
}
