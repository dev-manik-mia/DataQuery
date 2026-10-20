//! Data compare between two tables, possibly on different connections and
//! engines. The compare streams both sides, reports progress as
//! `transfer-progress` events and records the changes in a journal file; the
//! sync SQL is rendered from it for review, saved to a file, or applied in one
//! transaction on the target after the usual safety checks.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dq_core::DbError;
use dq_core::data_compare::{
    self, CompareCounts, ComparePlan, CompareSide, DataCompareOptions, DataDiff, Journal, MAX_EXAMPLES, SyncKinds,
    SyncTarget,
};
use dq_core::edit::ApplyReport;
use dq_core::node::{NodeKind, ObjectRef};
use dq_store::SavedConnection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use ts_rs::TS;

use super::transfer::{end_job, file_path, start_job};
use super::{CmdResult, Guarded};
use crate::connector::{self, Opened};
use crate::safety::{self, Confirmation, Intent};
use crate::state::{AppState, blocking};

/// Finished compares kept for their sync SQL; older ones are dropped with their journal.
const MAX_RUNS: usize = 4;
/// Largest sync script returned for display; bigger ones are saved to a file.
const MAX_SQL_TEXT: usize = 2 * 1024 * 1024;

/// One side of a data compare: a saved connection and a table.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CompareTable {
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
    pub table: String,
}

impl CompareTable {
    fn object(&self) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, self.database.clone(), self.schema.clone(), self.table.trim())
    }
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct DataCompareResult {
    /// Names this compare for the sync SQL commands.
    pub compare_id: String,
    pub plan: ComparePlan,
    pub diff: DataDiff,
}

/// A finished compare: its journal and how to write it to the target.
struct CompareRun {
    target: CompareTable,
    sync: SyncTarget,
    journal: PathBuf,
    counts: CompareCounts,
    header: String,
}

impl Drop for CompareRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.journal);
    }
}

/// Finished compares by id, newest last. Managed state of the app.
#[derive(Default)]
pub struct DataCompares(Mutex<Vec<(String, Arc<CompareRun>)>>);

impl DataCompares {
    fn insert(&self, id: String, run: CompareRun) {
        let mut runs = self.0.lock().expect("compares lock");
        runs.push((id, Arc::new(run)));
        let excess = runs.len().saturating_sub(MAX_RUNS);
        runs.drain(..excess);
    }

    fn get(&self, id: &str) -> CmdResult<Arc<CompareRun>> {
        let runs = self.0.lock().expect("compares lock");
        runs.iter()
            .find(|(i, _)| i == id)
            .map(|(_, run)| run.clone())
            .ok_or_else(|| DbError::not_found("This compare is no longer available. Compare again."))
    }

    fn remove(&self, id: &str) {
        self.0.lock().expect("compares lock").retain(|(i, _)| i != id);
    }
}

/// Opens a session for a saved connection; closed by [`close`].
async fn open(state: &AppState, connection_id: &str) -> CmdResult<(SavedConnection, Opened)> {
    let connection = state.connection(connection_id).await?;
    let secrets = connector::saved_secrets(state, &connection).await?;
    let opened = connector::open(state, &connection.config, &secrets).await?;
    Ok((connection, opened))
}

async fn close(opened: Opened) {
    opened.session.close().await;
}

fn check_options(options: &DataCompareOptions) -> CmdResult<()> {
    if options.max_examples > MAX_EXAMPLES {
        return Err(DbError::invalid(format!("Keep at most {MAX_EXAMPLES} differing rows.")));
    }
    if !options.float_tolerance.is_finite() || options.float_tolerance < 0.0 {
        return Err(DbError::invalid("The tolerance must be zero or a positive number."));
    }
    Ok(())
}

/// Opens both sides, runs `work` with them and closes them again.
async fn with_sides<T>(
    state: &AppState,
    source: &CompareTable,
    target: &CompareTable,
    work: impl AsyncFnOnce(&CompareSide, &CompareSide, &SavedConnection, &SavedConnection) -> CmdResult<T>,
) -> CmdResult<T> {
    let ((source_conn, source_opened), (target_conn, target_opened)) =
        tokio::try_join!(open(state, &source.connection_id), open(state, &target.connection_id))?;
    let source_side = CompareSide { session: source_opened.session.clone(), object: source.object() };
    let target_side = CompareSide { session: target_opened.session.clone(), object: target.object() };
    let result = work(&source_side, &target_side, &source_conn, &target_conn).await;
    tokio::join!(close(source_opened), close(target_opened));
    result
}

/// The columns both tables have, the key and what will be compared.
#[tauri::command]
pub async fn data_compare_plan(
    state: State<'_, AppState>,
    source: CompareTable,
    target: CompareTable,
    options: Option<DataCompareOptions>,
) -> CmdResult<ComparePlan> {
    super::license::require_pro(&state, "Data compare").await?;
    let options = options.unwrap_or_default();
    with_sides(&state, &source, &target, async |s, t, _, _| Ok(data_compare::prepare(s, t, &options).await?.0)).await
}

/// Compares the rows of `source` and `target`. Cancel with `cancel_transfer(job_id)`.
#[tauri::command]
pub async fn data_compare(
    app: AppHandle,
    state: State<'_, AppState>,
    compares: State<'_, DataCompares>,
    source: CompareTable,
    target: CompareTable,
    options: DataCompareOptions,
    job_id: String,
) -> CmdResult<DataCompareResult> {
    super::license::require_pro(&state, "Data compare").await?;
    check_options(&options)?;
    let compare_id = uuid::Uuid::new_v4().to_string();
    let journal_path = std::env::temp_dir().join(format!("dataquery-compare-{compare_id}.jsonl"));
    let control = start_job(&app, &state, &job_id);
    let result = with_sides(&state, &source, &target, async |s, t, source_conn, target_conn| {
        let mut journal = Journal::create(&journal_path)?;
        let compared = data_compare::compare(s, t, &options, &mut journal, &control).await?;
        journal.finish()?;
        let d = t.session.dialect();
        let header = format!(
            "Makes {} on {} match {} on {}.\n{} to insert, {} to update, {} to delete. Review before running.",
            d.display_name(&t.object),
            target_conn.name,
            s.session.dialect().display_name(&s.object),
            source_conn.name,
            compared.diff.counts.only_in_source,
            compared.diff.counts.different,
            compared.diff.counts.only_in_target,
        );
        Ok((compared, header))
    })
    .await;
    end_job(&state, &job_id);
    let (compared, header) = match result {
        Ok(done) => done,
        Err(e) => {
            let _ = std::fs::remove_file(&journal_path);
            return Err(e);
        }
    };
    let run = CompareRun { target, sync: compared.target, journal: journal_path, counts: compared.diff.counts, header };
    compares.insert(compare_id.clone(), run);
    Ok(DataCompareResult { compare_id, plan: compared.plan, diff: compared.diff })
}

/// The sync SQL for review, or `None` when it is too large to show (save it to a file instead).
#[tauri::command]
pub async fn data_compare_sql(
    compares: State<'_, DataCompares>,
    compare_id: String,
    kinds: SyncKinds,
) -> CmdResult<Option<String>> {
    let run = compares.get(&compare_id)?;
    blocking(move || {
        let statements = run.sync.statements(|| Journal::read(&run.journal), kinds, true);
        data_compare::sql_text(statements, &run.header, MAX_SQL_TEXT)
    })
    .await
}

/// Streams the sync SQL to a file chosen in the save dialog. Returns the statement count.
#[tauri::command]
pub async fn data_compare_save(
    compares: State<'_, DataCompares>,
    compare_id: String,
    kinds: SyncKinds,
    path: String,
) -> CmdResult<u64> {
    let path = file_path(&path)?;
    let run = compares.get(&compare_id)?;
    blocking(move || {
        let file = std::fs::File::create(&path).map_err(|e| DbError::invalid(format!("Cannot write the file: {e}")))?;
        let mut out = std::io::BufWriter::new(file);
        let statements = run.sync.statements(|| Journal::read(&run.journal), kinds, true);
        data_compare::write_sql(statements, &run.header, &mut out)
    })
    .await
}

/// Applies the sync changes in one transaction on the target, with values as
/// parameters. Read-only connections refuse; production and deletes ask first.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn data_compare_apply(
    app: AppHandle,
    state: State<'_, AppState>,
    compares: State<'_, DataCompares>,
    compare_id: String,
    kinds: SyncKinds,
    job_id: String,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<ApplyReport>> {
    let run = compares.get(&compare_id)?;
    let (connection, settings) = tokio::try_join!(state.connection(&run.target.connection_id), state.settings())?;
    let table = run.sync.table().clone();
    let deletes = if kinds.delete { run.counts.only_in_target } else { 0 };
    let intent = Intent {
        writes: true,
        destructive: if deletes > 0 {
            vec![format!("Deletes {deletes} rows from {}.", table.name)]
        } else {
            Vec::new()
        },
    };
    if let Some(request) = safety::check(&connection, &settings, &intent, confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let (_, opened) = open(&state, &run.target.connection_id).await?;
    let control = start_job(&app, &state, &job_id);
    let statements = run.sync.statements(|| Journal::read(&run.journal), kinds, false);
    let result = data_compare::apply(&*opened.session, &table.database, statements, &control).await;
    end_job(&state, &job_id);
    close(opened).await;
    // Row estimates and sizes of the table changed for open sessions on the target.
    for open in state.sessions.read().await.values().filter(|o| o.connection_id == run.target.connection_id) {
        open.structures.invalidate(Some(&table));
    }
    Ok(Guarded::Done { value: result? })
}

/// Forgets a compare and deletes its journal.
#[tauri::command]
pub async fn data_compare_discard(compares: State<'_, DataCompares>, compare_id: String) -> CmdResult<()> {
    compares.remove(&compare_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_validated() {
        assert!(check_options(&DataCompareOptions::default()).is_ok());
        let many = DataCompareOptions { max_examples: MAX_EXAMPLES + 1, ..DataCompareOptions::default() };
        assert!(check_options(&many).is_err());
        let negative = DataCompareOptions { float_tolerance: -1.0, ..DataCompareOptions::default() };
        assert!(check_options(&negative).is_err());
    }

    #[test]
    fn tables_are_named_by_side() {
        let side = CompareTable {
            connection_id: "c".into(),
            database: "app".into(),
            schema: Some("public".into()),
            table: " items ".into(),
        };
        assert_eq!(side.object(), ObjectRef::new(NodeKind::Table, "app", Some("public".into()), "items"));
    }
}
