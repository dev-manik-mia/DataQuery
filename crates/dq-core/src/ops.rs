//! Engine-neutral operations written once on top of [`Session`] primitives.

use std::path::Path;
use std::time::Instant;

use crate::adapter::Session;
use crate::edit::{ApplyReport, RowChange};
use crate::error::{DbError, DbResult};
use crate::node::ObjectRef;
use crate::sql::builder;
use crate::table::{RowCount, TablePage, TablePageRequest};
use crate::transfer::{self, CsvOptions, ExportFormat, ExportReport, ExportSource};
use crate::value::CellLimits;

pub const MAX_PAGE_SIZE: u32 = 5_000;
/// Above this estimate an unfiltered table shows the estimate instead of counting.
pub const EXACT_COUNT_THRESHOLD: i64 = 500_000;
const IMPORT_BATCH_ROWS: usize = 250;

pub async fn table_page(session: &dyn Session, req: &TablePageRequest) -> DbResult<TablePage> {
    if !req.object.kind.is_relation() {
        return Err(DbError::invalid("Only tables and views have data."));
    }
    let limit = req.limit.clamp(1, MAX_PAGE_SIZE);
    let structure = session.structure(&req.object).await?;
    let sql = builder::page_query(
        session.dialect(),
        &structure,
        req.filter.as_deref(),
        req.sort.as_ref(),
        req.offset,
        limit,
    )?;
    let started = Instant::now();
    let (columns, mut rows) = session.read_query(&req.object.database, &sql, CellLimits::GRID).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(TablePage { columns, rows, offset: req.offset, has_more, duration_ms: elapsed_ms(started) })
}

pub async fn row_count(session: &dyn Session, object: &ObjectRef, filter: Option<&str>) -> DbResult<RowCount> {
    let filter = builder::validate_filter(session.dialect(), filter)?;
    if filter.is_none()
        && let Some(estimate) = session.row_estimate(object).await?
        && estimate > EXACT_COUNT_THRESHOLD
    {
        return Ok(RowCount { count: estimate, exact: false });
    }
    let sql = builder::count_query(session.dialect(), object, filter.as_deref())?;
    let (_, rows) = session.read_query(&object.database, &sql, CellLimits::GRID).await?;
    let count = rows
        .first()
        .and_then(|r| r.first())
        .and_then(crate::value::Cell::to_text)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| DbError::internal("count(*) returned no number"))?;
    Ok(RowCount { count, exact: true })
}

pub async fn preview_changes(session: &dyn Session, object: &ObjectRef, changes: &[RowChange]) -> DbResult<String> {
    let structure = session.structure(object).await?;
    builder::preview_changes(session.dialect(), &structure, changes)
}

/// Applies grid changes in one transaction; any failure saves nothing.
pub async fn apply_changes(session: &dyn Session, object: &ObjectRef, changes: &[RowChange]) -> DbResult<ApplyReport> {
    let structure = session.structure(object).await?;
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
    let (database, sql) = match source {
        ExportSource::Query { sql } => {
            let statements = crate::sql::split_statements(sql, session.dialect().flavor());
            let [statement] = statements.as_slice() else {
                return Err(DbError::invalid("Export a single statement at a time."));
            };
            if statement.writes {
                return Err(DbError::invalid("Only read-only statements can be exported."));
            }
            (session.info().database.clone(), statement.text(sql).to_owned())
        }
        ExportSource::Table { object, filter, sort } => {
            let structure = session.structure(object).await?;
            (
                object.database.clone(),
                builder::table_select(session.dialect(), &structure, filter.as_deref(), sort.as_ref())?,
            )
        }
    };
    let started = Instant::now();
    let mut sink = transfer::file_sink(path, format)?;
    let mut started_sink = false;
    let rows = session
        .stream_query(&database, &sql, &mut |columns, batch| {
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
    if structure.object.kind != crate::node::NodeKind::Table {
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
