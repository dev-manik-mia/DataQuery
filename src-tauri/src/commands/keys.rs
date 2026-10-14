//! The key browser (screen 12) for key-value engines. Scans never use `KEYS`.
//! Writes follow the grid-edit rules (read-only refuses, production asks, a
//! delete asks like any destructive statement); console commands follow the
//! editor's rules (`FLUSHALL`, `DEBUG`, `KEYS` … ask first) and are recorded in history.

use std::time::Instant;

use dq_core::keyvalue::{ConsoleReply, KeyValueStore, KeyWrite, ScanPage, ScanRequest, ValuePage, ValueRequest};
use dq_core::sql::split_statements;
use dq_core::{DbError, DbResult};
use tauri::State;

use super::query::{next_tx, record_history};
use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, OpenSession};

/// Longest console line accepted.
const MAX_LINE_BYTES: usize = 1024 * 1024;

fn store(open: &OpenSession) -> DbResult<&dyn KeyValueStore> {
    open.session.keys().ok_or_else(|| DbError::unsupported("This connection has no key browser."))
}

#[tauri::command]
pub async fn keys_scan(state: State<'_, AppState>, session_id: String, request: ScanRequest) -> CmdResult<ScanPage> {
    let open = state.session(&session_id).await?;
    store(&open)?.scan(&request).await
}

#[tauri::command]
pub async fn keys_count(state: State<'_, AppState>, session_id: String, database: u32) -> CmdResult<u64> {
    let open = state.session(&session_id).await?;
    store(&open)?.key_count(database).await
}

#[tauri::command]
pub async fn keys_value(state: State<'_, AppState>, session_id: String, request: ValueRequest) -> CmdResult<ValuePage> {
    let open = state.session(&session_id).await?;
    store(&open)?.value(&request).await
}

#[tauri::command]
pub async fn keys_write(
    state: State<'_, AppState>,
    session_id: String,
    write: KeyWrite,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<()>> {
    let open = state.session(&session_id).await?;
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    let intent = if write.edit.is_delete() {
        Intent { writes: true, destructive: vec![format!("Deletes the key {} from database {}.", write.key.key.text, write.key.database)] }
    } else {
        Intent::write()
    };
    if let Some(request) = safety::check(&connection, &settings, &intent, confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    store(&open)?.write(&write).await?;
    Ok(Guarded::Done { value: () })
}

/// Runs one console line on the session's query connection, like `redis-cli`.
#[tauri::command]
pub async fn keys_console(
    state: State<'_, AppState>,
    session_id: String,
    line: String,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<ConsoleReply>> {
    if line.len() > MAX_LINE_BYTES {
        return Err(DbError::invalid("Console lines are limited to 1 MB."));
    }
    let open = state.session(&session_id).await?;
    let flavor = open.session.dialect().flavor();
    let statements = split_statements(&line, flavor);
    let [statement] = statements.as_slice() else {
        return Err(DbError::invalid(if statements.is_empty() { "Type a command." } else { "Run one command at a time." }));
    };
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) =
        safety::check(&connection, &settings, &Intent::from_statements(&statements), confirmation.as_ref())?
    {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let text = statement.text(&line);
    let started = Instant::now();
    let reply = store(&open)?.console(text, settings.timeout_secs).await;
    let failed = reply.as_ref().map_or(true, |r| r.error);
    open.set_tx_status(next_tx(open.tx_status(), text, flavor, failed));
    let history = match &reply {
        Ok(r) if r.error => Err(DbError::invalid(r.text.clone())),
        Ok(_) => Ok(None),
        Err(e) => Err(e.clone()),
    };
    record_history(&state, &settings, &connection, &open, text, started.elapsed(), history);
    Ok(Guarded::Done { value: reply? })
}
