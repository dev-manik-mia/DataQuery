use std::sync::Arc;
use std::time::Instant;

use dq_core::explain::ExplainResult;
use dq_core::result::{ExecOptions, RowsPage, StatementResult};
use dq_core::sql::dialect::dialect_for;
use dq_core::sql::{
    SqlFlavor, StatementInfo, StatementKind, TxEffect, redact_literals, split_statements, transaction_effect,
};
use dq_core::{DbError, ErrorKind};
use dq_store::{HistoryStatus, NewHistoryEntry, Retention, SavedConnection, Store};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, OpenSession, TxStatus, now_ms};

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
    HistoryBatch::new(state, settings, connection, open).push(sql, duration, result);
}

/// History of one run, written in store transactions on a blocking thread:
/// every [`HistoryBatch::FLUSH_EVERY`] statements, and the rest when dropped,
/// so a run that fails or is abandoned midway still records what ran.
pub struct HistoryBatch {
    /// `None` when history is off.
    store: Option<Arc<Store>>,
    retention: Retention,
    /// Literal redaction (on the blocking thread), when enabled.
    redact_literals: Option<SqlFlavor>,
    connection_id: String,
    connection_name: String,
    database: String,
    entries: Vec<NewHistoryEntry>,
}

impl HistoryBatch {
    /// Statements per write: long scripts show up in history while they run.
    const FLUSH_EVERY: usize = 100;

    pub fn new(
        state: &AppState,
        settings: &dq_store::Settings,
        connection: &SavedConnection,
        open: &OpenSession,
    ) -> Self {
        Self {
            store: settings.keep_history.then(|| state.store.clone()),
            retention: Retention { max_entries: settings.history_max_entries, max_days: settings.history_max_days },
            redact_literals: settings.history_redact_literals.then(|| open.session.dialect().flavor()),
            connection_id: connection.id.clone(),
            connection_name: connection.name.clone(),
            database: open.session.info().database.clone(),
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, sql: &str, duration: std::time::Duration, result: Result<Option<i64>, DbError>) {
        if self.store.is_none() {
            return;
        }
        self.entries.push(NewHistoryEntry {
            connection_id: Some(self.connection_id.clone()),
            connection_name: self.connection_name.clone(),
            database: self.database.clone(),
            sql: sql.to_owned(),
            executed_at: now_ms(),
            duration_ms: duration.as_millis() as i64,
            status: match &result {
                Ok(_) => HistoryStatus::Ok,
                Err(e) if e.kind == ErrorKind::Cancelled => HistoryStatus::Cancelled,
                Err(_) => HistoryStatus::Error,
            },
            rows: result.as_ref().ok().copied().flatten(),
            error: result.err().map(|e| e.message),
        });
        if self.entries.len() >= Self::FLUSH_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let Some(store) = self.store.clone() else { return };
        if self.entries.is_empty() {
            return;
        }
        let mut entries = std::mem::take(&mut self.entries);
        let (retention, redact) = (self.retention, self.redact_literals);
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(flavor) = redact {
                for entry in &mut entries {
                    entry.sql = redact_literals(&entry.sql, flavor);
                }
            }
            if let Err(e) = store.add_history_batch(&entries, retention) {
                tracing::warn!(error = %e, "could not record history");
            }
        });
    }
}

impl Drop for HistoryBatch {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Next transaction state after a statement ran (or failed).
pub(crate) fn next_tx(current: TxStatus, sql: &str, flavor: SqlFlavor, failed: bool) -> TxStatus {
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
/// Engines without transactions (`ClickHouse`, CQL) run every statement on its own.
async fn begin_if_manual(
    state: &AppState,
    open: &OpenSession,
    settings: &dq_store::Settings,
    statements: &[StatementInfo],
) -> CmdResult<()> {
    let only_tx_control = statements.iter().all(|s| s.kind == StatementKind::Transaction);
    let transactions = state.registry.capabilities(open.session.info().engine).is_some_and(|c| c.transactions);
    if settings.manual_commit && transactions && open.tx_status() == TxStatus::Auto && !only_tx_control {
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
    begin_if_manual(&state, &open, &settings, &statements).await?;
    let mut outcomes = Vec::with_capacity(statements.len());
    // Written when the run ends, also if it errors out or is dropped midway.
    let mut history = HistoryBatch::new(&state, &settings, &connection, &open);
    for (index, statement) in statements.iter().enumerate() {
        let text = statement.text(&sql);
        let ran = Instant::now();
        let result = open.session.execute(text, &params, options).await;
        open.set_tx_status(next_tx(open.tx_status(), text, flavor, result.is_err()));
        let rows = result.as_ref().ok().map(|r| r.affected.map_or(r.rows.len() as i64, |a| a as i64));
        history.push(text, ran.elapsed(), result.as_ref().map(|_| rows).map_err(Clone::clone));
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
    // Also when the statement failed: some engines keep part of a failed DDL statement.
    if statements[..outcomes.len()].iter().any(|s| s.kind.may_change_structure()) {
        open.structures.invalidate(None);
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
        (TxAction::Commit, TxStatus::Open) => open.session.dialect().commit_transaction(),
        (TxAction::Rollback, _) => open.session.dialect().rollback_transaction(),
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
    // Schema changes made in the transaction become visible (or are undone).
    if next == TxStatus::Auto {
        open.structures.invalidate(None);
    }
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

/// The editor banner for a statement: CQL single-partition reads vs scans. `None` for other engines.
#[tauri::command]
pub async fn query_analysis(
    state: State<'_, AppState>,
    session_id: String,
    sql: String,
) -> CmdResult<Option<dq_core::sql::cql::QueryAnalysis>> {
    let open = state.session(&session_id).await?;
    if open.session.dialect().flavor() != SqlFlavor::Cql {
        return Ok(None);
    }
    let statements = split_statements(&sql, SqlFlavor::Cql);
    let [statement] = statements.as_slice() else {
        return Ok(None);
    };
    let text = statement.text(&sql);
    let Some(object) = dq_core::sql::cql::select_target(text, &open.session.info().database) else {
        return Ok(None);
    };
    // An unknown table has no keys; the server will say so when the statement runs.
    let keys = open.session.key_roles(&object).await.unwrap_or_default();
    Ok(dq_core::sql::cql::analyze_select(text, object, keys))
}

/// Settings of the query connection shown in the editor toolbar (CQL consistency, page size).
#[tauri::command]
pub async fn session_options(
    state: State<'_, AppState>,
    session_id: String,
) -> CmdResult<Vec<dq_core::adapter::SessionOption>> {
    Ok(state.session(&session_id).await?.session.options())
}

#[tauri::command]
pub async fn set_session_option(
    state: State<'_, AppState>,
    session_id: String,
    name: String,
    value: String,
) -> CmdResult<Vec<dq_core::adapter::SessionOption>> {
    let open = state.session(&session_id).await?;
    open.session.set_option(&name, &value).await?;
    Ok(open.session.options())
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
