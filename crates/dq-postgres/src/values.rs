//! Decodes PostgreSQL binary values into [`Cell`]s without losing precision.
//!
//! Integers and `numeric` become exact strings, timestamps become RFC 3339,
//! and anything without a decoder becomes [`Cell::Unsupported`].

use std::error::Error;
use std::fmt::Write;

use bytes::BytesMut;
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits, hex};
use fallible_iterator::FallibleIterator;
use postgres_protocol::types as wire;
use tokio_postgres::Row;
use tokio_postgres::types::{Format, FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};

type DecodeResult<T> = Result<T, Box<dyn Error + Sync + Send>>;

/// A parameter sent in text format, so the server parses it with the input
/// function of the type it inferred for the placeholder. `None` is NULL.
#[derive(Debug)]
pub struct TextParam<'a>(pub Option<&'a str>);

impl ToSql for TextParam<'_> {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> DecodeResult<IsNull> {
        Ok(match self.0 {
            None => IsNull::Yes,
            Some(v) => {
                out.extend_from_slice(v.as_bytes());
                IsNull::No
            }
        })
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

/// Accepts any value and keeps the raw bytes, so decoding happens here.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> DecodeResult<Self> {
        Ok(Self(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> DecodeResult<Self> {
        Ok(Self(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

pub fn column_meta(columns: &[tokio_postgres::Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_owned(),
            type_name: type_label(c.type_()),
            numeric: is_numeric(c.type_()),
        })
        .collect()
}

pub fn decode_row(row: &Row, limits: CellLimits) -> CellRow {
    (0..row.len())
        .map(|i| match row.try_get::<_, Raw<'_>>(i) {
            Ok(Raw(raw)) => decode(row.columns()[i].type_(), raw, limits),
            Err(_) => Cell::Unsupported { type_name: type_label(row.columns()[i].type_()), value: None },
        })
        .collect()
}

fn type_label(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(elem) => format!("{}[]", elem.name()),
        _ => ty.name().to_owned(),
    }
}

pub fn is_numeric(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC | Type::MONEY | Type::OID
    )
}

/// Type OIDs this module decodes natively. Other columns are read as `::text`
/// in the table view.
pub fn is_native_oid(oid: u32) -> bool {
    Type::from_oid(oid).is_some_and(|t| is_native(&t))
}

fn is_native(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::BOOL
            | Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::NUMERIC
            | Type::MONEY
            | Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::CHAR
            | Type::XML
            | Type::JSON
            | Type::JSONB
            | Type::UUID
            | Type::BYTEA
            | Type::DATE
            | Type::TIME
            | Type::TIMETZ
            | Type::TIMESTAMP
            | Type::TIMESTAMPTZ
            | Type::INTERVAL
            | Type::INET
            | Type::CIDR
            | Type::MACADDR
            | Type::BIT
            | Type::VARBIT
    )
}

pub fn decode(ty: &Type, raw: Option<&[u8]>, limits: CellLimits) -> Cell {
    let Some(raw) = raw else { return Cell::Null };
    decode_value(ty, raw, limits).unwrap_or_else(|_| Cell::Unsupported { type_name: type_label(ty), value: None })
}

fn decode_value(ty: &Type, raw: &[u8], limits: CellLimits) -> DecodeResult<Cell> {
    Ok(match *ty {
        Type::BOOL => Cell::Bool { value: wire::bool_from_sql(raw)? },
        Type::INT2 => Cell::number(wire::int2_from_sql(raw)?.to_string()),
        Type::INT4 => Cell::number(wire::int4_from_sql(raw)?.to_string()),
        Type::INT8 => Cell::number(wire::int8_from_sql(raw)?.to_string()),
        Type::OID => Cell::number(wire::oid_from_sql(raw)?.to_string()),
        Type::FLOAT4 => Cell::number(float(f64::from(wire::float4_from_sql(raw)?))),
        Type::FLOAT8 => Cell::number(float(wire::float8_from_sql(raw)?)),
        Type::NUMERIC => Cell::number(numeric(raw)?),
        Type::MONEY => Cell::number(money(wire::int8_from_sql(raw)?)),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::XML | Type::UNKNOWN => {
            Cell::text(wire::text_from_sql(raw)?, limits)
        }
        Type::CHAR => Cell::text(char::from(*raw.first().ok_or("empty char")?).to_string(), limits),
        Type::JSON => Cell::json(wire::text_from_sql(raw)?, limits),
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => Cell::json(std::str::from_utf8(rest)?, limits),
            _ => return Err("unknown jsonb version".into()),
        },
        Type::UUID => Cell::text(uuid(wire::uuid_from_sql(raw)?), limits),
        Type::BYTEA => Cell::binary(raw, limits),
        Type::DATE => Cell::Temporal { value: date(wire::date_from_sql(raw)?), zoned: false },
        Type::TIME => Cell::Temporal { value: time(wire::time_from_sql(raw)?)?, zoned: false },
        Type::TIMETZ => Cell::Temporal { value: timetz(raw)?, zoned: true },
        Type::TIMESTAMP => Cell::Temporal { value: timestamp(wire::timestamp_from_sql(raw)?, false)?, zoned: false },
        Type::TIMESTAMPTZ => Cell::Temporal { value: timestamp(wire::timestamp_from_sql(raw)?, true)?, zoned: true },
        Type::INTERVAL => Cell::text(interval(raw)?, limits),
        Type::INET | Type::CIDR => Cell::text(inet(raw, *ty == Type::CIDR)?, limits),
        Type::MACADDR => Cell::text(raw.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"), limits),
        Type::BIT | Type::VARBIT => Cell::text(bits(raw)?, limits),
        _ => match ty.kind() {
            Kind::Enum(_) => Cell::text(wire::text_from_sql(raw)?, limits),
            Kind::Domain(base) => decode_value(base, raw, limits)?,
            Kind::Array(elem) => Cell::text(array(elem, raw)?, limits),
            // Extension types such as citext send text; anything binary stays opaque.
            _ => Cell::Unsupported { type_name: type_label(ty), value: printable(raw) },
        },
    })
}

fn printable(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    (!s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))).then(|| s.to_owned())
}

fn float(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        v.to_string()
    }
}

fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// Binary `numeric`: ndigits, weight, sign, dscale, then base-10000 digits.
pub fn numeric(raw: &[u8]) -> DecodeResult<String> {
    let word = |i: usize| -> DecodeResult<u16> {
        raw.get(i * 2..i * 2 + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| "short numeric".into())
    };
    let ndigits = word(0)? as usize;
    let weight = i32::from(word(1)? as i16);
    let sign = word(2)?;
    let dscale = word(3)? as usize;
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        _ => {}
    }
    let digits: Vec<u16> = (0..ndigits).map(|i| word(4 + i)).collect::<DecodeResult<_>>()?;
    let digit = |k: i32| -> u16 { usize::try_from(k).ok().and_then(|k| digits.get(k).copied()).unwrap_or(0) };

    let mut out = String::new();
    if sign == 0x4000 && ndigits > 0 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for k in 0..=weight {
            if k == 0 {
                write!(out, "{}", digit(k))?;
            } else {
                write!(out, "{:04}", digit(k))?;
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut fraction = String::with_capacity(dscale + 4);
        let groups = dscale.div_ceil(4) as i32;
        for p in 0..groups {
            write!(fraction, "{:04}", digit(weight + 1 + p))?;
        }
        fraction.truncate(dscale);
        out.push_str(&fraction);
    }
    Ok(out)
}

fn uuid(bytes: [u8; 16]) -> String {
    let h = hex(&bytes);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1).expect("valid date").and_hms_opt(0, 0, 0).expect("valid time")
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        d => (epoch().date() + Duration::days(i64::from(d))).format("%Y-%m-%d").to_string(),
    }
}

fn fraction(micros: u32) -> String {
    if micros == 0 { String::new() } else { format!(".{micros:06}").trim_end_matches('0').to_owned() }
}

fn time(micros: i64) -> DecodeResult<String> {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000) as u32;
    let t = NaiveTime::from_num_seconds_from_midnight_opt(u32::try_from(secs)?, 0).ok_or("time out of range")?;
    Ok(format!("{}{}", t.format("%H:%M:%S"), fraction(frac)))
}

fn offset(seconds_east: i32) -> String {
    let sign = if seconds_east < 0 { '-' } else { '+' };
    let abs = seconds_east.unsigned_abs();
    let (h, m, s) = (abs / 3600, abs % 3600 / 60, abs % 60);
    if s == 0 { format!("{sign}{h:02}:{m:02}") } else { format!("{sign}{h:02}:{m:02}:{s:02}") }
}

fn timetz(raw: &[u8]) -> DecodeResult<String> {
    if raw.len() != 12 {
        return Err("bad timetz".into());
    }
    let micros = i64::from_be_bytes(raw[0..8].try_into()?);
    // The wire format stores seconds WEST of UTC.
    let west = i32::from_be_bytes(raw[8..12].try_into()?);
    Ok(format!("{}{}", time(micros)?, offset(-west)))
}

/// `timestamptz` is an instant: rendered in UTC as RFC 3339. `timestamp` has no zone.
fn timestamp(micros: i64, zoned: bool) -> DecodeResult<String> {
    match micros {
        i64::MAX => return Ok("infinity".into()),
        i64::MIN => return Ok("-infinity".into()),
        _ => {}
    }
    let dt = epoch().checked_add_signed(Duration::microseconds(micros)).ok_or("timestamp out of range")?;
    let frac = fraction((micros.rem_euclid(1_000_000)) as u32);
    Ok(if zoned {
        // The wire value is already UTC.
        format!("{}{frac}+00:00", dt.format("%Y-%m-%dT%H:%M:%S"))
    } else {
        format!("{}{frac}", dt.format("%Y-%m-%d %H:%M:%S"))
    })
}

/// PostgreSQL's default `postgres` interval style, e.g. `1 year 2 mons 3 days 04:05:06`.
fn interval(raw: &[u8]) -> DecodeResult<String> {
    if raw.len() != 16 {
        return Err("bad interval".into());
    }
    let micros = i64::from_be_bytes(raw[0..8].try_into()?);
    let days = i32::from_be_bytes(raw[8..12].try_into()?);
    let months = i32::from_be_bytes(raw[12..16].try_into()?);
    let mut parts = Vec::new();
    let unit = |n: i32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if months / 12 != 0 {
        parts.push(unit(months / 12, "year", "years"));
    }
    if months % 12 != 0 {
        parts.push(unit(months % 12, "mon", "mons"));
    }
    if days != 0 {
        parts.push(unit(days, "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        parts.push(format!(
            "{sign}{:02}:{:02}:{:02}{}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60,
            fraction((abs % 1_000_000) as u32)
        ));
    }
    Ok(parts.join(" "))
}

fn inet(raw: &[u8], cidr: bool) -> DecodeResult<String> {
    let value = wire::inet_from_sql(raw)?;
    let full = if value.addr().is_ipv4() { 32 } else { 128 };
    Ok(if cidr || value.netmask() != full {
        format!("{}/{}", value.addr(), value.netmask())
    } else {
        value.addr().to_string()
    })
}

fn bits(raw: &[u8]) -> DecodeResult<String> {
    let value = wire::varbit_from_sql(raw)?;
    let len = value.len();
    Ok(value
        .bytes()
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| if b >> i & 1 == 1 { '1' } else { '0' }))
        .take(len)
        .collect())
}

/// Array text form, e.g. `{1,NULL,"a b"}`; nested dimensions nest braces.
fn array(elem: &Type, raw: &[u8]) -> DecodeResult<String> {
    let array = wire::array_from_sql(raw)?;
    let dims: Vec<usize> = array.dimensions().map(|d| Ok(usize::try_from(d.len).unwrap_or(0))).collect()?;
    let values: Vec<String> = array
        .values()
        .map(|v| {
            Ok(match decode(elem, v, CellLimits::FULL).to_text() {
                None => "NULL".to_owned(),
                Some(s) => quote_array_element(&s),
            })
        })
        .collect()?;
    if dims.is_empty() {
        return Ok("{}".into());
    }
    let mut out = String::new();
    write_dims(&dims, &values, &mut 0, &mut out);
    Ok(out)
}

fn write_dims(dims: &[usize], values: &[String], cursor: &mut usize, out: &mut String) {
    out.push('{');
    for i in 0..dims[0] {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            write_dims(&dims[1..], values, cursor, out);
        } else {
            out.push_str(values.get(*cursor).map_or("NULL", String::as_str));
            *cursor += 1;
        }
    }
    out.push('}');
}

fn quote_array_element(s: &str) -> String {
    let needs = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
    if needs { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_bytes(ndigits: u16, weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        for w in [ndigits, weight as u16, sign, dscale].iter().chain(digits) {
            v.extend_from_slice(&w.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_exact() {
        // 1234567890123456789012345678.0123456789 → groups of 4 around the point.
        let raw = numeric_bytes(10, 6, 0, 10, &[1234, 5678, 9012, 3456, 7890, 1234, 5678, 123, 4567, 8900]);
        assert_eq!(numeric(&raw).unwrap(), "1234567890123456789012345678.0123456789");
        // -0.0000000001 (weight -3: 0.0000 0000 01)
        let raw = numeric_bytes(1, -3, 0x4000, 10, &[100]);
        assert_eq!(numeric(&raw).unwrap(), "-0.0000000001");
        assert_eq!(numeric(&numeric_bytes(0, 0, 0, 2, &[])).unwrap(), "0.00");
        assert_eq!(numeric(&numeric_bytes(1, 1, 0, 0, &[5])).unwrap(), "50000");
        assert_eq!(numeric(&numeric_bytes(0, 0, 0xC000, 0, &[])).unwrap(), "NaN");
    }

    #[test]
    fn temporal_formats() {
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(timestamp(1_000, false).unwrap(), "2000-01-01 00:00:00.001");
        assert_eq!(timestamp(-1, true).unwrap(), "1999-12-31T23:59:59.999999+00:00");
        assert_eq!(timestamp(i64::MAX, true).unwrap(), "infinity");
        assert_eq!(time(3_723_000_000).unwrap(), "01:02:03");
        let mut tz = 3_600_000_000i64.to_be_bytes().to_vec();
        tz.extend_from_slice(&(-7200i32).to_be_bytes());
        assert_eq!(timetz(&tz).unwrap(), "01:00:00+02:00");
    }

    #[test]
    fn interval_style() {
        let enc = |micros: i64, days: i32, months: i32| {
            let mut v = micros.to_be_bytes().to_vec();
            v.extend_from_slice(&days.to_be_bytes());
            v.extend_from_slice(&months.to_be_bytes());
            v
        };
        assert_eq!(interval(&enc(14_706_000_000, 3, 14)).unwrap(), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(interval(&enc(-1_000_000, 0, 0)).unwrap(), "-00:00:01");
        assert_eq!(interval(&enc(0, 1, 0)).unwrap(), "1 day");
        assert_eq!(interval(&enc(0, 0, 0)).unwrap(), "00:00:00");
    }

    #[test]
    fn misc() {
        assert_eq!(money(-12345), "-123.45");
        assert_eq!(float(0.1), "0.1");
        assert_eq!(quote_array_element("b c"), "\"b c\"");
        assert_eq!(quote_array_element("NULL"), "\"NULL\"");
        assert_eq!(uuid([0xa0; 16]), "a0a0a0a0-a0a0-a0a0-a0a0-a0a0a0a0a0a0");
    }
}
