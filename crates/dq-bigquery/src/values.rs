//! Decodes `BigQuery` rows (`{"f": [{"v": …}]}`) by their schema and builds
//! query parameters. Scalars arrive as text: integers and numerics stay exact,
//! `BYTES` is base64, `TIMESTAMP` is microseconds since the epoch (with
//! `useInt64Timestamp`) or seconds in decimal notation; `RECORD` and
//! `REPEATED` values nest and are shown as JSON.

use base64::Engine as _;
use chrono::DateTime;
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use serde_json::{Map, Value, json};

use crate::client::Field;

/// The `GoogleSQL` name of an API type name (`INTEGER` → `INT64`, `RECORD` → `STRUCT`).
fn sql_type(kind: &str) -> &str {
    match kind {
        "INTEGER" => "INT64",
        "FLOAT" => "FLOAT64",
        "BOOLEAN" => "BOOL",
        "RECORD" => "STRUCT",
        other => other,
    }
}

fn base_kind(field: &Field) -> &str {
    sql_type(field.kind.as_deref().unwrap_or("STRING"))
}

fn repeated(field: &Field) -> bool {
    field.mode.as_deref() == Some("REPEATED")
}

/// The type as `GoogleSQL` spells it: `STRING(20)`, `NUMERIC(12, 2)`,
/// `ARRAY<STRUCT<a INT64, b STRING>>`, `RANGE<DATE>`.
pub fn type_label(field: &Field) -> String {
    let base = base_kind(field);
    let scalar = match base {
        "STRUCT" => format!(
            "STRUCT<{}>",
            field.fields.iter().map(|f| format!("{} {}", f.name, type_label(f))).collect::<Vec<_>>().join(", ")
        ),
        "STRING" | "BYTES" => field.max_length.as_ref().map_or_else(|| base.to_owned(), |l| format!("{base}({l})")),
        "NUMERIC" | "BIGNUMERIC" => match (&field.precision, &field.scale) {
            (Some(p), Some(s)) => format!("{base}({p}, {s})"),
            (Some(p), None) => format!("{base}({p})"),
            _ => base.to_owned(),
        },
        "RANGE" => format!("RANGE<{}>", field.range_element_type.as_ref().map_or("DATE", |r| r.kind.as_str())),
        other => other.to_owned(),
    };
    if repeated(field) { format!("ARRAY<{scalar}>") } else { scalar }
}

pub fn is_numeric(field: &Field) -> bool {
    !repeated(field) && matches!(base_kind(field), "INT64" | "NUMERIC" | "BIGNUMERIC" | "FLOAT64")
}

pub fn column_meta(fields: &[Field]) -> Vec<ColumnMeta> {
    fields
        .iter()
        .map(|f| ColumnMeta { name: f.name.clone(), type_name: type_label(f), numeric: is_numeric(f) })
        .collect()
}

/// Microseconds since the epoch of `1791964740123456`, `1791964740.123456` or `1.791964740123456E9`.
pub fn timestamp_micros(text: &str) -> Option<i64> {
    if let Ok(micros) = text.parse::<i64>() {
        return Some(micros);
    }
    let (mantissa, exponent) = match text.split_once(['E', 'e']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches(['-', '+']);
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // The decimal point sits after `whole.len() + exponent` digits; keep 6 more.
    let point = whole.len() as i32 + exponent;
    let keep = usize::try_from(point + 6).ok()?;
    let micros: String = digits.chars().chain(std::iter::repeat('0')).take(keep).collect();
    let value: i64 = if micros.is_empty() { 0 } else { micros.parse().ok()? };
    Some(if negative { -value } else { value })
}

/// `2026-10-14 07:59:00.123456+00:00` (fractions only when present).
pub fn timestamp_text(micros: i64) -> Option<String> {
    let t = DateTime::from_timestamp_micros(micros)?;
    let fraction = micros.rem_euclid(1_000_000);
    let fraction = if fraction == 0 { String::new() } else { format!(".{fraction:06}") };
    Some(format!("{}{fraction}+00:00", t.format("%Y-%m-%d %H:%M:%S")))
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A nested value as JSON: structs as objects, arrays as arrays, exact numbers as strings.
pub fn json_value(field: &Field, value: &Value, element: bool) -> Value {
    if repeated(field) && !element {
        let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
        return Value::Array(items.iter().map(|item| json_value(field, &item["v"], true)).collect());
    }
    if base_kind(field) == "STRUCT" {
        let cells = value["f"].as_array().map(Vec::as_slice).unwrap_or_default();
        if value.is_null() {
            return Value::Null;
        }
        let object: Map<String, Value> =
            field.fields.iter().zip(cells).map(|(f, c)| (f.name.clone(), json_value(f, &c["v"], false))).collect();
        return Value::Object(object);
    }
    let Some(text) = scalar_text(value) else { return Value::Null };
    match base_kind(field) {
        "INT64" => text.parse::<i64>().map_or(Value::String(text), |n| json!(n)),
        "FLOAT64" => {
            text.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map_or(Value::String(text), Value::Number)
        }
        "BOOL" => Value::Bool(text == "true"),
        "TIMESTAMP" => Value::String(timestamp_micros(&text).and_then(timestamp_text).unwrap_or(text)),
        "DATETIME" => Value::String(text.replacen('T', " ", 1)),
        "JSON" => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        _ => Value::String(text),
    }
}

/// One cell of a result row.
pub fn cell(field: &Field, value: &Value, limits: CellLimits) -> Cell {
    if value.is_null() {
        return Cell::Null;
    }
    if repeated(field) || base_kind(field) == "STRUCT" {
        return Cell::json(json_value(field, value, false).to_string(), limits);
    }
    let Some(text) = scalar_text(value) else { return Cell::Null };
    match base_kind(field) {
        "INT64" | "NUMERIC" | "BIGNUMERIC" | "FLOAT64" => Cell::number(text),
        "BOOL" => Cell::Bool { value: text == "true" },
        "BYTES" => match base64::engine::general_purpose::STANDARD.decode(&text) {
            Ok(bytes) => Cell::binary(&bytes, limits),
            Err(_) => Cell::Unsupported { type_name: "BYTES".into(), value: Some(text) },
        },
        "DATE" | "TIME" => Cell::Temporal { value: text, zoned: false },
        "DATETIME" => Cell::Temporal { value: text.replacen('T', " ", 1), zoned: false },
        "TIMESTAMP" => match timestamp_micros(&text).and_then(timestamp_text) {
            Some(value) => Cell::Temporal { value, zoned: true },
            None => Cell::Unsupported { type_name: "TIMESTAMP".into(), value: Some(text) },
        },
        "JSON" => Cell::json(text, limits),
        // STRING, GEOGRAPHY (WKT), INTERVAL, RANGE.
        _ => Cell::text(text, limits),
    }
}

/// Cells of the API rows `{"f": [{"v": …}, …]}`.
pub fn rows(fields: &[Field], rows: &[Value], limits: CellLimits) -> Vec<CellRow> {
    rows.iter()
        .map(|row| {
            let cells = row["f"].as_array().map(Vec::as_slice).unwrap_or_default();
            fields.iter().zip(cells).map(|(f, c)| cell(f, &c["v"], limits)).collect()
        })
        .collect()
}

/// The parameter type an editor value is sent as: `BOOL`, `INT64` and
/// `NUMERIC` for values spelled that way, `STRING` for the rest (`BigQuery`
/// coerces `STRING` parameters to dates, times and timestamps).
pub fn infer_type(value: &str) -> &'static str {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let integer =
        !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) && (digits == "0" || !digits.starts_with('0'));
    if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        "BOOL"
    } else if integer && value.parse::<i64>().is_ok() {
        "INT64"
    } else if let Some((whole, fraction)) = digits.split_once('.')
        && !whole.is_empty()
        && !fraction.is_empty()
        && whole.bytes().chain(fraction.bytes()).all(|c| c.is_ascii_digit())
        && (whole == "0" || !whole.starts_with('0'))
        && whole.len() <= 29
        && fraction.len() <= 9
    {
        "NUMERIC"
    } else {
        "STRING"
    }
}

/// A `queryParameters` entry; `name` is `None` for positional parameters.
pub fn parameter(name: Option<&str>, value: Option<&str>, infer: bool) -> Value {
    let kind = match value {
        Some(v) if infer => infer_type(v),
        _ => "STRING",
    };
    let value = match value {
        Some(v) if kind == "BOOL" => json!({ "value": v.to_ascii_lowercase() }),
        Some(v) => json!({ "value": v }),
        None => json!({}),
    };
    let mut parameter = json!({ "parameterType": { "type": kind }, "parameterValue": value });
    if let Some(name) = name {
        parameter["name"] = json!(name);
    }
    parameter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(json: Value) -> Field {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn decodes_scalars_exactly() {
        let g = CellLimits::GRID;
        let f = |kind: &str| field(json!({ "name": "c", "type": kind, "mode": "NULLABLE" }));
        assert_eq!(cell(&f("INTEGER"), &json!("9223372036854775807"), g), Cell::number("9223372036854775807"));
        assert_eq!(
            cell(&f("BIGNUMERIC"), &json!("123456789012345678901234567890.0123456789"), g),
            Cell::number("123456789012345678901234567890.0123456789")
        );
        assert_eq!(cell(&f("BOOLEAN"), &json!("true"), g), Cell::Bool { value: true });
        assert_eq!(cell(&f("BYTES"), &json!("AP8="), g), Cell::binary(&[0, 255], g));
        assert_eq!(
            cell(&f("DATETIME"), &json!("2026-10-14T09:59:00.5"), g),
            Cell::Temporal { value: "2026-10-14 09:59:00.5".into(), zoned: false }
        );
        let zoned = Cell::Temporal { value: "2026-10-14 07:59:00.123456+00:00".into(), zoned: true };
        assert_eq!(cell(&f("TIMESTAMP"), &json!("1791964740123456"), g), zoned);
        assert_eq!(cell(&f("TIMESTAMP"), &json!("1.791964740123456E9"), g), zoned);
        assert_eq!(cell(&f("TIMESTAMP"), &json!("1791964740.123456"), g), zoned);
        assert_eq!(timestamp_text(0).as_deref(), Some("1970-01-01 00:00:00+00:00"));
        assert_eq!(timestamp_micros("-1.5E0"), Some(-1_500_000));
        assert_eq!(cell(&f("GEOGRAPHY"), &json!("POINT(1 2)"), g), Cell::text("POINT(1 2)", g));
        assert_eq!(cell(&f("JSON"), &json!("{\"k\":1}"), g), Cell::json("{\"k\":1}", g));
        assert_eq!(cell(&f("STRING"), &Value::Null, g), Cell::Null);
    }

    #[test]
    fn nests_structs_and_arrays_as_json() {
        let g = CellLimits::GRID;
        let s = field(json!({ "name": "s", "type": "RECORD", "mode": "NULLABLE", "fields": [
            { "name": "a", "type": "INTEGER" }, { "name": "b", "type": "STRING", "mode": "REPEATED" },
            { "name": "n", "type": "NUMERIC" } ] }));
        assert_eq!(type_label(&s), "STRUCT<a INT64, b ARRAY<STRING>, n NUMERIC>");
        let v = json!({ "f": [{ "v": "1" }, { "v": [{ "v": "x" }, { "v": "y" }] }, { "v": "0.10" }] });
        assert_eq!(cell(&s, &v, g), Cell::json(r#"{"a":1,"b":["x","y"],"n":"0.10"}"#, g));
        let arr = field(json!({ "name": "arr", "type": "INTEGER", "mode": "REPEATED" }));
        assert_eq!(type_label(&arr), "ARRAY<INT64>");
        assert_eq!(cell(&arr, &json!([{ "v": "1" }, { "v": "2" }]), g), Cell::json("[1,2]", g));
        assert_eq!(cell(&arr, &json!([]), g), Cell::json("[]", g));
        let typed = field(json!({ "name": "n", "type": "NUMERIC", "precision": "12", "scale": "2" }));
        assert_eq!(type_label(&typed), "NUMERIC(12, 2)");
        assert!(is_numeric(&typed) && !is_numeric(&arr));
    }

    #[test]
    fn types_editor_parameters() {
        assert_eq!(infer_type("42"), "INT64");
        assert_eq!(infer_type("-7"), "INT64");
        assert_eq!(infer_type("007"), "STRING");
        assert_eq!(infer_type("99999999999999999999"), "STRING");
        assert_eq!(infer_type("12.50"), "NUMERIC");
        assert_eq!(infer_type("TRUE"), "BOOL");
        assert_eq!(infer_type("2026-10-14"), "STRING");
        assert_eq!(
            parameter(Some("id"), Some("1"), true),
            json!({ "name": "id", "parameterType": { "type": "INT64" }, "parameterValue": { "value": "1" } })
        );
        assert_eq!(parameter(None, None, true), json!({ "parameterType": { "type": "STRING" }, "parameterValue": {} }));
        assert_eq!(parameter(None, Some("1"), false)["parameterType"]["type"], "STRING");
    }
}
