//! Replies as `redis-cli` prints them (the console) and as grid rows (the editor).

use std::fmt::Write;

use dq_core::result::{ColumnMeta, Row};
use dq_core::value::{Cell, CellLimits};
use redis::Value;

/// A reply printed like `redis-cli` in a terminal: `"text"`, `(integer) 1`,
/// `(nil)`, `(empty array)` and numbered, indented arrays.
pub(crate) fn format_cli(value: &Value) -> String {
    let mut out = String::new();
    format_into(value, "", &mut out);
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn format_into(value: &Value, prefix: &str, out: &mut String) {
    match value {
        Value::Nil => out.push_str("(nil)\n"),
        Value::Int(i) => {
            let _ = writeln!(out, "(integer) {i}");
        }
        Value::BulkString(b) => {
            out.push_str(&quoted(b));
            out.push('\n');
        }
        Value::SimpleString(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Okay => out.push_str("OK\n"),
        Value::Double(d) => {
            let _ = writeln!(out, "(double) {}", format_double(*d));
        }
        Value::Boolean(b) => {
            let _ = writeln!(out, "({b})");
        }
        Value::VerbatimString { text, .. } => {
            out.push_str(text);
            out.push('\n');
        }
        Value::BigNumber(n) => {
            let _ = writeln!(out, "(big number) {}", String::from_utf8_lossy(n));
        }
        Value::ServerError(e) => {
            let _ = writeln!(out, "(error) {}", error_text(e));
        }
        Value::Attribute { data, .. } => format_into(data, prefix, out),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            let sep = if matches!(value, Value::Set(_)) { '~' } else { ')' };
            format_list(items.iter().map(|v| (v, None)), items.len(), sep, prefix, out);
        }
        Value::Map(pairs) => format_list(pairs.iter().map(|(k, v)| (v, Some(k))), pairs.len(), '#', prefix, out),
        _ => out.push_str("(unsupported reply)\n"),
    }
}

/// `redis-cli`'s numbered list: the index is right-aligned to the widest one,
/// and nested lines are indented past it.
fn format_list<'a>(
    items: impl Iterator<Item = (&'a Value, Option<&'a Value>)>,
    len: usize,
    sep: char,
    prefix: &str,
    out: &mut String,
) {
    if len == 0 {
        out.push_str(if sep == '~' { "(empty set)\n" } else { "(empty array)\n" });
        return;
    }
    let width = len.to_string().len();
    let inner = format!("{prefix}{}", " ".repeat(width + 2));
    for (i, (value, key)) in items.enumerate() {
        let _ = write!(out, "{}{:>width$}{sep} ", if i == 0 { "" } else { prefix }, i + 1);
        if let Some(key) = key {
            let mut k = String::new();
            format_into(key, &inner, &mut k);
            out.push_str(k.trim_end_matches('\n'));
            out.push_str(" => ");
        }
        format_into(value, &inner, out);
    }
}

fn error_text(e: &redis::ServerError) -> String {
    match e.details() {
        Some(d) => format!("{} {d}", e.code()),
        None => e.code().to_owned(),
    }
}

/// `redis-cli`'s quoted form: printable ASCII as is, `\"`, `\\`, `\n`, `\r`,
/// `\t`, `\a`, `\b`, and `\xNN` for every other byte.
pub(crate) fn quoted(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 2);
    s.push('"');
    for &b in bytes {
        match b {
            b'\\' => s.push_str("\\\\"),
            b'"' => s.push_str("\\\""),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            7 => s.push_str("\\a"),
            8 => s.push_str("\\b"),
            0x20..=0x7e => s.push(b as char),
            _ => {
                let _ = write!(s, "\\x{b:02x}");
            }
        }
    }
    s.push('"');
    s
}

fn format_double(d: f64) -> String {
    if d.is_infinite() { if d > 0.0 { "inf".into() } else { "-inf".into() } } else { d.to_string() }
}

/// One reply value as a grid cell. Nested values become compact JSON.
pub(crate) fn cell(value: &Value, limits: CellLimits) -> Cell {
    match value {
        Value::Nil => Cell::Null,
        Value::Int(i) => Cell::number(i.to_string()),
        Value::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => Cell::text(s, limits),
            Err(_) => Cell::binary(b, limits),
        },
        Value::SimpleString(s) => Cell::text(s.clone(), limits),
        Value::Okay => Cell::text("OK", limits),
        Value::Double(d) => Cell::number(format_double(*d)),
        Value::Boolean(b) => Cell::Bool { value: *b },
        Value::VerbatimString { text, .. } => Cell::text(text.clone(), limits),
        Value::BigNumber(n) => Cell::number(String::from_utf8_lossy(n).into_owned()),
        Value::ServerError(e) => Cell::text(format!("(error) {}", error_text(e)), limits),
        Value::Attribute { data, .. } => cell(data, limits),
        Value::Array(_) | Value::Set(_) | Value::Map(_) | Value::Push { .. } => {
            Cell::json(json(value).to_string(), limits)
        }
        _ => Cell::Unsupported { type_name: "reply".into(), value: None },
    }
}

fn json(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Int(i) => J::from(*i),
        Value::Boolean(b) => J::Bool(*b),
        Value::Double(d) => serde_json::Number::from_f64(*d).map_or_else(|| J::String(format_double(*d)), J::Number),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            J::Array(items.iter().map(json).collect())
        }
        Value::Map(pairs) => J::Array(pairs.iter().map(|(k, v)| J::Array(vec![json(k), json(v)])).collect()),
        Value::Attribute { data, .. } => json(data),
        other => J::String(cell(other, CellLimits::FULL).to_text().unwrap_or_default()),
    }
}

/// Commands whose flat array replies are field/value pairs.
fn pairs_reply(args: &[Vec<u8>]) -> bool {
    let word = |i: usize| args.get(i).map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
    let name = word(0);
    name == "HGETALL"
        || (name == "CONFIG" && word(1) == "GET")
        || args.iter().skip(1).any(|a| a.eq_ignore_ascii_case(b"WITHSCORES") || a.eq_ignore_ascii_case(b"WITHVALUES"))
}

fn meta(name: &str, numeric: bool) -> ColumnMeta {
    ColumnMeta { name: name.into(), type_name: String::new(), numeric }
}

fn is_list(v: &Value) -> bool {
    matches!(v, Value::Array(_) | Value::Set(_) | Value::Map(_) | Value::Push { .. })
}

/// A reply as grid rows: a scalar in one `reply` cell, a flat array as `#`/`value`,
/// pair replies (`HGETALL`, `CONFIG GET`, `WITHSCORES`) and maps as `field`/`value`,
/// an array of arrays as columns `1`, `2`, ….
pub(crate) fn to_grid(args: &[Vec<u8>], value: &Value, limits: CellLimits) -> (Vec<ColumnMeta>, Vec<Row>) {
    let value = match value {
        Value::Attribute { data, .. } => data,
        v => v,
    };
    match value {
        Value::Map(pairs) => (
            vec![meta("field", false), meta("value", false)],
            pairs.iter().map(|(k, v)| vec![cell(k, limits), cell(v, limits)]).collect(),
        ),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            if items.iter().any(is_list) {
                let width = items
                    .iter()
                    .map(|i| match i {
                        Value::Array(x) | Value::Set(x) => x.len(),
                        _ => 1,
                    })
                    .max()
                    .unwrap_or(1);
                let columns = (1..=width).map(|n| meta(&n.to_string(), false)).collect();
                let rows = items
                    .iter()
                    .map(|item| {
                        let mut row: Row = match item {
                            Value::Array(x) | Value::Set(x) => x.iter().map(|v| cell(v, limits)).collect(),
                            other => vec![cell(other, limits)],
                        };
                        row.resize(width, Cell::Null);
                        row
                    })
                    .collect();
                (columns, rows)
            } else if pairs_reply(args) && items.len() % 2 == 0 {
                (
                    vec![meta("field", false), meta("value", false)],
                    items.chunks(2).map(|p| vec![cell(&p[0], limits), cell(&p[1], limits)]).collect(),
                )
            } else {
                (
                    vec![meta("#", true), meta("value", false)],
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, v)| vec![Cell::number((i + 1).to_string()), cell(v, limits)])
                        .collect(),
                )
            }
        }
        scalar => {
            (vec![meta("reply", matches!(scalar, Value::Int(_) | Value::Double(_)))], vec![vec![cell(scalar, limits)]])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn prints_like_redis_cli() {
        assert_eq!(format_cli(&Value::Int(5)), "(integer) 5");
        assert_eq!(format_cli(&Value::Nil), "(nil)");
        assert_eq!(format_cli(&Value::Okay), "OK");
        assert_eq!(format_cli(&Value::Array(vec![])), "(empty array)");
        assert_eq!(format_cli(&Value::BulkString(vec![b'a', b'"', b'\n', 0xc3, 0xab])), r#""a\"\n\xc3\xab""#);
        let nested = Value::Array(vec![bulk("a"), Value::Array(vec![bulk("b"), Value::Int(2)]), Value::Nil]);
        assert_eq!(format_cli(&nested), "1) \"a\"\n2) 1) \"b\"\n   2) (integer) 2\n3) (nil)");
        let ten = Value::Array((1..=10).map(Value::Int).collect());
        let printed = format_cli(&ten);
        assert!(printed.starts_with(" 1) (integer) 1\n"), "{printed}");
        assert!(printed.ends_with("10) (integer) 10"));
        let deep = Value::Array((0..10).map(|_| Value::Array(vec![Value::Int(1), Value::Int(2)])).collect());
        assert!(format_cli(&deep).contains("\n    2) (integer) 2\n"), "{}", format_cli(&deep));
    }

    #[test]
    fn grids() {
        let args = |s: &str| s.split(' ').map(|a| a.as_bytes().to_vec()).collect::<Vec<_>>();
        let (cols, rows) = to_grid(&args("GET a"), &bulk("x"), CellLimits::GRID);
        assert_eq!((cols[0].name.as_str(), rows.len()), ("reply", 1));
        let (cols, rows) = to_grid(&args("HGETALL h"), &Value::Array(vec![bulk("f"), bulk("v")]), CellLimits::GRID);
        assert_eq!((cols[0].name.as_str(), rows[0][1].clone()), ("field", Cell::text("v", CellLimits::GRID)));
        let (cols, rows) =
            to_grid(&args("LRANGE l 0 -1"), &Value::Array(vec![bulk("a"), Value::Nil]), CellLimits::GRID);
        assert_eq!((cols[0].name.as_str(), rows[1][1].clone()), ("#", Cell::Null));
        let streams = Value::Array(vec![Value::Array(vec![bulk("1-0"), Value::Array(vec![bulk("f"), bulk("v")])])]);
        let (cols, rows) = to_grid(&args("XRANGE s - +"), &streams, CellLimits::GRID);
        assert_eq!(cols.len(), 2);
        assert_eq!(rows[0][1], Cell::json(r#"["f","v"]"#, CellLimits::GRID));
        assert_eq!(cell(&Value::BulkString(vec![0xff]), CellLimits::GRID), Cell::binary(&[0xff], CellLimits::GRID));
    }
}
