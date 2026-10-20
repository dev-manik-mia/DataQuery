//! Converts Vertica's text-format values into grid cells. Numbers stay
//! exact (their text), binary values are unescaped, zoned times become
//! RFC 3339, and arrays, sets and rows keep the server's JSON text.

use dq_core::result::{ColumnMeta, Row};
use dq_core::value::{Cell, CellLimits};

use crate::conn::RawRow;
use crate::protocol::{Column, oid, unescape_binary};

/// The type's name as Vertica spells it in column definitions.
pub fn type_name(column: &Column) -> String {
    if let Some(name) = &column.type_name {
        return name.clone();
    }
    match column.type_oid {
        oid::BOOL => "boolean",
        oid::INT8 => "int",
        oid::FLOAT8 => "float",
        oid::CHAR => "char",
        oid::VARCHAR => "varchar",
        oid::LONGVARCHAR => "long varchar",
        oid::DATE => "date",
        oid::TIME => "time",
        oid::TIMETZ => "timetz",
        oid::TIMESTAMP => "timestamp",
        oid::TIMESTAMPTZ => "timestamptz",
        oid::INTERVAL => "interval",
        oid::INTERVALYM => "interval year to month",
        oid::NUMERIC => "numeric",
        oid::BINARY => "binary",
        oid::VARBINARY => "varbinary",
        oid::LONGVARBINARY => "long varbinary",
        oid::UUID => "uuid",
        oid::ROW => "row",
        oid::MAP => "map",
        o if o == oid::ARRAY || oid::ARRAY1D.contains(&o) => "array",
        o if oid::SET.contains(&o) => "set",
        _ => "unknown",
    }
    .to_owned()
}

fn numeric(type_oid: u32) -> bool {
    matches!(type_oid, oid::INT8 | oid::FLOAT8 | oid::NUMERIC)
}

pub fn column_meta(columns: &[Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta { name: c.name.clone(), type_name: type_name(c), numeric: numeric(c.type_oid) })
        .collect()
}

/// `2026-10-14 09:59:00.123+02` → `2026-10-14T09:59:00.123+02:00`. Values
/// outside RFC 3339 (`infinity`, BC dates) are kept as the server wrote them.
fn rfc3339(text: &str) -> String {
    let Some((date, time)) = text.split_once(' ') else { return text.to_owned() };
    let Some(sign) = time.rfind(['+', '-']) else { return text.to_owned() };
    let (clock, offset) = time.split_at(sign);
    let offset = match offset.len() {
        3 => format!("{offset}:00"),
        _ => offset.to_owned(),
    };
    format!("{date}T{clock}{offset}")
}

/// One value. `None` is SQL NULL.
pub fn decode(column: &Column, raw: Option<&[u8]>, limits: CellLimits) -> Cell {
    let Some(raw) = raw else { return Cell::Null };
    // Binary types, and user types stored as binary (`GEOMETRY`, `GEOGRAPHY`).
    if oid::is_binary(column.type_oid) {
        return Cell::binary(&unescape_binary(raw), limits);
    }
    let text = String::from_utf8_lossy(raw);
    if column.type_name.is_some() {
        return Cell::text(text, limits);
    }
    match column.type_oid {
        oid::BOOL => Cell::Bool { value: text == "t" || text == "1" },
        o if numeric(o) => Cell::number(text),
        oid::DATE | oid::TIME | oid::TIMESTAMP => Cell::Temporal { value: text.into_owned(), zoned: false },
        oid::TIMESTAMPTZ => Cell::Temporal { value: rfc3339(&text), zoned: true },
        oid::TIMETZ => Cell::Temporal { value: text.into_owned(), zoned: true },
        oid::ROW | oid::ARRAY | oid::MAP => Cell::json(text, limits),
        o if oid::ARRAY1D.contains(&o) || oid::SET.contains(&o) => Cell::json(text, limits),
        oid::CHAR | oid::VARCHAR | oid::LONGVARCHAR | oid::INTERVAL | oid::INTERVALYM | oid::UUID | oid::UNKNOWN => {
            Cell::text(text, limits)
        }
        _ => Cell::Unsupported { type_name: type_name(column), value: Some(text.into_owned()) },
    }
}

pub fn decode_row(columns: &[Column], row: RawRow, limits: CellLimits) -> Row {
    columns.iter().zip(row.iter()).map(|(c, v)| decode(c, v.as_deref(), limits)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(type_oid: u32) -> Column {
        Column { name: "c".into(), table_oid: 0, type_oid, type_name: None, type_modifier: -1, nullable: true }
    }

    fn cell(type_oid: u32, raw: &str) -> Cell {
        decode(&column(type_oid), Some(raw.as_bytes()), CellLimits::GRID)
    }

    #[test]
    fn values_by_type() {
        assert_eq!(decode(&column(oid::INT8), None, CellLimits::GRID), Cell::Null);
        assert_eq!(cell(oid::BOOL, "t"), Cell::Bool { value: true });
        assert_eq!(cell(oid::NUMERIC, "1234567890123456789012345678.0123456789").to_text().unwrap().len(), 39);
        assert_eq!(cell(oid::INT8, "9223372036854775807"), Cell::number("9223372036854775807"));
        assert_eq!(
            cell(oid::TIMESTAMPTZ, "2026-10-14 09:59:00.123+02"),
            Cell::Temporal { value: "2026-10-14T09:59:00.123+02:00".into(), zoned: true }
        );
        assert_eq!(
            cell(oid::TIMESTAMPTZ, "2026-10-14 09:59:00+05:30"),
            Cell::Temporal { value: "2026-10-14T09:59:00+05:30".into(), zoned: true }
        );
        assert_eq!(
            cell(oid::TIMESTAMP, "2026-10-14 09:59:00"),
            Cell::Temporal { value: "2026-10-14 09:59:00".into(), zoned: false }
        );
        assert_eq!(cell(oid::VARBINARY, "\\211PNG"), Cell::binary(&[0x89, b'P', b'N', b'G'], CellLimits::GRID));
        assert_eq!(cell(1509, "[\"a\",\"b c\"]").to_json(), serde_json::json!(["a", "b c"]));
        let geometry = Column { type_name: Some("geometry".into()), ..column(oid::LONGVARBINARY) };
        assert!(matches!(decode(&geometry, Some(b"\\001"), CellLimits::GRID), Cell::Binary { length: 1, .. }));
        assert_eq!(type_name(&geometry), "geometry");
        assert!(column_meta(&[column(oid::FLOAT8)])[0].numeric);
    }
}
