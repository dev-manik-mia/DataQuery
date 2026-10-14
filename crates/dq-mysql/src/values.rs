//! Decodes MySQL text-protocol values. Every value arrives as the server's own
//! text, so numbers and decimals stay exact.

// The MYSQL_TYPE_* constants read better unqualified in these matches.
#![allow(clippy::enum_glob_use)]

use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Row, Value};

/// The `binary` character set: bytes, not text.
const BINARY_CHARSET: u16 = 63;

pub fn is_numeric(c: &Column) -> bool {
    use ColumnType::*;
    matches!(
        c.column_type(),
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_YEAR
    )
}

fn is_binary(c: &Column) -> bool {
    use ColumnType::*;
    match c.column_type() {
        MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY => true,
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_VARCHAR => c.character_set() == BINARY_CHARSET,
        _ => false,
    }
}

pub fn type_label(c: &Column) -> String {
    use ColumnType::*;
    // ENUM and SET columns arrive as strings, marked only by a flag.
    if c.flags().contains(ColumnFlags::ENUM_FLAG) {
        return "enum".to_owned();
    }
    if c.flags().contains(ColumnFlags::SET_FLAG) {
        return "set".to_owned();
    }
    let unsigned = c.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = c.character_set() == BINARY_CHARSET;
    let name = match c.column_type() {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        _ => "unknown",
    };
    if unsigned { format!("{name} unsigned") } else { name.to_owned() }
}

pub fn column_meta(columns: &[Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta { name: c.name_str().into_owned(), type_name: type_label(c), numeric: is_numeric(c) })
        .collect()
}

pub fn decode_row(row: &Row, columns: &[Column], limits: CellLimits) -> CellRow {
    columns.iter().enumerate().map(|(i, c)| row.as_ref(i).map_or(Cell::Null, |v| decode(c, v, limits))).collect()
}

pub fn decode(c: &Column, value: &Value, limits: CellLimits) -> Cell {
    use ColumnType::*;
    match value {
        Value::NULL => Cell::Null,
        Value::Bytes(bytes) => {
            if is_binary(c) {
                return Cell::binary(bytes, limits);
            }
            let text = String::from_utf8_lossy(bytes);
            if is_numeric(c) {
                return Cell::number(text);
            }
            match c.column_type() {
                MYSQL_TYPE_DATE
                | MYSQL_TYPE_NEWDATE
                | MYSQL_TYPE_TIME
                | MYSQL_TYPE_TIME2
                | MYSQL_TYPE_DATETIME
                | MYSQL_TYPE_DATETIME2
                | MYSQL_TYPE_TIMESTAMP
                | MYSQL_TYPE_TIMESTAMP2 => Cell::Temporal { value: text.into_owned(), zoned: false },
                MYSQL_TYPE_JSON => Cell::json(text, limits),
                _ => Cell::text(text, limits),
            }
        }
        Value::Int(i) => Cell::number(i.to_string()),
        Value::UInt(u) => Cell::number(u.to_string()),
        Value::Float(f) => Cell::number(f.to_string()),
        Value::Double(d) => Cell::number(d.to_string()),
        other => Cell::text(other.as_sql(true).trim_matches('\'').to_owned(), limits),
    }
}

/// Reads a catalog value as text, whichever protocol produced it. `None` is SQL NULL.
pub fn text(row: &Row, index: usize) -> Option<String> {
    match row.as_ref(index)? {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}
