//! Decodes Oracle values. NUMBER keeps every digit (it is read as the
//! server's own text); dates and timestamps keep their declared precision.

use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use oracle::sql_type::{OracleType, Timestamp};
use oracle::{ColumnInfo, Row, SqlValue};

pub fn is_numeric(t: &OracleType) -> bool {
    matches!(
        t,
        OracleType::Number(..)
            | OracleType::Float(_)
            | OracleType::BinaryFloat
            | OracleType::BinaryDouble
            | OracleType::Int64
            | OracleType::UInt64
    )
}

pub fn column_meta(columns: &[ColumnInfo]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_owned(),
            type_name: c.oracle_type().to_string(),
            numeric: is_numeric(c.oracle_type()),
        })
        .collect()
}

fn timestamp_text(t: &Timestamp) -> String {
    t.to_string()
}

pub fn decode(v: &SqlValue, limits: CellLimits) -> Cell {
    if v.is_null().unwrap_or(true) {
        return Cell::Null;
    }
    let Ok(t) = v.oracle_type() else { return Cell::Null };
    let unsupported = || Cell::Unsupported { type_name: t.to_string(), value: v.get::<String>().ok() };
    match t {
        OracleType::Number(..) | OracleType::Float(_) | OracleType::Int64 | OracleType::UInt64 => {
            v.get::<String>().map_or_else(|_| unsupported(), Cell::number)
        }
        OracleType::BinaryFloat | OracleType::BinaryDouble => {
            v.get::<f64>().map_or_else(|_| unsupported(), |f| Cell::number(f.to_string()))
        }
        OracleType::Date | OracleType::Timestamp(_) | OracleType::TimestampLTZ(_) => v
            .get::<Timestamp>()
            .map_or_else(|_| unsupported(), |ts| Cell::Temporal { value: timestamp_text(&ts), zoned: false }),
        OracleType::TimestampTZ(_) => v
            .get::<Timestamp>()
            .map_or_else(|_| unsupported(), |ts| Cell::Temporal { value: timestamp_text(&ts), zoned: true }),
        OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB => {
            v.get::<Vec<u8>>().map_or_else(|_| unsupported(), |b| Cell::binary(&b, limits))
        }
        OracleType::Boolean => v.get::<bool>().map_or_else(|_| unsupported(), |b| Cell::Bool { value: b }),
        OracleType::Json => v.get::<String>().map_or_else(|_| unsupported(), |s| Cell::json(s, limits)),
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::Long
        | OracleType::Rowid
        | OracleType::CLOB
        | OracleType::NCLOB
        | OracleType::Xml
        | OracleType::IntervalDS(..)
        | OracleType::IntervalYM(_) => v.get::<String>().map_or_else(|_| unsupported(), |s| Cell::text(s, limits)),
        _ => unsupported(),
    }
}

/// Catalog value as text; binary values become lower-case hex.
pub fn cell_text(v: &SqlValue) -> Option<String> {
    match decode(v, CellLimits::FULL) {
        Cell::Binary { head_hex, .. } => Some(head_hex),
        other => other.to_text(),
    }
}

pub fn decode_row(row: &Row, limits: CellLimits) -> CellRow {
    row.sql_values().iter().map(|v| decode(v, limits)).collect()
}
