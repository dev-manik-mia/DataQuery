//! CQL values: column types spelled as CQL, server values as grid cells, and
//! the text the grid and the editor send converted to the exact CQL type a
//! prepared statement expects.

use std::fmt::Write;
use std::net::IpAddr;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dq_core::result::ColumnMeta;
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult};
use num_bigint::BigInt;
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::value::{
    Counter, CqlDate, CqlDecimal, CqlDuration, CqlTime, CqlTimestamp, CqlTimeuuid, CqlValue, CqlVarint,
};
use serde_json::Value as Json;

/// `CqlDate` counts days from 2^31 days before the Unix epoch.
const DATE_EPOCH_OFFSET: i64 = 1 << 31;

/// The type as CQL spells it, e.g. `map<text, frozen<location>>`.
pub fn type_name(t: &ColumnType<'_>) -> String {
    match t {
        ColumnType::Native(n) => native_name(n).to_owned(),
        ColumnType::Collection { frozen, typ } => {
            let inner = match typ {
                CollectionType::List(e) => format!("list<{}>", type_name(e)),
                CollectionType::Set(e) => format!("set<{}>", type_name(e)),
                CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
                _ => "collection".to_owned(),
            };
            if *frozen { format!("frozen<{inner}>") } else { inner }
        }
        ColumnType::Vector { typ, dimensions } => format!("vector<{}, {dimensions}>", type_name(typ)),
        ColumnType::UserDefinedType { frozen, definition } => {
            if *frozen {
                format!("frozen<{}>", definition.name)
            } else {
                definition.name.to_string()
            }
        }
        ColumnType::Tuple(items) => format!("tuple<{}>", items.iter().map(type_name).collect::<Vec<_>>().join(", ")),
        _ => "unknown".to_owned(),
    }
}

fn native_name(n: &NativeType) -> &'static str {
    match n {
        NativeType::Ascii => "ascii",
        NativeType::Boolean => "boolean",
        NativeType::Blob => "blob",
        NativeType::Counter => "counter",
        NativeType::Date => "date",
        NativeType::Decimal => "decimal",
        NativeType::Double => "double",
        NativeType::Duration => "duration",
        NativeType::Float => "float",
        NativeType::Int => "int",
        NativeType::BigInt => "bigint",
        NativeType::Text => "text",
        NativeType::Timestamp => "timestamp",
        NativeType::Inet => "inet",
        NativeType::SmallInt => "smallint",
        NativeType::TinyInt => "tinyint",
        NativeType::Time => "time",
        NativeType::Timeuuid => "timeuuid",
        NativeType::Uuid => "uuid",
        NativeType::Varint => "varint",
        _ => "unknown",
    }
}

/// Numeric CQL type names (right-aligned in the grid).
pub fn is_numeric(type_name: &str) -> bool {
    matches!(
        type_name,
        "int" | "bigint" | "smallint" | "tinyint" | "varint" | "counter" | "decimal" | "float" | "double"
    )
}

pub fn column_meta(name: &str, t: &ColumnType<'_>) -> ColumnMeta {
    let type_name = type_name(t);
    ColumnMeta { name: name.to_owned(), numeric: is_numeric(&type_name), type_name }
}

// ------------------------------------------------------------ server → text

pub fn varint_text(v: &CqlVarint) -> String {
    BigInt::from_signed_bytes_be(v.as_signed_bytes_be_slice()).to_string()
}

/// Exact decimal text: the unscaled integer with the point placed by `scale`.
pub fn decimal_text(v: &CqlDecimal) -> String {
    let (bytes, scale) = v.as_signed_be_bytes_slice_and_exponent();
    let int = BigInt::from_signed_bytes_be(bytes);
    let (sign, digits) = match int.sign() {
        num_bigint::Sign::Minus => ("-", (-int).to_string()),
        _ => ("", int.to_string()),
    };
    if scale <= 0 {
        return format!("{sign}{digits}{}", "0".repeat(scale.unsigned_abs() as usize));
    }
    let scale = scale as usize;
    let padded =
        if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale - digits.len() + 1)) } else { digits };
    let (whole, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{frac}")
}

/// Shortest text that reads back as the same float; `NaN` and `Infinity` spelled as CQL does.
pub fn float_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        v.to_string()
    }
}

pub fn timestamp_text(v: CqlTimestamp) -> String {
    DateTime::from_timestamp_millis(v.0)
        .map_or_else(|| v.0.to_string(), |t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

pub fn date_text(v: CqlDate) -> String {
    let days = i64::from(v.0) - DATE_EPOCH_OFFSET;
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|epoch| epoch.checked_add_signed(chrono::TimeDelta::days(days)))
        .map_or_else(|| days.to_string(), |d| d.format("%Y-%m-%d").to_string())
}

pub fn time_text(v: CqlTime) -> String {
    let secs = v.0.div_euclid(1_000_000_000);
    let nanos = v.0.rem_euclid(1_000_000_000);
    format!("{:02}:{:02}:{:02}.{nanos:09}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// CQL's duration literal, e.g. `1mo2d3h4m5s6ns`.
pub fn duration_text(d: CqlDuration) -> String {
    if d.months == 0 && d.days == 0 && d.nanoseconds == 0 {
        return "0s".into();
    }
    let negative = d.months < 0 || d.days < 0 || d.nanoseconds < 0;
    let mut out = String::from(if negative { "-" } else { "" });
    let (months, days) = (d.months.unsigned_abs(), d.days.unsigned_abs());
    let mut nanos = d.nanoseconds.unsigned_abs();
    if months / 12 > 0 {
        let _ = write!(out, "{}y", months / 12);
    }
    if months % 12 > 0 {
        let _ = write!(out, "{}mo", months % 12);
    }
    if days > 0 {
        let _ = write!(out, "{days}d");
    }
    for (unit, size) in [
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
        ("ns", 1),
    ] {
        if nanos >= size {
            let _ = write!(out, "{}{unit}", nanos / size);
            nanos %= size;
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", dq_core::value::hex(bytes))
}

/// Text form of a scalar, as cqlsh shows it.
pub fn text_of(v: &CqlValue) -> String {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => s.clone(),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::Blob(b) => hex(b),
        CqlValue::Counter(c) => c.0.to_string(),
        CqlValue::Decimal(d) => decimal_text(d),
        CqlValue::Date(d) => date_text(*d),
        CqlValue::Double(f) => float_text(*f),
        CqlValue::Float(f) => float_text(f64::from(*f)),
        CqlValue::Duration(d) => duration_text(*d),
        CqlValue::Empty => String::new(),
        CqlValue::Int(i) => i.to_string(),
        CqlValue::BigInt(i) => i.to_string(),
        CqlValue::SmallInt(i) => i.to_string(),
        CqlValue::TinyInt(i) => i.to_string(),
        CqlValue::Timestamp(t) => timestamp_text(*t),
        CqlValue::Inet(ip) => ip.to_string(),
        CqlValue::Time(t) => time_text(*t),
        CqlValue::Timeuuid(u) => uuid::Uuid::from(*u).to_string(),
        CqlValue::Uuid(u) => u.to_string(),
        CqlValue::Varint(v) => varint_text(v),
        other => json_of(other).to_string(),
    }
}

/// JSON for collections, tuples and UDTs. Numbers stay JSON numbers only when
/// they read back exactly; others (decimal, varint, huge ints) are strings.
pub fn json_of(v: &CqlValue) -> Json {
    match v {
        CqlValue::Boolean(b) => Json::Bool(*b),
        CqlValue::Int(i) => Json::from(*i),
        CqlValue::BigInt(i) => Json::from(*i),
        CqlValue::SmallInt(i) => Json::from(*i),
        CqlValue::TinyInt(i) => Json::from(*i),
        CqlValue::Counter(c) => Json::from(c.0),
        CqlValue::Double(f) => {
            serde_json::Number::from_f64(*f).map_or_else(|| Json::String(float_text(*f)), Json::Number)
        }
        CqlValue::Float(f) => serde_json::Number::from_f64(f64::from(*f))
            .map_or_else(|| Json::String(float_text(f64::from(*f))), Json::Number),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            Json::Array(items.iter().map(json_of).collect())
        }
        CqlValue::Tuple(items) => Json::Array(items.iter().map(|i| i.as_ref().map_or(Json::Null, json_of)).collect()),
        CqlValue::Map(pairs) => {
            let text_keys = pairs.iter().all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_)));
            if text_keys {
                Json::Object(pairs.iter().map(|(k, v)| (text_of(k), json_of(v))).collect())
            } else {
                Json::Array(pairs.iter().map(|(k, v)| Json::Array(vec![json_of(k), json_of(v)])).collect())
            }
        }
        CqlValue::UserDefinedType { fields, .. } => Json::Object(
            fields.iter().map(|(name, value)| (name.clone(), value.as_ref().map_or(Json::Null, json_of))).collect(),
        ),
        CqlValue::Empty => Json::Null,
        other => Json::String(text_of(other)),
    }
}

/// One grid cell. `None` (null) and empty values are `Null`.
pub fn cell(v: Option<&CqlValue>, limits: CellLimits) -> Cell {
    let Some(v) = v else { return Cell::Null };
    match v {
        CqlValue::Empty => Cell::Null,
        CqlValue::Ascii(s) | CqlValue::Text(s) => Cell::text(s.clone(), limits),
        CqlValue::Boolean(b) => Cell::Bool { value: *b },
        CqlValue::Blob(b) => Cell::binary(b, limits),
        CqlValue::Counter(_)
        | CqlValue::Decimal(_)
        | CqlValue::Double(_)
        | CqlValue::Float(_)
        | CqlValue::Int(_)
        | CqlValue::BigInt(_)
        | CqlValue::SmallInt(_)
        | CqlValue::TinyInt(_)
        | CqlValue::Varint(_) => Cell::number(text_of(v)),
        CqlValue::Timestamp(t) => Cell::Temporal { value: timestamp_text(*t), zoned: true },
        CqlValue::Date(d) => Cell::Temporal { value: date_text(*d), zoned: false },
        CqlValue::Time(t) => Cell::Temporal { value: time_text(*t), zoned: false },
        CqlValue::Duration(_) | CqlValue::Inet(_) | CqlValue::Timeuuid(_) | CqlValue::Uuid(_) => {
            Cell::text(text_of(v), limits)
        }
        _ => Cell::json(json_of(v).to_string(), limits),
    }
}

// ------------------------------------------------------------ text → server

/// Converts the text for parameter `n` (1-based) to `t`. `None` is null.
pub fn param(n: usize, text: Option<&str>, t: &ColumnType<'_>) -> DbResult<Option<CqlValue>> {
    let Some(text) = text else { return Ok(None) };
    from_text(text, t).map(Some).map_err(|e| DbError::invalid(format!("Parameter {n} ({}): {e}", type_name(t))))
}

/// Text to a value of `t`. Collections, tuples and UDTs are JSON.
pub fn from_text(text: &str, t: &ColumnType<'_>) -> Result<CqlValue, String> {
    if let ColumnType::Native(n) = t {
        return native(text, n);
    }
    let json: Json = serde_json::from_str(text).map_err(|_| format!("`{text}` is not valid JSON"))?;
    from_json(&json, t)
}

fn native(text: &str, n: &NativeType) -> Result<CqlValue, String> {
    let t = text.trim();
    let parse_err = |what: &str| format!("`{text}` is not {what}");
    Ok(match n {
        NativeType::Text => CqlValue::Text(text.to_owned()),
        NativeType::Ascii if text.is_ascii() => CqlValue::Ascii(text.to_owned()),
        NativeType::Ascii => return Err("ascii values cannot contain non-ASCII characters".into()),
        NativeType::Boolean => match t.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" => CqlValue::Boolean(true),
            "false" | "f" | "0" | "no" | "n" => CqlValue::Boolean(false),
            _ => return Err(parse_err("a boolean")),
        },
        NativeType::Int => CqlValue::Int(t.parse().map_err(|_| parse_err("an int"))?),
        NativeType::BigInt => CqlValue::BigInt(t.parse().map_err(|_| parse_err("a bigint"))?),
        NativeType::SmallInt => CqlValue::SmallInt(t.parse().map_err(|_| parse_err("a smallint"))?),
        NativeType::TinyInt => CqlValue::TinyInt(t.parse().map_err(|_| parse_err("a tinyint"))?),
        NativeType::Counter => CqlValue::Counter(Counter(t.parse().map_err(|_| parse_err("a counter value"))?)),
        NativeType::Varint => {
            let v: BigInt = t.parse().map_err(|_| parse_err("a whole number"))?;
            CqlValue::Varint(CqlVarint::from_signed_bytes_be(v.to_signed_bytes_be()))
        }
        NativeType::Decimal => CqlValue::Decimal(parse_decimal(t).ok_or_else(|| parse_err("a decimal number"))?),
        NativeType::Double => CqlValue::Double(parse_float(t).ok_or_else(|| parse_err("a number"))?),
        NativeType::Float => CqlValue::Float(parse_float(t).ok_or_else(|| parse_err("a number"))? as f32),
        NativeType::Uuid => CqlValue::Uuid(uuid::Uuid::parse_str(t).map_err(|_| parse_err("a UUID"))?),
        NativeType::Timeuuid => {
            let u = uuid::Uuid::parse_str(t).map_err(|_| parse_err("a UUID"))?;
            if u.get_version_num() != 1 {
                return Err("a timeuuid must be a version 1 UUID".into());
            }
            CqlValue::Timeuuid(CqlTimeuuid::from(u))
        }
        NativeType::Inet => CqlValue::Inet(t.parse::<IpAddr>().map_err(|_| parse_err("an IP address"))?),
        NativeType::Blob => CqlValue::Blob(parse_hex(t).ok_or_else(|| parse_err("hex bytes (0x…)"))?),
        NativeType::Timestamp => CqlValue::Timestamp(CqlTimestamp(
            parse_timestamp(t)
                .ok_or_else(|| parse_err("a timestamp (2026-10-14 09:59:12+0000, RFC 3339 or milliseconds)"))?,
        )),
        NativeType::Date => {
            let d = NaiveDate::parse_from_str(t, "%Y-%m-%d").map_err(|_| parse_err("a date (YYYY-MM-DD)"))?;
            let days = d.signed_duration_since(NaiveDate::default()).num_days() + DATE_EPOCH_OFFSET;
            CqlValue::Date(CqlDate(u32::try_from(days).map_err(|_| parse_err("a supported date"))?))
        }
        NativeType::Time => {
            let time = NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
                .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M"))
                .map_err(|_| parse_err("a time (HH:MM:SS.fff)"))?;
            CqlValue::Time(CqlTime(
                i64::from(time.num_seconds_from_midnight()) * 1_000_000_000 + i64::from(time.nanosecond()),
            ))
        }
        NativeType::Duration => {
            CqlValue::Duration(parse_duration(t).ok_or_else(|| parse_err("a duration (e.g. 1h30m)"))?)
        }
        _ => return Err("this type cannot be set from text".into()),
    })
}

fn parse_float(t: &str) -> Option<f64> {
    match t {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => t.parse().ok().filter(|v: &f64| v.is_finite()),
    }
}

/// `123.4500`, `-0.5`, `1.2E+3` → unscaled integer and scale.
fn parse_decimal(t: &str) -> Option<CqlDecimal> {
    let (mantissa, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i32>().ok()?),
        None => (t, 0),
    };
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{frac}");
    let body = digits.strip_prefix(['-', '+']).unwrap_or(&digits);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int: BigInt = digits.parse().ok()?;
    let scale = i32::try_from(frac.len()).ok()?.checked_sub(exp)?;
    Some(CqlDecimal::from_signed_be_bytes_and_exponent(int.to_signed_bytes_be(), scale))
}

fn parse_hex(t: &str) -> Option<Vec<u8>> {
    let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

/// RFC 3339, `YYYY-MM-DD HH:MM:SS[.fff][Z|±hh[:mm]|±hhmm]` (UTC without a zone),
/// `YYYY-MM-DD` (midnight UTC) or milliseconds since the epoch.
fn parse_timestamp(t: &str) -> Option<i64> {
    if let Ok(ms) = t.parse::<i64>() {
        return Some(ms);
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(d.timestamp_millis());
    }
    let t = t.replace('T', " ");
    for zoned in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M%#z"] {
        if let Ok(d) = DateTime::parse_from_str(&t, zoned) {
            return Some(d.timestamp_millis());
        }
    }
    let t = t.strip_suffix('Z').unwrap_or(&t);
    for naive in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, naive) {
            return Some(d.and_utc().timestamp_millis());
        }
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d").ok().map(|d| d.and_time(NaiveTime::MIN).and_utc().timestamp_millis())
}

/// `1y2mo3w4d5h6m7s8ms9us10ns`, optionally negative.
fn parse_duration(t: &str) -> Option<CqlDuration> {
    let (negative, mut rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    if rest.is_empty() {
        return None;
    }
    let (mut months, mut days, mut nanos) = (0i64, 0i64, 0i64);
    while !rest.is_empty() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let n: i64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest.chars().take_while(|c| c.is_ascii_alphabetic() || *c == 'µ').map(char::len_utf8).sum();
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        match unit.to_ascii_lowercase().as_str() {
            "y" => months += n * 12,
            "mo" => months += n,
            "w" => days += n * 7,
            "d" => days += n,
            "h" => nanos += n * 3_600_000_000_000,
            "m" => nanos += n * 60_000_000_000,
            "s" => nanos += n * 1_000_000_000,
            "ms" => nanos += n * 1_000_000,
            "us" | "µs" => nanos += n * 1_000,
            "ns" => nanos += n,
            _ => return None,
        }
    }
    let sign = if negative { -1 } else { 1 };
    Some(CqlDuration {
        months: i32::try_from(months * sign).ok()?,
        days: i32::try_from(days * sign).ok()?,
        nanoseconds: nanos * sign,
    })
}

fn json_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// JSON for a collection, tuple or UDT, element by element.
fn from_json(v: &Json, t: &ColumnType<'_>) -> Result<CqlValue, String> {
    let element = |v: &Json, t: &ColumnType<'_>| -> Result<CqlValue, String> {
        if v.is_null() {
            return Err("collections cannot hold null".into());
        }
        match t {
            ColumnType::Native(n) => native(&json_text(v), n),
            other => from_json(v, other),
        }
    };
    let optional = |v: &Json, t: &ColumnType<'_>| if v.is_null() { Ok(None) } else { element(v, t).map(Some) };
    let array = || v.as_array().ok_or_else(|| "expected a JSON array".to_owned());
    match t {
        ColumnType::Native(n) => native(&json_text(v), n),
        ColumnType::Collection { typ: CollectionType::List(e), .. } => {
            Ok(CqlValue::List(array()?.iter().map(|x| element(x, e)).collect::<Result<_, _>>()?))
        }
        ColumnType::Collection { typ: CollectionType::Set(e), .. } => {
            Ok(CqlValue::Set(array()?.iter().map(|x| element(x, e)).collect::<Result<_, _>>()?))
        }
        ColumnType::Collection { typ: CollectionType::Map(k, val), .. } => {
            let pairs = match v {
                Json::Object(map) => map
                    .iter()
                    .map(|(key, x)| Ok((element(&Json::String(key.clone()), k)?, element(x, val)?)))
                    .collect::<Result<Vec<_>, String>>()?,
                Json::Array(items) => items
                    .iter()
                    .map(|pair| match pair.as_array().map(Vec::as_slice) {
                        Some([key, x]) => Ok((element(key, k)?, element(x, val)?)),
                        _ => Err("map entries are [key, value] pairs".to_owned()),
                    })
                    .collect::<Result<Vec<_>, String>>()?,
                _ => return Err("expected a JSON object".into()),
            };
            Ok(CqlValue::Map(pairs))
        }
        ColumnType::Vector { typ, dimensions } => {
            let items = array()?;
            if items.len() != usize::from(*dimensions) {
                return Err(format!("expected {dimensions} values"));
            }
            Ok(CqlValue::Vector(items.iter().map(|x| element(x, typ)).collect::<Result<_, _>>()?))
        }
        ColumnType::Tuple(types) => {
            let items = array()?;
            if items.len() != types.len() {
                return Err(format!("expected {} values", types.len()));
            }
            Ok(CqlValue::Tuple(items.iter().zip(types).map(|(x, t)| optional(x, t)).collect::<Result<_, _>>()?))
        }
        ColumnType::UserDefinedType { definition, .. } => {
            let map = v.as_object().ok_or_else(|| "expected a JSON object".to_owned())?;
            if let Some(unknown) = map.keys().find(|k| !definition.field_types.iter().any(|(n, _)| n == *k)) {
                return Err(format!("{} has no field `{unknown}`", definition.name));
            }
            let fields = definition
                .field_types
                .iter()
                .map(|(name, t)| Ok((name.to_string(), map.get(name.as_ref()).map_or(Ok(None), |x| optional(x, t))?)))
                .collect::<Result<_, String>>()?;
            Ok(CqlValue::UserDefinedType {
                keyspace: definition.keyspace.to_string(),
                name: definition.name.to_string(),
                fields,
            })
        }
        _ => Err("this type cannot be set from text".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use scylla::frame::response::result::UserDefinedType;

    use super::*;

    const fn n(t: NativeType) -> ColumnType<'static> {
        ColumnType::Native(t)
    }

    #[test]
    fn numbers_stay_exact() {
        let v = from_text("9007199254740993123456789", &n(NativeType::Varint)).unwrap();
        assert_eq!(text_of(&v), "9007199254740993123456789");
        for d in ["120.50", "-0.005", "0.0", "123", "-7.25"] {
            assert_eq!(text_of(&from_text(d, &n(NativeType::Decimal)).unwrap()), d);
        }
        assert_eq!(text_of(&from_text("1.5E+3", &n(NativeType::Decimal)).unwrap()), "1500");
        assert_eq!(cell(Some(&CqlValue::Double(61.25)), CellLimits::GRID), Cell::number("61.25"));
        assert_eq!(text_of(&CqlValue::Double(f64::NAN)), "NaN");
        assert!(from_text("1.2.3", &n(NativeType::Decimal)).is_err());
        assert!(from_text("70000", &n(NativeType::SmallInt)).is_err());
    }

    #[test]
    fn temporal_values() {
        let ts = from_text("2026-10-14 09:59:12+0000", &n(NativeType::Timestamp)).unwrap();
        assert_eq!(text_of(&ts), "2026-10-14T09:59:12.000Z");
        assert_eq!(from_text("2026-10-14T09:59:12Z", &n(NativeType::Timestamp)).unwrap(), ts);
        assert_eq!(from_text("2026-10-14 11:59:12+02:00", &n(NativeType::Timestamp)).unwrap(), ts);
        assert_eq!(text_of(&from_text("2026-10-14", &n(NativeType::Date)).unwrap()), "2026-10-14");
        assert_eq!(text_of(&from_text("1969-12-31", &n(NativeType::Date)).unwrap()), "1969-12-31");
        assert_eq!(text_of(&from_text("09:05:01.5", &n(NativeType::Time)).unwrap()), "09:05:01.500000000");
        let d = from_text("1y2mo3d4h5m6s7ms", &n(NativeType::Duration)).unwrap();
        assert_eq!(text_of(&d), "1y2mo3d4h5m6s7ms");
        assert!(matches!(cell(Some(&ts), CellLimits::GRID), Cell::Temporal { zoned: true, .. }));
        assert!(from_text("yesterday", &n(NativeType::Timestamp)).is_err());
    }

    #[test]
    fn other_scalars() {
        assert_eq!(from_text("0xCAFE", &n(NativeType::Blob)).unwrap(), CqlValue::Blob(vec![0xca, 0xfe]));
        assert!(from_text("0xabc", &n(NativeType::Blob)).is_err());
        assert_eq!(from_text("yes", &n(NativeType::Boolean)).unwrap(), CqlValue::Boolean(true));
        assert!(from_text("::1", &n(NativeType::Inet)).is_ok());
        let u = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        assert_eq!(text_of(&from_text(u, &n(NativeType::Timeuuid)).unwrap()), u);
        assert!(from_text("550e8400-e29b-41d4-a716-446655440000", &n(NativeType::Timeuuid)).is_err());
        assert!(from_text("Zoë", &n(NativeType::Ascii)).is_err());
        let e = param(2, Some("x"), &n(NativeType::Int)).unwrap_err();
        assert!(e.message.contains("Parameter 2 (int)"));
        assert_eq!(param(1, None, &n(NativeType::Int)).unwrap(), None);
    }

    #[test]
    fn collections_and_udts() {
        let map = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(Box::new(n(NativeType::Text)), Box::new(n(NativeType::Int))),
        };
        assert_eq!(type_name(&map), "map<text, int>");
        let v = from_text(r#"{"a": 1, "b": "2"}"#, &map).unwrap();
        assert_eq!(json_of(&v), serde_json::json!({"a": 1, "b": 2}));
        let udt = ColumnType::UserDefinedType {
            frozen: true,
            definition: Arc::new(UserDefinedType {
                name: Cow::Borrowed("location"),
                keyspace: Cow::Borrowed("ks"),
                field_types: vec![
                    (Cow::Borrowed("lat"), n(NativeType::Double)),
                    (Cow::Borrowed("label"), n(NativeType::Text)),
                ],
            }),
        };
        assert_eq!(type_name(&udt), "frozen<location>");
        let v = from_text(r#"{"lat": 48.85}"#, &udt).unwrap();
        assert_eq!(json_of(&v), serde_json::json!({"lat": 48.85, "label": null}));
        assert!(from_text(r#"{"nope": 1}"#, &udt).is_err());
        let set = ColumnType::Collection { frozen: false, typ: CollectionType::Set(Box::new(n(NativeType::Varint))) };
        let v = from_text(r#"["123456789012345678901234567890"]"#, &set).unwrap();
        assert_eq!(json_of(&v), serde_json::json!(["123456789012345678901234567890"]));
        assert!(matches!(cell(Some(&v), CellLimits::GRID), Cell::Json { .. }));
        let bad = from_text("[1, null]", &set);
        assert!(bad.is_err());
        let ints = CqlValue::Map(vec![(CqlValue::Int(1), CqlValue::Text("x".into()))]);
        assert_eq!(json_of(&ints), serde_json::json!([[1, "x"]]));
    }
}
