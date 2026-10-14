//! `DynamoDB` attribute values (`{"S": "a"}`, `{"N": "1.5"}`, `{"M": {…}}`).
//!
//! Values stay in the wire form (`serde_json::Value`): numbers are strings
//! there, so every digit survives display, editing and export.

use std::fmt::Write;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use dq_core::document::DocField;
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult};
use serde_json::{Map, Value};

/// An item: attribute name → attribute value.
pub type Item = Map<String, Value>;

const TYPES: &[&str] = &["S", "N", "B", "BOOL", "NULL", "M", "L", "SS", "NS", "BS"];

/// The type tag and payload of an attribute value, when it has exactly one tag.
pub fn tagged(av: &Value) -> Option<(&str, &Value)> {
    let map = av.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let (tag, payload) = map.iter().next()?;
    TYPES.contains(&tag.as_str()).then_some((tag.as_str(), payload))
}

/// A field of the Documents view tree.
pub fn field(key: &str, av: &Value, limits: CellLimits) -> DocField {
    let Some((tag, payload)) = tagged(av) else {
        return DocField::scalar(
            key,
            "?",
            Cell::Unsupported { type_name: "unknown".into(), value: Some(av.to_string()) },
        );
    };
    let summary = || Cell::json(plain(av), limits);
    let elements = |label: &'static str| {
        let items = payload.as_array().map(Vec::as_slice).unwrap_or_default();
        let children: Vec<DocField> =
            items.iter().enumerate().map(|(i, v)| scalar_set_member(&i.to_string(), label, v, limits)).collect();
        children
    };
    match tag {
        "S" => DocField::scalar(key, "S", Cell::text(payload.as_str().unwrap_or_default(), limits)),
        "N" => DocField::scalar(key, "N", Cell::number(payload.as_str().unwrap_or_default())),
        "B" => DocField::scalar(key, "B", binary_cell(payload, limits)),
        "BOOL" => DocField::scalar(key, "BOOL", Cell::Bool { value: payload.as_bool().unwrap_or(false) }),
        "NULL" => DocField::scalar(key, "NULL", Cell::Null),
        "M" => {
            let map = payload.as_object();
            let children: Vec<DocField> =
                map.into_iter().flatten().map(|(k, v)| field(k, v, limits)).collect::<Vec<_>>();
            DocField::container(key, "M", summary(), children.into_iter())
        }
        "L" => {
            let items = payload.as_array().map(Vec::as_slice).unwrap_or_default();
            let children: Vec<DocField> =
                items.iter().enumerate().map(|(i, v)| field(&i.to_string(), v, limits)).collect();
            DocField::container(key, "L", summary(), children.into_iter())
        }
        "SS" => DocField::container(key, "SS", summary(), elements("S").into_iter()),
        "NS" => DocField::container(key, "NS", summary(), elements("N").into_iter()),
        _ => DocField::container(key, "BS", summary(), elements("B").into_iter()),
    }
}

fn scalar_set_member(key: &str, label: &str, v: &Value, limits: CellLimits) -> DocField {
    let wrapped = Value::Object(Map::from_iter([(label.to_owned(), v.clone())]));
    field(key, &wrapped, limits)
}

fn binary_cell(payload: &Value, limits: CellLimits) -> Cell {
    let text = payload.as_str().unwrap_or_default();
    BASE64.decode(text).map_or_else(|_| Cell::text(text, limits), |bytes| Cell::binary(&bytes, limits))
}

/// Every attribute of an item as fields, key attributes first.
pub fn fields(item: &Item, key_names: &[String], limits: CellLimits) -> Vec<DocField> {
    ordered(item, key_names).map(|(k, v)| field(k, v, limits)).collect()
}

/// Key attributes in key order, then the others by name.
pub fn ordered<'a>(item: &'a Item, key_names: &'a [String]) -> impl Iterator<Item = (&'a String, &'a Value)> {
    let keys = key_names.iter().filter_map(|k| item.get_key_value(k));
    let rest = item.iter().filter(|(k, _)| !key_names.contains(k));
    keys.chain(rest)
}

/// Compact plain JSON of a value: numbers unquoted with every digit, binary
/// as base64 text, sets as arrays.
pub fn plain(av: &Value) -> String {
    let mut out = String::new();
    write_plain(av, &mut out);
    out
}

fn write_plain(av: &Value, out: &mut String) {
    let Some((tag, payload)) = tagged(av) else {
        out.push_str(&av.to_string());
        return;
    };
    match tag {
        "N" => out.push_str(payload.as_str().unwrap_or("null")),
        "NS" => {
            let items = payload.as_array().map(Vec::as_slice).unwrap_or_default();
            out.push('[');
            for (i, n) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(n.as_str().unwrap_or("null"));
            }
            out.push(']');
        }
        "NULL" => out.push_str("null"),
        "M" => {
            out.push('{');
            for (i, (k, v)) in payload.as_object().into_iter().flatten().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_plain(v, out);
            }
            out.push('}');
        }
        "L" => {
            out.push('[');
            for (i, v) in payload.as_array().into_iter().flatten().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_plain(v, out);
            }
            out.push(']');
        }
        // S, B (base64 text), BOOL, SS, BS are already plain JSON.
        _ => out.push_str(&payload.to_string()),
    }
}

/// Pretty `DynamoDB` JSON of an item, key attributes first.
pub fn item_text(item: &Item, key_names: &[String]) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in ordered(item, key_names).enumerate() {
        out.push_str(if i == 0 { "\n  " } else { ",\n  " });
        out.push_str(&Value::String(k.clone()).to_string());
        out.push_str(": ");
        let pretty = serde_json::to_string_pretty(v).unwrap_or_default();
        out.push_str(&pretty.replace('\n', "\n  "));
    }
    out.push_str(if item.is_empty() { "}" } else { "\n}" });
    out
}

/// Compact `DynamoDB` JSON of the key attributes, in key order. `None` when one is missing.
pub fn key_json(item: &Item, key_names: &[String]) -> Option<String> {
    let mut out = String::from("{");
    for (i, name) in key_names.iter().enumerate() {
        let v = item.get(name)?;
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:{v}", Value::String(name.clone()));
    }
    out.push('}');
    Some(out)
}

/// `pk: "a" · sk: 3`.
pub fn key_label(item: &Item, key_names: &[String]) -> String {
    key_names
        .iter()
        .filter_map(|name| {
            let (tag, payload) = tagged(item.get(name)?)?;
            let value = match tag {
                "S" => Value::String(payload.as_str().unwrap_or_default().to_owned()).to_string(),
                _ => payload.as_str().map_or_else(|| payload.to_string(), str::to_owned),
            };
            Some(format!("{name}: {value}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Approximate stored size: attribute names plus values, as `DynamoDB` counts them.
pub fn item_size(item: &Item) -> u64 {
    item.iter().map(|(k, v)| k.len() as u64 + value_size(v)).sum()
}

fn value_size(av: &Value) -> u64 {
    let Some((tag, payload)) = tagged(av) else { return 0 };
    let text_len = |v: &Value| v.as_str().map_or(0, str::len) as u64;
    match tag {
        "S" => text_len(payload),
        "N" => text_len(payload).div_ceil(2) + 1,
        "B" => text_len(payload) * 3 / 4,
        "BOOL" | "NULL" => 1,
        "M" => {
            3 + payload.as_object().into_iter().flatten().map(|(k, v)| 1 + k.len() as u64 + value_size(v)).sum::<u64>()
        }
        "L" => 3 + payload.as_array().into_iter().flatten().map(|v| 1 + value_size(v)).sum::<u64>(),
        _ => payload.as_array().into_iter().flatten().map(text_len).sum(),
    }
}

/// Parses an item typed as `DynamoDB` JSON and checks every value.
pub fn parse_item(text: &str) -> DbResult<Item> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| DbError::invalid(format!("This is not valid DynamoDB JSON: {e}.")))?;
    let Value::Object(item) = value else {
        return Err(DbError::invalid("An item is a JSON object of attribute names to typed values."));
    };
    for (name, av) in &item {
        validate(av).map_err(|m| DbError::invalid(format!("Attribute `{name}`: {m}")))?;
    }
    Ok(item)
}

/// Checks one attribute value, recursively.
pub fn validate(av: &Value) -> Result<(), String> {
    let Some((tag, payload)) = tagged(av) else {
        return Err(format!("expected one type such as {{\"S\": \"text\"}} or {{\"N\": \"1\"}}, got {av}"));
    };
    let set = |check: fn(&str) -> bool, what: &str| -> Result<(), String> {
        let items = payload.as_array().ok_or_else(|| format!("{tag} must be an array"))?;
        if items.is_empty() {
            return Err(format!("{tag} cannot be empty"));
        }
        let mut seen = std::collections::HashSet::new();
        for item in items {
            let s = item.as_str().ok_or_else(|| format!("{tag} holds strings"))?;
            if !check(s) {
                return Err(format!("`{s}` is not {what}"));
            }
            if !seen.insert(s) {
                return Err(format!("{tag} has `{s}` twice"));
            }
        }
        Ok(())
    };
    match tag {
        "S" if payload.is_string() => Ok(()),
        "N" => match payload.as_str() {
            Some(n) if is_number(n) => Ok(()),
            _ => Err(format!("{payload} is not a number (write numbers as strings: {{\"N\": \"1.5\"}})")),
        },
        "B" => match payload.as_str() {
            Some(b) if BASE64.decode(b).is_ok() => Ok(()),
            _ => Err("B must be base64 text".into()),
        },
        "BOOL" if payload.is_boolean() => Ok(()),
        "NULL" if payload == &Value::Bool(true) => Ok(()),
        "M" => payload
            .as_object()
            .ok_or_else(|| "M must be an object".to_owned())?
            .iter()
            .try_for_each(|(k, v)| validate(v).map_err(|m| format!("{k}: {m}"))),
        "L" => payload.as_array().ok_or_else(|| "L must be an array".to_owned())?.iter().try_for_each(validate),
        "SS" => set(|_| true, "a string"),
        "NS" => set(is_number, "a number"),
        "BS" => set(|b| BASE64.decode(b).is_ok(), "base64"),
        _ => Err(format!("{tag} has the wrong payload type")),
    }
}

/// A `DynamoDB` number literal: optional sign, digits, optional fraction and exponent.
pub fn is_number(text: &str) -> bool {
    let t = text.trim_start_matches(['-', '+']);
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e.trim_start_matches(['-', '+']))),
        None => (t, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    !(int.is_empty() && frac.is_empty())
        && digits(int)
        && digits(frac)
        && exponent.is_none_or(|e| !e.is_empty() && digits(e))
        && text.len() <= 64
}

/// A `?` parameter: typed `DynamoDB` JSON as given, a number as `N`, else `S`.
pub fn param(value: Option<&str>) -> Value {
    let Some(text) = value else { return tagged_value("NULL", Value::Bool(true)) };
    if text.trim_start().starts_with('{')
        && let Ok(av) = serde_json::from_str::<Value>(text)
        && validate(&av).is_ok()
    {
        return av;
    }
    if is_number(text.trim()) {
        return tagged_value("N", Value::String(text.trim().to_owned()));
    }
    tagged_value("S", Value::String(text.to_owned()))
}

pub fn tagged_value(tag: &str, payload: Value) -> Value {
    Value::Object(Map::from_iter([(tag.to_owned(), payload)]))
}

/// Converts imported text by the attribute's type label; an empty label infers
/// the type from the value. `None` leaves the attribute out.
pub fn from_text(label: &str, text: &str) -> Result<Value, String> {
    let s = |v: &str| Value::String(v.to_owned());
    let av = match label {
        "S" => tagged_value("S", s(text)),
        "N" => tagged_value("N", s(text.trim())),
        "B" => tagged_value("B", s(text.trim())),
        "BOOL" => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => tagged_value("BOOL", Value::Bool(true)),
            "false" | "0" | "no" => tagged_value("BOOL", Value::Bool(false)),
            _ => return Err(format!("`{text}` is not a boolean")),
        },
        "NULL" => tagged_value("NULL", Value::Bool(true)),
        "M" | "L" | "SS" | "NS" | "BS" => {
            let json: Value = serde_json::from_str(text).map_err(|_| format!("`{text}` is not JSON"))?;
            // Typed DynamoDB JSON is taken as is; plain JSON is converted.
            if validate(&json).is_ok() && tagged(&json).is_some_and(|(t, _)| t == label) {
                json
            } else if matches!(label, "SS" | "NS" | "BS") {
                let items = json.as_array().ok_or_else(|| format!("{label} needs a JSON array"))?;
                let members: Vec<Value> =
                    items.iter().map(|v| s(&v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect();
                tagged_value(label, Value::Array(members))
            } else {
                from_plain(&json)
            }
        }
        _ => infer(text),
    };
    validate(&av)?;
    Ok(av)
}

fn infer(text: &str) -> Value {
    let trimmed = text.trim();
    if is_number(trimmed) && !trimmed.is_empty() {
        return tagged_value("N", Value::String(trimmed.to_owned()));
    }
    match trimmed {
        "true" => return tagged_value("BOOL", Value::Bool(true)),
        "false" => return tagged_value("BOOL", Value::Bool(false)),
        _ => {}
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(json) = serde_json::from_str::<Value>(trimmed)
    {
        return from_plain(&json);
    }
    tagged_value("S", Value::String(text.to_owned()))
}

/// Plain JSON to attribute values: objects → `M`, arrays → `L`.
pub fn from_plain(json: &Value) -> Value {
    match json {
        Value::Null => tagged_value("NULL", Value::Bool(true)),
        Value::Bool(b) => tagged_value("BOOL", Value::Bool(*b)),
        Value::Number(n) => tagged_value("N", Value::String(n.to_string())),
        Value::String(s) => tagged_value("S", Value::String(s.clone())),
        Value::Array(items) => tagged_value("L", Value::Array(items.iter().map(from_plain).collect())),
        Value::Object(map) => {
            tagged_value("M", Value::Object(map.iter().map(|(k, v)| (k.clone(), from_plain(v))).collect()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(json: &str) -> Item {
        parse_item(json).unwrap()
    }

    #[test]
    fn numbers_stay_exact() {
        let i = item(r#"{"pk":{"S":"a"},"n":{"N":"9007199254740993.5"}}"#);
        let f = fields(&i, &["pk".into()], CellLimits::GRID);
        assert_eq!(f[1].value, Cell::number("9007199254740993.5"));
        assert_eq!(plain(&i["n"]), "9007199254740993.5");
        assert!(item_text(&i, &["pk".into()]).contains("\"9007199254740993.5\""));
    }

    #[test]
    fn sets_binary_and_maps() {
        let i = item(
            r#"{"pk":{"S":"a"},"tags":{"SS":["x","y"]},"bin":{"B":"AAEC"},"m":{"M":{"z":{"N":"1"},"l":{"L":[{"BOOL":true},{"NULL":true}]}}}}"#,
        );
        let f = fields(&i, &["pk".into()], CellLimits::GRID);
        let bin = f.iter().find(|f| f.key == "bin").unwrap();
        assert_eq!(bin.value, Cell::Binary { length: 3, head_hex: "000102".into() });
        let tags = f.iter().find(|f| f.key == "tags").unwrap();
        assert_eq!(
            (tags.type_label.as_str(), tags.children.len(), tags.children[0].type_label.as_str()),
            ("SS", 2, "S")
        );
        let m = f.iter().find(|f| f.key == "m").unwrap();
        // Key order depends on serde_json's `preserve_order`, which other crates may enable.
        let parsed: Value = serde_json::from_str(&plain(&i["m"])).unwrap();
        assert_eq!(parsed, serde_json::json!({ "l": [true, null], "z": 1 }));
        assert_eq!(m.children.iter().find(|c| c.key == "l").unwrap().children[0].value, Cell::Bool { value: true });
    }

    #[test]
    fn keys_and_labels() {
        let i = item(r#"{"x":{"S":"1"},"sk":{"N":"3"},"pk":{"S":"a\"b"}}"#);
        let keys = ["pk".to_owned(), "sk".to_owned()];
        assert_eq!(key_json(&i, &keys).unwrap(), r#"{"pk":{"S":"a\"b"},"sk":{"N":"3"}}"#);
        assert_eq!(key_label(&i, &keys), r#"pk: "a\"b" · sk: 3"#);
        assert!(item_text(&i, &keys).starts_with("{\n  \"pk\""));
        assert_eq!(key_json(&i, &["missing".into()]), None);
    }

    #[test]
    fn validation_errors() {
        assert!(parse_item(r#"{"n":{"N":"abc"}}"#).unwrap_err().message.contains("not a number"));
        assert!(parse_item(r#"{"s":{"SS":[]}}"#).unwrap_err().message.contains("empty"));
        assert!(parse_item(r#"{"s":{"SS":["a","a"]}}"#).unwrap_err().message.contains("twice"));
        assert!(parse_item(r#"{"b":{"B":"!!"}}"#).is_err());
        assert!(parse_item(r#"{"x":"plain"}"#).is_err());
        assert!(parse_item(r#"{"x":{"S":"a","N":"1"}}"#).is_err());
        assert!(parse_item("[1]").is_err());
    }

    #[test]
    fn params_and_inference() {
        assert_eq!(param(None), serde_json::json!({"NULL": true}));
        assert_eq!(param(Some("42")), serde_json::json!({"N": "42"}));
        assert_eq!(param(Some("abc")), serde_json::json!({"S": "abc"}));
        assert_eq!(param(Some(r#"{"BOOL": true}"#)), serde_json::json!({"BOOL": true}));
        assert_eq!(param(Some(r#"{"x": 1}"#)), serde_json::json!({"S": r#"{"x": 1}"#}));
        assert_eq!(from_text("", "1.50").unwrap(), serde_json::json!({"N": "1.50"}));
        assert_eq!(from_text("", "true").unwrap(), serde_json::json!({"BOOL": true}));
        assert_eq!(
            from_text("", r#"{"a":[1,"x"]}"#).unwrap(),
            serde_json::json!({"M": {"a": {"L": [{"N": "1"}, {"S": "x"}]}}})
        );
        assert_eq!(from_text("S", "007").unwrap(), serde_json::json!({"S": "007"}));
        assert_eq!(from_text("SS", r#"["a","b"]"#).unwrap(), serde_json::json!({"SS": ["a", "b"]}));
        assert!(from_text("N", "x").is_err());
        assert!(is_number("-1.5e10") && !is_number("1e") && !is_number(".") && !is_number("NaN"));
    }
}
