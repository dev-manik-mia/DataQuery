use std::time::Instant;

use rb_core::result::{ExecOptions, RowsPage, StatementResult};
use rb_core::sql::split_statements;
use rb_core::{DbError, ErrorKind};
use rb_store::{HistoryStatus, NewHistoryEntry};
use serde::Serialize;
use tauri::State;
use ts_rs::TS;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::AppState;

const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct StatementOutcome {
    pub index: u32,
    /// UTF-16 range of the statement inside the submitted text.
    pub from: u32,
    pub to: u32,
    pub result: Option<StatementResult>,
    pub error: Option<DbError>,
    /// Only the last statement keeps an open cursor.
    pub can_fetch_more: bool,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct RunReport {
    pub outcomes: Vec<StatementOutcome>,
    /// Statements after the first failure were not run.
    pub skipped: u32,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// Runs one statement or a whole script, stopping at the first error.
/// Destructive and production writes need a confirmation first.
#[tauri::command]
pub async fn run_sql(
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<RunReport>> {
    if sql.len() > MAX_SCRIPT_BYTES {
        return Err(DbError::invalid("Scripts larger than 8 MB cannot be run from the editor."));
    }
    let open = state.session(&session_id).await?;
    let statements = split_statements(&sql, open.session.dialect().flavor());
    if statements.is_empty() {
        return Err(DbError::invalid("There is no statement to run."));
    }
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) =
        safety::check(&connection, &settings, &Intent::from_statements(&statements), confirmation.as_ref())?
    {
        return Ok(Guarded::NeedsConfirmation { request });
    }

    let options = ExecOptions { row_limit: settings.row_limit, timeout_secs: settings.timeout_secs };
    let started = Instant::now();
    let mut outcomes = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let text = statement.text(&sql);
        let ran = Instant::now();
        let result = open.session.execute(text, options).await;
        if settings.keep_history {
            let entry = NewHistoryEntry {
                connection_id: Some(connection.id.clone()),
                connection_name: connection.name.clone(),
                database: open.session.info().database.clone(),
                sql: text.to_owned(),
                duration_ms: ran.elapsed().as_millis() as i64,
                status: match &result {
                    Ok(_) => HistoryStatus::Ok,
                    Err(e) if e.kind == ErrorKind::Cancelled => HistoryStatus::Cancelled,
                    Err(_) => HistoryStatus::Error,
                },
                rows: result.as_ref().ok().map(|r| r.affected.map_or(r.rows.len() as i64, |a| a as i64)),
                error: result.as_ref().err().map(|e| e.message.clone()),
            };
            let store = state.store.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = store.add_history(&entry) {
                    tracing::warn!(error = %e, "could not record history");
                }
            });
        }
        let failed = result.is_err();
        let (result, error) = match result {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        };
        outcomes.push(StatementOutcome {
            index: index as u32,
            from: statement.from,
            to: statement.to,
            result,
            error,
            can_fetch_more: false,
        });
        if failed {
            break;
        }
    }
    if let Some(last) = outcomes.last_mut() {
        last.can_fetch_more = last.result.as_ref().is_some_and(|r| r.has_more);
    }
    Ok(Guarded::Done {
        value: RunReport {
            skipped: (statements.len() - outcomes.len()) as u32,
            outcomes,
            duration_ms: started.elapsed().as_millis() as u64,
        },
    })
}

#[tauri::command]
pub async fn fetch_more(state: State<'_, AppState>, session_id: String, limit: u32) -> CmdResult<RowsPage> {
    state.session(&session_id).await?.session.fetch_more(limit).await
}

/// Cancels the running statement from a separate connection.
#[tauri::command]
pub async fn cancel_query(state: State<'_, AppState>, session_id: String) -> CmdResult<bool> {
    state.session(&session_id).await?.session.cancel().await
}
