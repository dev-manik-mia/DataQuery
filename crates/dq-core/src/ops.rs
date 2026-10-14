//! Engine-neutral operations written once on top of [`Session`] primitives.

use std::path::Path;
use std::time::Instant;

use crate::adapter::Session;
use crate::edit::{ApplyReport, RowChange};
use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::TableStructure;
use crate::sql::builder;
use crate::table::{RowCount, RowFilter, TablePage, TablePageRequest};
use crate::transfer::{
    self, ExportFormat, ExportOptions, ExportReport, ExportSource, FieldRef, ImportOptions, ImportReport, ImportSource,
    RowError, SqlTarget, TransferControl,
};
use crate::value::CellLimits;

pub const MAX_PAGE_SIZE: u32 = 5_000;
/// Above this estimate an unfiltered table shows the estimate instead of counting.
pub const EXACT_COUNT_THRESHOLD: i64 = 500_000;

/// Table structure with [`TableStructure::full_row_key`] filled in.
pub async fn structure(session: &dyn Session, object: &ObjectRef) -> DbResult<TableStructure> {
    let mut s = session.structure(object).await?;
    if s.row_identity.is_empty() && s.object.kind == NodeKind::Table {
        let d = session.dialect();
        s.full_row_key =
            s.columns.iter().filter(|c| c.editable && d.comparable(&c.data_type)).map(|c| c.name.clone()).collect();
    }
    Ok(s)
}

/// Structure used to build edits. `full_row` (confirmed by the user) lets a
/// table without a key be matched on every comparable column.
async fn edit_structure(session: &dyn Session, object: &ObjectRef, full_row: bool) -> DbResult<TableStructure> {
    let mut s = structure(session, object).await?;
    if full_row && s.row_identity.is_empty() {
        s.row_identity = std::mem::take(&mut s.full_row_key);
    }
    Ok(s)
}

pub async fn table_page(session: &dyn Session, req: &TablePageRequest) -> DbResult<TablePage> {
    if !req.object.kind.is_relation() {
        return Err(DbError::invalid("Only tables and views have data."));
    }
    let limit = req.limit.clamp(1, MAX_PAGE_SIZE);
    let structure = session.structure(&req.object).await?;
    let q = builder::page_query(session.dialect(), &structure, &req.rows, req.sort.as_ref(), req.offset, limit)?;
    let started = Instant::now();
    let (columns, mut rows) = session.read_query(&req.object.database, &q.sql, &q.params, CellLimits::GRID).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(TablePage { columns, rows, offset: req.offset, has_more, duration_ms: elapsed_ms(started) })
}

pub async fn row_count(session: &dyn Session, object: &ObjectRef, filter: &RowFilter) -> DbResult<RowCount> {
    let unfiltered = builder::validate_filter(session.dialect(), filter.filter.as_deref())?.is_none()
        && filter.matches.is_empty()
        && filter.searches.iter().all(|s| s.contains.is_empty());
    if unfiltered
        && let Some(estimate) = session.row_estimate(object).await?
        && estimate > EXACT_COUNT_THRESHOLD
    {
        return Ok(RowCount { count: estimate, exact: false });
    }
    let structure = session.structure(object).await?;
    let q = builder::count_query(session.dialect(), &structure, filter)?;
    let (_, rows) = session.read_query(&object.database, &q.sql, &q.params, CellLimits::GRID).await?;
    let count = rows
        .first()
        .and_then(|r| r.first())
        .and_then(crate::value::Cell::to_text)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| DbError::internal("count(*) returned no number"))?;
    Ok(RowCount { count, exact: true })
}

/// The full value of one cell, matched by the row key. Size-limited by `limits`.
pub async fn cell_value(
    session: &dyn Session,
    object: &ObjectRef,
    key: &[crate::edit::ColumnValue],
    column: &str,
    limits: CellLimits,
) -> DbResult<crate::value::Cell> {
    let structure = session.structure(object).await?;
    let q = builder::cell_query(session.dialect(), &structure, key, column)?;
    let (_, mut rows) = session.read_query(&object.database, &q.sql, &q.params, limits).await?;
    if rows.len() != 1 {
        return Err(DbError::not_found("The row changed or is no longer unique. Reload the data."));
    }
    rows.pop().and_then(|mut r| r.pop()).ok_or_else(|| DbError::internal("empty cell"))
}

pub async fn preview_changes(
    session: &dyn Session,
    object: &ObjectRef,
    changes: &[RowChange],
    full_row: bool,
) -> DbResult<String> {
    let structure = edit_structure(session, object, full_row).await?;
    builder::preview_changes(session.dialect(), &structure, changes)
}

/// Applies grid changes in one transaction; any failure saves nothing.
pub async fn apply_changes(
    session: &dyn Session,
    object: &ObjectRef,
    changes: &[RowChange],
    full_row: bool,
) -> DbResult<ApplyReport> {
    let structure = edit_structure(session, object, full_row).await?;
    let statements = builder::change_statements(session.dialect(), &structure, changes)?;
    let started = Instant::now();
    session.run_in_transaction(&object.database, &mut statements.into_iter().map(Ok)).await?;
    let mut report = ApplyReport { duration_ms: elapsed_ms(started), ..ApplyReport::default() };
    for change in changes {
        match change {
            RowChange::Insert { .. } => report.inserted += 1,
            RowChange::Update { .. } => report.updated += 1,
            RowChange::Delete { .. } => report.deleted += 1,
        }
    }
    Ok(report)
}

pub async fn export(
    session: &dyn Session,
    source: &ExportSource,
    format: ExportFormat,
    options: &ExportOptions,
    path: &Path,
    control: &TransferControl,
) -> DbResult<ExportReport> {
    let fallback_table = ObjectRef::new(NodeKind::Table, "", None, "exported_rows");
    let (database, sql, params, table) = match source {
        ExportSource::Rows { columns, rows } => {
            let target = SqlTarget { dialect: session.dialect(), table: &fallback_table };
            return export_rows(columns, rows, format, options, target, path);
        }
        ExportSource::Query { sql } => {
            let statements = crate::sql::split_statements(sql, session.dialect().flavor());
            let [statement] = statements.as_slice() else {
                return Err(DbError::invalid("Export a single statement at a time."));
            };
            if statement.writes {
                return Err(DbError::invalid("Only read-only statements can be exported."));
            }
            (session.info().database.clone(), statement.text(sql).to_owned(), Vec::new(), fallback_table.clone())
        }
        ExportSource::Table { object, rows, sort } => {
            let structure = session.structure(object).await?;
            let q = builder::table_select(session.dialect(), &structure, rows, sort.as_ref())?;
            (object.database.clone(), q.sql, q.params, object.clone())
        }
    };
    let started = Instant::now();
    let target = SqlTarget { dialect: session.dialect(), table: &table };
    let mut sink = transfer::file_sink(path, format, options, Some(target))?;
    let mut started_sink = false;
    let mut total = 0u64;
    let rows = session
        .stream_query(&database, &sql, &params, &mut |columns, batch| {
            control.check()?;
            if !started_sink {
                sink.begin(columns)?;
                started_sink = true;
            }
            total += batch.len() as u64;
            sink.write(&batch)?;
            control.progress(total, sink.bytes());
            Ok(())
        })
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            drop(sink);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    if !started_sink {
        sink.begin(&[])?;
    }
    let bytes = sink.finish()?;
    control.progress(rows, bytes);
    Ok(ExportReport { rows, bytes, duration_ms: elapsed_ms(started) })
}

/// Writes rows the UI already holds, through the same sinks as streamed exports.
fn export_rows(
    columns: &[crate::result::ColumnMeta],
    rows: &[crate::result::Row],
    format: ExportFormat,
    options: &ExportOptions,
    target: SqlTarget<'_>,
    path: &Path,
) -> DbResult<ExportReport> {
    if rows.iter().flatten().any(|c| !transfer::is_complete(c)) {
        return Err(DbError::invalid(
            "Some selected values were shortened for display. Export the table or query instead to get full values.",
        ));
    }
    let started = Instant::now();
    let mut sink = transfer::file_sink(path, format, options, Some(target))?;
    sink.begin(columns)?;
    sink.write(rows)?;
    let bytes = sink.finish()?;
    Ok(ExportReport { rows: rows.len() as u64, bytes, duration_ms: elapsed_ms(started) })
}

/// A target column and where its value comes from in the source file.
#[derive(Debug, Clone)]
pub struct ImportMapping {
    pub column: String,
    pub field: FieldRef,
}

/// Validates every row first (reporting each bad row), then inserts in
/// batches. `commit_rows` splits the import into several transactions; the
/// report says how many rows were committed when a later one fails.
pub async fn import(
    session: &dyn Session,
    object: &ObjectRef,
    source: &ImportSource,
    mapping: &[ImportMapping],
    options: &ImportOptions,
    control: &TransferControl,
) -> DbResult<ImportReport> {
    let started = Instant::now();
    if mapping.is_empty() {
        return Err(DbError::invalid("Map at least one column."));
    }
    let structure = session.structure(object).await?;
    if structure.object.kind != NodeKind::Table {
        return Err(DbError::invalid("Rows can only be imported into tables."));
    }
    let columns: Vec<String> = mapping.iter().map(|m| m.column.clone()).collect();
    let mut infos = Vec::with_capacity(columns.len());
    for c in &columns {
        let info = structure.column(c).ok_or_else(|| DbError::invalid(format!("Unknown column `{c}`.")))?;
        if !info.editable {
            return Err(DbError::invalid(format!("Column `{c}` ({}) cannot be imported from text.", info.data_type)));
        }
        infos.push(info.clone());
    }
    let fields: Vec<FieldRef> = mapping.iter().map(|m| m.field.clone()).collect();
    let mut report = ImportReport::default();

    // Pass 1: validate every row without writing.
    let mut invalid_rows = Vec::new();
    for (i, row) in transfer::rows(source, fields.clone())?.enumerate() {
        if i % 1000 == 0 {
            control.check()?;
            control.progress(0, i as u64);
        }
        let number = i as u64 + 1;
        let problem = match row {
            Err(e) => Some((None, e.message)),
            Ok(values) => infos.iter().zip(&values).find_map(|(info, value)| {
                transfer::validate_value(info, value.as_deref())
                    .err()
                    .map(|m| (Some(info.name.clone()), format!("{} {m}", info.name)))
            }),
        };
        if let Some((column, message)) = problem {
            report.error_count += 1;
            invalid_rows.push(number);
            if report.errors.len() < transfer::MAX_REPORTED_ERRORS {
                report.errors.push(RowError { row: number, column, message });
            }
        }
    }
    if report.error_count > 0 && !options.skip_invalid {
        report.stopped = true;
        report.message = Some("Nothing was imported. Fix the rows below, or choose to skip invalid rows.".into());
        report.duration_ms = elapsed_ms(started);
        return Ok(report);
    }
    report.skipped = invalid_rows.len() as u64;

    // Pass 2: insert the valid rows.
    let dialect = session.dialect();
    let batch_rows = options.batch_rows.clamp(1, 5_000) as usize;
    let commit_rows = if options.commit_rows == 0 { usize::MAX } else { options.commit_rows as usize };
    let mut skip = invalid_rows.into_iter().peekable();
    let mut rows = transfer::rows(source, fields)?
        .enumerate()
        .filter(|(i, _)| {
            let number = *i as u64 + 1;
            while skip.peek().is_some_and(|s| *s < number) {
                skip.next();
            }
            skip.peek() != Some(&number)
        })
        .peekable();
    let mut done = 0u64;
    while rows.peek().is_some() {
        let mut in_tx = 0usize;
        let mut statements = std::iter::from_fn(|| {
            if in_tx >= commit_rows || rows.peek().is_none() {
                return None;
            }
            if let Err(e) = control.check() {
                return Some(Err(e));
            }
            let take = batch_rows.min(commit_rows - in_tx);
            let mut batch = Vec::with_capacity(take);
            let first = rows.peek().map_or(0, |(i, _)| *i as u64 + 1);
            for (_, row) in rows.by_ref().take(take) {
                match row {
                    Ok(values) => batch.push(values),
                    Err(e) => return Some(Err(e)),
                }
            }
            in_tx += batch.len();
            control.progress(done + in_tx as u64, 0);
            Some(
                builder::insert_batch(dialect, &structure, &columns, &batch)
                    .map_err(|e| DbError { message: format!("Rows from {first}: {}", e.message), ..e }),
            )
        });
        match session.run_in_transaction(&object.database, &mut statements).await {
            Ok(counts) => {
                done += counts.iter().sum::<u64>();
                report.rows = done;
            }
            Err(e) if report.rows > 0 || e.kind == ErrorKind::Cancelled => {
                report.stopped = true;
                report.message = Some(format!("{} {} rows were committed before this.", e.message, report.rows));
                break;
            }
            Err(e) => return Err(e),
        }
    }
    report.duration_ms = elapsed_ms(started);
    Ok(report)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}
