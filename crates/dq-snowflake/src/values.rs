//! Decodes Snowflake results by their `rowtype`. Arrow results carry
//! NUMBER as scaled integers, TIMESTAMP as `{epoch, fraction[, timezone]}`
//! structs and TIME as scaled integers; JSON results carry text (dates as
//! day counts, timestamps as `seconds.fraction`).

use arrow::array::{Array, ArrayRef, AsArray};
use arrow::datatypes::{DataType, Decimal128Type, Int8Type, Int16Type, Int32Type, Int64Type};
use arrow::record_batch::RecordBatch;
use chrono::{DateTime, NaiveDate};
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use serde_json::Value;

use crate::client::RowType;

/// The type as Snowflake spells it in DDL.
pub fn type_label(t: &RowType) -> String {
    let scale = t.scale.unwrap_or(0);
    match t.kind.as_str() {
        "fixed" => format!("NUMBER({},{scale})", t.precision.unwrap_or(38)),
        "real" => "FLOAT".into(),
        "text" => {
            t.length.filter(|l| *l > 0 && *l < 16_777_216).map_or_else(|| "VARCHAR".into(), |l| format!("VARCHAR({l})"))
        }
        "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => format!("{}({scale})", t.kind.to_ascii_uppercase()),
        other => other.to_ascii_uppercase(),
    }
}

pub fn column_meta(rowtype: &[RowType]) -> Vec<ColumnMeta> {
    rowtype
        .iter()
        .map(|t| ColumnMeta {
            name: t.name.clone(),
            type_name: type_label(t),
            numeric: matches!(t.kind.as_str(), "fixed" | "real"),
        })
        .collect()
}

/// `value / 10^scale` written out exactly.
pub fn scaled(value: i128, scale: i64) -> String {
    if scale <= 0 {
        return value.to_string();
    }
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    let pow = 10u128.pow(scale as u32);
    format!("{sign}{}.{:0width$}", abs / pow, abs % pow, width = scale as usize)
}

fn fraction(nanos: u32, digits: i64) -> String {
    let digits = digits.clamp(0, 9) as usize;
    if digits == 0 {
        return String::new();
    }
    let value = nanos / 10u32.pow(9 - digits as u32);
    format!(".{value:0digits$}")
}

/// `YYYY-MM-DD HH:MM:SS[.fff]` for a UTC instant (or a wall-clock time stored as one).
fn timestamp_text(seconds: i64, nanos: u32, digits: i64) -> Option<String> {
    let t = DateTime::from_timestamp(seconds, nanos)?.naive_utc();
    Some(format!("{}{}", t.format("%Y-%m-%d %H:%M:%S"), fraction(nanos, digits)))
}

fn offset_text(minutes: i64) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    format!("{sign}{:02}:{:02}", minutes.abs() / 60, minutes.abs() % 60)
}

/// A zoned timestamp shown in its own offset.
fn zoned_text(seconds: i64, nanos: u32, offset_minutes: i64, digits: i64) -> Option<String> {
    let local = timestamp_text(seconds + offset_minutes * 60, nanos, digits)?;
    Some(format!("{local} {}", offset_text(offset_minutes)))
}

fn int_value(column: &ArrayRef, row: usize) -> Option<i128> {
    Some(match column.data_type() {
        DataType::Int8 => i128::from(column.as_primitive::<Int8Type>().value(row)),
        DataType::Int16 => i128::from(column.as_primitive::<Int16Type>().value(row)),
        DataType::Int32 => i128::from(column.as_primitive::<Int32Type>().value(row)),
        DataType::Int64 => i128::from(column.as_primitive::<Int64Type>().value(row)),
        DataType::Decimal128(..) => column.as_primitive::<Decimal128Type>().value(row),
        _ => return None,
    })
}

/// Seconds and nanoseconds of a scaled integer timestamp.
fn split_scaled(value: i128, scale: i64) -> (i64, u32) {
    let pow = 10i128.pow(scale.clamp(0, 9) as u32);
    let seconds = value.div_euclid(pow);
    let nanos = value.rem_euclid(pow) * 10i128.pow(9 - scale.clamp(0, 9) as u32);
    (seconds as i64, nanos as u32)
}

fn arrow_cell(t: &RowType, column: &ArrayRef, row: usize, limits: CellLimits) -> Cell {
    if column.is_null(row) {
        return Cell::Null;
    }
    let scale = t.scale.unwrap_or(0);
    let unsupported = || Cell::Unsupported { type_name: t.kind.clone(), value: None };
    match t.kind.as_str() {
        "fixed" => match column.data_type() {
            DataType::Decimal128(_, s) => {
                Cell::number(scaled(column.as_primitive::<Decimal128Type>().value(row), i64::from(*s)))
            }
            _ => int_value(column, row).map_or_else(unsupported, |v| Cell::number(scaled(v, scale))),
        },
        "real" => column
            .as_primitive_opt::<arrow::datatypes::Float64Type>()
            .map_or_else(unsupported, |a| Cell::number(a.value(row).to_string())),
        "boolean" => column.as_boolean_opt().map_or_else(unsupported, |a| Cell::Bool { value: a.value(row) }),
        "binary" => column.as_binary_opt::<i32>().map_or_else(unsupported, |a| Cell::binary(a.value(row), limits)),
        "date" => column
            .as_primitive_opt::<arrow::datatypes::Date32Type>()
            .and_then(|a| NaiveDate::from_num_days_from_ce_opt(a.value(row) + 719_163))
            .map_or_else(unsupported, |d| Cell::Temporal { value: d.format("%Y-%m-%d").to_string(), zoned: false }),
        "time" => int_value(column, row).map_or_else(unsupported, |v| {
            let (seconds, nanos) = split_scaled(v, scale);
            Cell::Temporal {
                value: format!(
                    "{:02}:{:02}:{:02}{}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60,
                    fraction(nanos, scale)
                ),
                zoned: false,
            }
        }),
        "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => {
            let zoned = t.kind == "timestamp_tz";
            let parts = match column.data_type() {
                DataType::Struct(_) => {
                    let s = column.as_struct();
                    let field = |name: &str| s.column_by_name(name).and_then(|c| int_value(c, row));
                    match (field("epoch"), field("fraction"), field("timezone")) {
                        (Some(epoch), Some(fraction), tz) => Some((epoch as i64, fraction as u32, tz)),
                        // Two-field form: a scaled epoch and the zone.
                        (Some(epoch), None, tz) => {
                            let (seconds, nanos) = split_scaled(epoch, scale);
                            Some((seconds, nanos, tz))
                        }
                        _ => None,
                    }
                }
                _ => int_value(column, row).map(|v| {
                    let (seconds, nanos) = split_scaled(v, scale);
                    (seconds, nanos, None)
                }),
            };
            let text = parts.and_then(|(seconds, nanos, tz)| match tz {
                Some(tz) if zoned => zoned_text(seconds, nanos, tz as i64 - 1440, scale),
                _ => timestamp_text(seconds, nanos, scale),
            });
            text.map_or_else(unsupported, |value| Cell::Temporal { value, zoned })
        }
        "variant" | "object" | "array" => {
            column.as_string_opt::<i32>().map_or_else(unsupported, |a| Cell::json(a.value(row), limits))
        }
        _ => match column.as_string_opt::<i32>() {
            Some(a) => Cell::text(a.value(row), limits),
            None => column.as_string_opt::<i64>().map_or_else(unsupported, |a| Cell::text(a.value(row), limits)),
        },
    }
}

pub fn arrow_rows(batch: &RecordBatch, rowtype: &[RowType], limits: CellLimits) -> Vec<CellRow> {
    (0..batch.num_rows())
        .map(|row| rowtype.iter().zip(batch.columns()).map(|(t, c)| arrow_cell(t, c, row, limits)).collect())
        .collect()
}

/// A JSON-format value (all text).
fn json_cell(t: &RowType, value: &Value, limits: CellLimits) -> Cell {
    let Some(text) = value.as_str().map(str::to_owned).or_else(|| (!value.is_null()).then(|| value.to_string())) else {
        return Cell::Null;
    };
    let scale = t.scale.unwrap_or(0);
    let seconds_fraction = |s: &str| -> Option<(i64, u32)> {
        let (sec, frac) = s.split_once('.').unwrap_or((s, "0"));
        let nanos: u32 = format!("{frac:0<9}")[..9].parse().ok()?;
        Some((sec.parse().ok()?, nanos))
    };
    match t.kind.as_str() {
        "fixed" | "real" => Cell::number(text),
        "boolean" => Cell::Bool { value: matches!(text.as_str(), "1" | "true" | "TRUE") },
        "date" => {
            text.parse::<i32>().ok().and_then(|days| NaiveDate::from_num_days_from_ce_opt(days + 719_163)).map_or(
                Cell::Temporal { value: text.clone(), zoned: false },
                |d| Cell::Temporal { value: d.format("%Y-%m-%d").to_string(), zoned: false },
            )
        }
        "timestamp_ntz" | "timestamp_ltz" => {
            seconds_fraction(&text).and_then(|(s, n)| timestamp_text(s, n, scale)).map_or(
                Cell::Temporal { value: text.clone(), zoned: false },
                |value| Cell::Temporal { value, zoned: false },
            )
        }
        "timestamp_tz" => {
            let (instant, zone) = text.split_once(' ').unwrap_or((&text, "1440"));
            seconds_fraction(instant)
                .zip(zone.parse::<i64>().ok())
                .and_then(|((s, n), z)| zoned_text(s, n, z - 1440, scale))
                .map_or(Cell::Temporal { value: text.clone(), zoned: true }, |value| Cell::Temporal {
                    value,
                    zoned: true,
                })
        }
        "variant" | "object" | "array" => Cell::json(text, limits),
        "binary" => {
            let bytes: Vec<u8> =
                (0..text.len() / 2).filter_map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()).collect();
            Cell::binary(&bytes, limits)
        }
        _ => Cell::text(text, limits),
    }
}

pub fn json_rows(rows: &[Vec<Value>], rowtype: &[RowType], limits: CellLimits) -> Vec<CellRow> {
    rows.iter().map(|r| rowtype.iter().zip(r).map(|(t, v)| json_cell(t, v, limits)).collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(kind: &str, scale: i64) -> RowType {
        RowType {
            name: "c".into(),
            kind: kind.into(),
            scale: Some(scale),
            precision: Some(38),
            length: None,
            nullable: Some(true),
        }
    }

    #[test]
    fn decodes_scaled_and_temporal_values() {
        assert_eq!(scaled(-1, 10), "-0.0000000001");
        assert_eq!(scaled(1_234_567, 2), "12345.67");
        assert_eq!(scaled(7, 0), "7");
        let g = CellLimits::GRID;
        assert_eq!(
            json_cell(&t("date", 0), &Value::String("20375".into()), g),
            Cell::Temporal { value: "2025-10-14".into(), zoned: false }
        );
        assert_eq!(
            json_cell(&t("timestamp_ntz", 3), &Value::String("1760435940.123000000".into()), g),
            Cell::Temporal { value: "2025-10-14 09:59:00.123".into(), zoned: false }
        );
        assert_eq!(
            json_cell(&t("timestamp_tz", 3), &Value::String("1760435940.123000000 1560".into()), g),
            Cell::Temporal { value: "2025-10-14 11:59:00.123 +02:00".into(), zoned: true }
        );
        assert_eq!(json_cell(&t("fixed", 0), &Value::Null, g), Cell::Null);
        assert_eq!(type_label(&t("fixed", 2)), "NUMBER(38,2)");
    }
}
