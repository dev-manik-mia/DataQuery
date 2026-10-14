//! Framework migration tables (Prisma, Laravel, Django, Rails, Flyway, Knex,
//! Alembic, TypeORM, Sequelize, golang-migrate). Detected from the schema
//! model, read with a bounded `SELECT` on a read-only connection.

use serde::Serialize;
use ts_rs::TS;

use crate::adapter::Session;
use crate::error::DbResult;
use crate::node::ObjectRef;
use crate::schema::{ModelTable, SchemaModel};
use crate::value::CellLimits;

const MAX_ROWS: u32 = 500;

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct MigrationEntry {
    /// Version, id or migration name, whichever the framework keys on.
    pub version: String,
    pub name: Option<String>,
    pub applied_at: Option<String>,
    /// `applied`, `failed`, `rolled back`, `pending` or `dirty`.
    pub status: String,
    pub batch: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct FrameworkMigrations {
    pub framework: String,
    pub table: ObjectRef,
    /// Newest first, at most 500.
    pub entries: Vec<MigrationEntry>,
}

/// Which framework a table belongs to, judged by its name and columns.
struct Detector {
    framework: &'static str,
    table: &'static str,
    /// Columns that must exist.
    columns: &'static [&'static str],
    /// `SELECT` list: version, name, applied_at, status, batch (column names or `NULL`).
    select: [&'static str; 5],
    order_by: &'static str,
}

const DETECTORS: &[Detector] = &[
    Detector {
        framework: "Prisma",
        table: "_prisma_migrations",
        columns: &["migration_name", "finished_at"],
        select: ["id", "migration_name", "finished_at", "CASE WHEN rolled_back_at IS NOT NULL THEN 'rolled back' WHEN finished_at IS NULL THEN 'failed' ELSE 'applied' END", "NULL"],
        order_by: "started_at DESC",
    },
    Detector {
        framework: "Django",
        table: "django_migrations",
        columns: &["app", "name", "applied"],
        select: ["name", "app", "applied", "'applied'", "NULL"],
        order_by: "id DESC",
    },
    Detector {
        framework: "Laravel",
        table: "migrations",
        columns: &["migration", "batch"],
        select: ["migration", "NULL", "NULL", "'applied'", "batch"],
        order_by: "id DESC",
    },
    Detector {
        framework: "TypeORM",
        table: "migrations",
        columns: &["timestamp", "name"],
        select: ["timestamp", "name", "NULL", "'applied'", "NULL"],
        order_by: "id DESC",
    },
    Detector {
        framework: "golang-migrate",
        table: "schema_migrations",
        columns: &["version", "dirty"],
        select: ["version", "NULL", "NULL", "CASE WHEN dirty THEN 'dirty' ELSE 'applied' END", "NULL"],
        order_by: "version DESC",
    },
    Detector {
        framework: "Rails",
        table: "schema_migrations",
        columns: &["version"],
        select: ["version", "NULL", "NULL", "'applied'", "NULL"],
        order_by: "version DESC",
    },
    Detector {
        framework: "Flyway",
        table: "flyway_schema_history",
        columns: &["installed_rank", "version", "success"],
        select: ["coalesce(version, script)", "description", "installed_on", "CASE WHEN success THEN 'applied' ELSE 'failed' END", "installed_rank"],
        order_by: "installed_rank DESC",
    },
    Detector {
        framework: "Knex",
        table: "knex_migrations",
        columns: &["name", "batch"],
        select: ["name", "NULL", "migration_time", "'applied'", "batch"],
        order_by: "id DESC",
    },
    Detector {
        framework: "Alembic",
        table: "alembic_version",
        columns: &["version_num"],
        select: ["version_num", "NULL", "NULL", "'current'", "NULL"],
        order_by: "version_num DESC",
    },
    Detector {
        framework: "Sequelize",
        table: "SequelizeMeta",
        columns: &["name"],
        select: ["name", "NULL", "NULL", "'applied'", "NULL"],
        order_by: "name DESC",
    },
];

fn matches(t: &ModelTable, d: &Detector) -> bool {
    t.object.name.eq_ignore_ascii_case(d.table)
        && d.columns.iter().all(|c| t.columns.iter().any(|tc| tc.name.eq_ignore_ascii_case(c)))
}

/// Finds migration tables in `model` and reads their latest entries.
pub async fn inspect(session: &dyn Session, model: &SchemaModel) -> DbResult<Vec<FrameworkMigrations>> {
    let d = session.dialect();
    let mut out = Vec::new();
    for table in &model.tables {
        // The first matching detector wins (e.g. golang-migrate before Rails).
        let Some(detector) = DETECTORS.iter().find(|det| matches(table, det)) else {
            continue;
        };
        // Column names come from the fixed list above; quote them for the engine.
        let quote = |expr: &str| {
            if expr.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !expr.eq_ignore_ascii_case("NULL") {
                d.quote_ident(expr)
            } else {
                expr.to_owned()
            }
        };
        let cast = |expr: &str| d.cast_to_text(&quote(expr));
        let [version, name, applied, status, batch] = detector.select;
        let order = detector.order_by.split_whitespace().map(|w| if w == "DESC" { w.to_owned() } else { quote(w) }).collect::<Vec<_>>().join(" ");
        let sql = format!(
            "SELECT {}, {}, {}, {}, {} FROM {} ORDER BY {order} LIMIT {MAX_ROWS}",
            cast(version),
            cast(name),
            cast(applied),
            cast(status),
            cast(batch),
            d.quoted_name(&table.object),
        );
        let (_, rows) = session.read_query(&table.object.database, &sql, &[], CellLimits::GRID).await?;
        let text = |row: &[crate::value::Cell], i: usize| row.get(i).and_then(crate::value::Cell::to_text);
        out.push(FrameworkMigrations {
            framework: detector.framework.to_owned(),
            table: table.object.clone(),
            entries: rows
                .iter()
                .map(|r| MigrationEntry {
                    version: text(r, 0).unwrap_or_default(),
                    name: text(r, 1),
                    applied_at: text(r, 2),
                    status: text(r, 3).unwrap_or_else(|| "applied".into()),
                    batch: text(r, 4),
                })
                .collect(),
        });
    }
    Ok(out)
}
