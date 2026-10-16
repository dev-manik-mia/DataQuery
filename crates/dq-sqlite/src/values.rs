//! Turns `SQLite` values into cells. A column's declared type picks the cell
//! kind only where the stored value fits it; `SQLite` stores any value in any column.

use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use rusqlite::types::ValueRef;
use rusqlite::{Row, Statement};

/// Declared types that hold JSON text.
fn is_json(declared: &str) -> bool {
    declared.eq_ignore_ascii_case("json") || declared.eq_ignore_ascii_case("jsonb")
}

/// Declared types with numeric affinity are right-aligned in the grid.
pub fn numeric_affinity(declared: &str) -> bool {
    let t = declared.to_ascii_uppercase();
    t.contains("INT")
        || t.contains("REAL")
        || t.contains("FLOA")
        || t.contains("DOUB")
        || t.contains("NUM")
        || t.contains("DEC")
}

pub fn column_meta(statement: &Statement<'_>) -> Vec<ColumnMeta> {
    statement
        .columns()
        .iter()
        .map(|c| {
            let declared = c.decl_type().unwrap_or_default().to_owned();
            ColumnMeta { name: c.name().to_owned(), numeric: numeric_affinity(&declared), type_name: declared }
        })
        .collect()
}

pub fn cell(value: ValueRef<'_>, declared: &str, limits: CellLimits) -> Cell {
    match value {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::number(i.to_string()),
        ValueRef::Real(f) => Cell::number(if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
            format!("{f:.1}")
        } else {
            f.to_string()
        }),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            if is_json(declared) {
                match Cell::text(text, limits) {
                    Cell::Text { value, truncated } => Cell::Json { value, truncated },
                    other => other,
                }
            } else {
                Cell::text(text, limits)
            }
        }
        ValueRef::Blob(bytes) => Cell::binary(bytes, limits),
    }
}

pub fn row(row: &Row<'_>, declared: &[String], limits: CellLimits) -> rusqlite::Result<CellRow> {
    (0..declared.len()).map(|i| Ok(cell(row.get_ref(i)?, &declared[i], limits))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_follow_storage_class() {
        assert_eq!(cell(ValueRef::Integer(7), "TEXT", CellLimits::GRID), Cell::number("7"));
        assert_eq!(cell(ValueRef::Real(2.0), "REAL", CellLimits::GRID), Cell::number("2.0"));
        assert!(matches!(cell(ValueRef::Text(b"{}"), "JSON", CellLimits::GRID), Cell::Json { .. }));
        assert!(matches!(cell(ValueRef::Blob(&[1, 2]), "", CellLimits::GRID), Cell::Binary { length: 2, .. }));
        assert!(numeric_affinity("BIGINT") && numeric_affinity("decimal(10,2)") && !numeric_affinity("TEXT"));
    }
}
