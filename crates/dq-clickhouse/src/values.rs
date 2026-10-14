//! Decodes `ClickHouse` JSON values by their declared type. Integers and
//! decimals arrive quoted, so every digit survives.

use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use serde_json::Value;

/// The type without `Nullable(…)` / `LowCardinality(…)` wrappers.
pub fn base_type(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("].iter().find_map(|w| t.strip_prefix(w).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(inner) => t = inner.trim(),
            None => return t,
        }
    }
}

pub fn is_numeric(t: &str) -> bool {
    let base = base_type(t);
    ["Int", "UInt", "Float", "Decimal", "BFloat16"].iter().any(|p| base.starts_with(p))
}

fn is_temporal(base: &str) -> bool {
    base.starts_with("Date") || base.starts_with("Time")
}

pub fn column_meta(names: &[String], types: &[String]) -> Vec<ColumnMeta> {
    names
        .iter()
        .zip(types)
        .map(|(name, t)| ColumnMeta { name: name.clone(), type_name: t.clone(), numeric: is_numeric(t) })
        .collect()
}

pub fn decode(value: &Value, data_type: &str, limits: CellLimits) -> Cell {
    let base = base_type(data_type);
    match value {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool { value: *b },
        Value::Number(n) => Cell::number(n.to_string()),
        Value::String(s) if is_numeric(base) => Cell::number(s.clone()),
        Value::String(s) if is_temporal(base) => Cell::Temporal { value: s.clone(), zoned: false },
        Value::String(s) if base.starts_with("JSON") || base.starts_with("Object") => Cell::json(s.clone(), limits),
        Value::String(s) => Cell::text(s.clone(), limits),
        // Arrays, maps, tuples and JSON documents.
        other => Cell::json(other.to_string(), limits),
    }
}

pub fn decode_row(row: &[Value], types: &[String], limits: CellLimits) -> CellRow {
    row.iter().zip(types).map(|(v, t)| decode(v, t, limits)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_by_type() {
        assert_eq!(base_type("LowCardinality(Nullable(String))"), "String");
        let g = CellLimits::GRID;
        assert_eq!(decode(&Value::String("18446744073709551615".into()), "UInt64", g), Cell::number("18446744073709551615"));
        assert_eq!(decode(&Value::String("-0.0000000001".into()), "Nullable(Decimal(38, 10))", g), Cell::number("-0.0000000001"));
        assert_eq!(decode(&Value::String("2026-10-14 09:59:00.123".into()), "DateTime64(3, 'UTC')", g), Cell::Temporal { value: "2026-10-14 09:59:00.123".into(), zoned: false });
        assert_eq!(decode(&serde_json::json!(["a", "b"]), "Array(String)", g), Cell::json("[\"a\",\"b\"]", g));
        assert_eq!(decode(&Value::Null, "Nullable(String)", g), Cell::Null);
        assert!(is_numeric("Nullable(Int128)") && !is_numeric("String"));
    }
}
