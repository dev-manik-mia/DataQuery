//! Engine-neutral operations written once on top of [`Session`] primitives.

use std::path::Path;
use std::time::Instant;

use crate::adapter::Session;
use crate::edit::{ApplyReport, RowChange};
use crate::error::{DbError, DbResult};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::TableStructure;
use crate::sql::builder;
use crate::table::{RowCount, RowFilter, TablePage, TablePageRequest};
use crate::transfer::{self, CsvOptions, ExportFormat, ExportReport, ExportSource};
use crate::value::CellLimits;

pub const MAX_PAGE_SIZE: u32 = 5_000;
/// Above this estimate an unfiltered table shows the estimate instead of counting.
pub const EXACT_COUNT_THRESHOLD: i64 = 500_000;
const IMPORT_BATCH_ROWS: usize = 250;

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
    path: &Path,
) -> DbResult<ExportReport> {
    let (database, sql, params) = match source {
        ExportSource::Rows { columns, rows } => return export_rows(columns, rows, format, path),
        ExportSource::Query { sql } => {
            let statements = crate::sql::split_statements(sql, session.dialect().flavor());
            let [statement] = statements.as_slice() else {
                return Err(DbError::invalid("Export a single statement at a time."));
            };
            if statement.writes {
                return Err(DbError::invalid("Only read-only statements can be exported."));
            }
            (session.info().database.clone(), statement.text(sql).to_owned(), Vec::new())
        }
        ExportSource::Table { object, rows, sort } => {
            let structure = session.structure(object).await?;
            let q = builder::table_select(session.dialect(), &structure, rows, sort.as_ref())?;
            (object.database.clone(), q.sql, q.params)
        }
    };
    let started = Instant::now();
    let mut sink = transfer::file_sink(path, format)?;
    let mut started_sink = false;
    let rows = session
        .stream_query(&database, &sql, &params, &mut |columns, batch| {
            if !started_sink {
                sink.begin(columns)?;
                started_sink = true;
            }
            sink.write(&batch)
        })
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    if !started_sink {
        sink.begin(&[])?;
    }
    let bytes = sink.finish()?;
    Ok(ExportReport { rows, bytes, duration_ms: elapsed_ms(started) })
}

/// Writes rows the UI already holds, through the same sink as streamed exports.
fn export_rows(
    columns: &[crate::result::ColumnMeta],
    rows: &[crate::result::Row],
    format: ExportFormat,
    path: &Path,
) -> DbResult<ExportReport> {
    if rows.iter().flatten().any(|c| !transfer::is_complete(c)) {
        return Err(DbError::invalid(
            "Some selected values were shortened for display. Export the table or query instead to get full values.",
        ));
    }
    let started = Instant::now();
    let mut sink = transfer::file_sink(path, format)?;
    sink.begin(columns)?;
    sink.write(rows)?;
    let bytes = sink.finish()?;
    Ok(ExportReport { rows: rows.len() as u64, bytes, duration_ms: elapsed_ms(started) })
}

/// Imports a CSV file in one transaction. `mapping` pairs table columns with CSV field indexes.
pub async fn import_csv(
    session: &dyn Session,
    object: &ObjectRef,
    path: &Path,
    options: &CsvOptions,
    mapping: &[(String, usize)],
) -> DbResult<u64> {
    if mapping.is_empty() {
        return Err(DbError::invalid("Map at least one column."));
    }
    let structure = session.structure(object).await?;
    if structure.object.kind != NodeKind::Table {
        return Err(DbError::invalid("Rows can only be imported into tables."));
    }
    let columns: Vec<String> = mapping.iter().map(|(c, _)| c.clone()).collect();
    for c in &columns {
        let info = structure.column(c).ok_or_else(|| DbError::invalid(format!("Unknown column `{c}`.")))?;
        if !info.editable {
            return Err(DbError::invalid(format!("Column `{c}` ({}) cannot be imported from text.", info.data_type)));
        }
    }
    let mut rows = transfer::csv_rows(path, options, mapping.iter().map(|(_, i)| *i).collect())?.peekable();
    let dialect = session.dialect();
    let mut first_row = 1usize;
    let mut statements = std::iter::from_fn(|| {
        rows.peek()?;
        let batch: DbResult<Vec<_>> = rows.by_ref().take(IMPORT_BATCH_ROWS).collect();
        let range = first_row;
        let result = batch.and_then(|batch| {
            first_row += batch.len();
            builder::insert_batch(dialect, &structure, &columns, &batch)
        });
        Some(result.map_err(|e| DbError { message: format!("Rows from {range}: {}", e.message), ..e }))
    });
    let counts = session.run_in_transaction(&object.database, &mut statements).await?;
    Ok(counts.iter().sum())
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}
