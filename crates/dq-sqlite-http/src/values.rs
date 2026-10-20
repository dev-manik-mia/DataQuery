//! Remote results as cells, with the same rules as local `SQLite` files
//! ([`dq_sqlite::values::cell`]).

use dq_core::result::{ColumnMeta, Row as CellRow, TextRow};
use dq_core::value::CellLimits;
use dq_sqlite::catalog::text_of;
use dq_sqlite::values::{Value, ValueRef, cell, numeric_affinity};

use crate::transport::{Column, Output};

/// Grid columns. Without a declared type (expressions, D1) a column is
/// numeric when its first non-NULL value is.
pub fn column_meta(output: &Output) -> Vec<ColumnMeta> {
    output
        .columns
        .iter()
        .enumerate()
        .map(|(i, Column { name, decltype })| {
            let numeric = if decltype.is_empty() {
                output
                    .rows
                    .iter()
                    .map(|r| r.get(i))
                    .find(|v| !matches!(v, Some(Value::Null) | None))
                    .is_some_and(|v| matches!(v, Some(Value::Integer(_) | Value::Real(_))))
            } else {
                numeric_affinity(decltype)
            };
            ColumnMeta { name: name.clone(), type_name: decltype.clone(), numeric }
        })
        .collect()
}

pub fn row(values: &[Value], columns: &[Column], limits: CellLimits) -> CellRow {
    values.iter().zip(columns).map(|(value, column)| cell(ValueRef::from(value), &column.decltype, limits)).collect()
}

pub fn text_row(values: &[Value]) -> TextRow {
    TextRow(values.iter().map(|v| text_of(ValueRef::from(v))).collect())
}

#[cfg(test)]
mod tests {
    use dq_core::value::Cell;

    use super::*;

    #[test]
    fn infers_numeric_columns_without_types() {
        let output = Output {
            columns: vec![
                Column { name: "n".into(), decltype: String::new() },
                Column { name: "t".into(), decltype: String::new() },
                Column { name: "d".into(), decltype: "JSON".into() },
            ],
            rows: vec![
                vec![Value::Null, Value::Text("x".into()), Value::Text("{}".into())],
                vec![Value::Integer(7), Value::Integer(1), Value::Null],
            ],
            affected: 0,
        };
        let meta = column_meta(&output);
        assert!(meta[0].numeric && !meta[1].numeric && !meta[2].numeric);
        let cells = row(&output.rows[0], &output.columns, CellLimits::GRID);
        assert!(matches!(cells[2], Cell::Json { .. }));
        assert_eq!(text_row(&output.rows[1]).0, [Some("7".to_owned()), Some("1".to_owned()), None]);
    }
}
