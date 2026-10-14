use std::time::Instant;

use dq_core::explain::ExplainResult;
use dq_core::result::{ExecOptions, RowsPage, StatementResult};
use dq_core::sql::dialect::dialect_for;
use dq_core::sql::{
    SqlFlavor, StatementInfo, StatementKind, TxEffect, redact_literals, split_statements, transaction_effect,
};
use dq_core::{DbError, ErrorKind};
use dq_store::{HistoryStatus, NewHistoryEntry, Retention, SavedConnection};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, OpenSession, TxStatus};

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
    /// Transaction state after the run.
    pub tx: TxStatus,
}

/// Records one statement in history (if enabled) on a blocking thread. Result
/// rows are never stored; SQL is redacted by the store.
pub fn record_history(
    state: &AppState,
    settings: &dq_store::Settings,
    connection: &SavedConnection,
    open: &OpenSession,
    sql: &str,
    duration: std::time::Duration,
    result: Result<Option<i64>, DbError>,
) {
    if !settings.keep_history {
        return;
    }
    let flavor = open.session.dialect().flavor();
    let entry = NewHistoryEntry {
        connection_id: Some(connection.id.clone()),
        connection_name: connection.name.clone(),
        database: open.session.info().database.clone(),
        sql: if settings.history_redact_literals { redact_literals(sql, flavor) } else { sql.to_owned() },
        duration_ms: duration.as_millis() as i64,
        status: match &result {
            Ok(_) => HistoryStatus::Ok,
            Err(e) if e.kind == ErrorKind::Cancelled => HistoryStatus::Cancelled,
            Err(_) => HistoryStatus::Error,
        },
        rows: result.as_ref().ok().copied().flatten(),
        error: result.err().map(|e| e.message),
    };
    let store = state.store.clone();
    let retention = Retention { max_entries: settings.history_max_entries, max_days: settings.history_max_days };
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = store.add_history(&entry, retention) {
            tracing::warn!(error = %e, "could not record history");
        }
    });
}

/// Next transaction state after a statement ran (or failed).
fn next_tx(current: TxStatus, sql: &str, flavor: SqlFlavor, failed: bool) -> TxStatus {
    if failed {
        // PostgreSQL and DuckDB abort the whole transaction; the others keep it usable.
        return if current == TxStatus::Open && dialect_for(flavor).aborts_transaction_on_error() {
            TxStatus::Failed
        } else {
            current
        };
    }
    match transaction_effect(sql, flavor) {
        TxEffect::Begin => TxStatus::Open,
        TxEffect::End => TxStatus::Auto,
        TxEffect::RollbackToSavepoint if current == TxStatus::Failed => TxStatus::Open,
        TxEffect::RollbackToSavepoint | TxEffect::None => current,
    }
}

/// Starts a transaction when manual-commit mode is on and none is open.
async fn begin_if_manual(
    open: &OpenSession,
    settings: &dq_store::Settings,
    statements: &[StatementInfo],
) -> CmdResult<()> {
    let only_tx_control = statements.iter().all(|s| s.kind == StatementKind::Transaction);
    if settings.manual_commit && open.tx_status() == TxStatus::Auto && !only_tx_control {
        let options = ExecOptions { row_limit: 1, timeout_secs: settings.timeout_secs };
        open.session.execute(open.session.dialect().begin_transaction(), &[], options).await?;
        open.set_tx_status(TxStatus::Open);
    }
    Ok(())
}

/// Runs one statement or a whole script, stopping at the first error.
/// Destructive and production writes need a confirmation first. `params`
/// bind the placeholders of a single statement.
#[tauri::command]
pub async fn run_sql(
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
    params: Option<Vec<Option<String>>>,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<RunReport>> {
    if sql.len() > MAX_SCRIPT_BYTES {
        return Err(DbError::invalid("Scripts larger than 8 MB cannot be run from the editor."));
    }
    let open = state.session(&session_id).await?;
    let flavor = open.session.dialect().flavor();
    let statements = split_statements(&sql, flavor);
    if statements.is_empty() {
        return Err(DbError::invalid("There is no statement to run."));
    }
    let params = params.unwrap_or_default();
    if !params.is_empty() && statements.len() > 1 {
        return Err(DbError::invalid("Parameters can be given for one statement at a time."));
    }
    if params.len() > 10_000 || params.iter().flatten().map(String::len).sum::<usize>() > MAX_SCRIPT_BYTES {
        return Err(DbError::invalid("Too many or too large parameter values."));
    }
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    if let Some(request) =
        safety::check(&connection, &settings, &Intent::from_statements(&statements), confirmation.as_ref())?
    {
        return Ok(Guarded::NeedsConfirmation { request });
    }

    let options = ExecOptions { row_limit: settings.row_limit, timeout_secs: settings.timeout_secs };
    let started = Instant::now();
    begin_if_manual(&open, &settings, &statements).await?;
    let mut outcomes = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let text = statement.text(&sql);
        let ran = Instant::now();
        let result = open.session.execute(text, &params, options).await;
        open.set_tx_status(next_tx(open.tx_status(), text, flavor, result.is_err()));
        let rows = result.as_ref().ok().map(|r| r.affected.map_or(r.rows.len() as i64, |a| a as i64));
        record_history(
            &state,
            &settings,
            &connection,
            &open,
            text,
            ran.elapsed(),
            result.as_ref().map(|_| rows).map_err(Clone::clone),
        );
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
            tx: open.tx_status(),
        },
    })
}

#[derive(Debug, Clone, Copy, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TxAction {
    Begin,
    Commit,
    Rollback,
}

/// BEGIN / COMMIT / ROLLBACK on the session's query connection.
#[tauri::command]
pub async fn transaction(state: State<'_, AppState>, session_id: String, action: TxAction) -> CmdResult<TxStatus> {
    let open = state.session(&session_id).await?;
    let current = open.tx_status();
    let sql = match (action, current) {
        (TxAction::Begin, TxStatus::Auto) => open.session.dialect().begin_transaction(),
        (TxAction::Begin, _) => return Err(DbError::invalid("A transaction is already open.")),
        (TxAction::Commit, TxStatus::Failed) => {
            return Err(DbError::invalid("The transaction failed; only Rollback is possible."));
        }
        (TxAction::Commit | TxAction::Rollback, TxStatus::Auto) => {
            return Err(DbError::invalid("No transaction is open."));
        }
        (TxAction::Commit, TxStatus::Open) => "COMMIT",
        (TxAction::Rollback, _) => "ROLLBACK",
    };
    let settings = state.settings().await?;
    let result =
        open.session.execute(sql, &[], ExecOptions { row_limit: 1, timeout_secs: settings.timeout_secs }).await;
    // A failed COMMIT or ROLLBACK still ends the transaction on the server.
    let next = match (action, &result) {
        (TxAction::Begin, Ok(_)) => TxStatus::Open,
        (TxAction::Begin, Err(_)) => current,
        _ => TxStatus::Auto,
    };
    open.set_tx_status(next);
    result.map(|_| next)
}

/// Plans a single statement. `analyze` runs it, so only read-only statements may be analyzed.
#[tauri::command]
pub async fn explain(
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
    params: Option<Vec<Option<String>>>,
    analyze: bool,
) -> CmdResult<ExplainResult> {
    let open = state.session(&session_id).await?;
    let flavor = open.session.dialect().flavor();
    let statements = split_statements(&sql, flavor);
    let [statement] = statements.as_slice() else {
        return Err(DbError::invalid("Explain one statement at a time."));
    };
    if analyze && statement.writes {
        return Err(DbError::invalid(
            "EXPLAIN ANALYZE runs the statement. Only read-only statements can be analyzed; use Explain for writes.",
        ));
    }
    let settings = state.settings().await?;
    open.session.explain(statement.text(&sql), &params.unwrap_or_default(), analyze, settings.timeout_secs).await
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
