//! Neo4j's typed JSON (`{ "$type": "Integer", "_value": "9007199254740993" }`)
//! to grid cells, document fields and plain JSON; editor text to typed parameters.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use dq_core::document::DocField;
use dq_core::value::{Cell, CellLimits};
use serde_json::{Map, Number, Value, json};

/// The `$type` of a value (`Null` when missing).
pub fn type_of(v: &Value) -> &str {
    v.get("$type").and_then(Value::as_str).unwrap_or("Null")
}

fn inner(v: &Value) -> &Value {
    v.get("_value").unwrap_or(&Value::Null)
}

fn inner_str(v: &Value) -> &str {
    inner(v).as_str().unwrap_or_default()
}

/// The Cypher type name shown in headers and the Documents view.
pub fn type_label(v: &Value) -> &'static str {
    match type_of(v) {
        "Boolean" => "Boolean",
        "Integer" => "Integer",
        "Float" => "Float",
        "String" => "String",
        "Base64" => "Bytes",
        "Date" => "Date",
        "Time" => "Time",
        "LocalTime" => "LocalTime",
        "DateTime" | "OffsetDateTime" => "DateTime",
        "LocalDateTime" => "LocalDateTime",
        "Duration" => "Duration",
        "Point" => "Point",
        "List" => "List",
        "Map" => "Map",
        "Node" => "Node",
        "Relationship" => "Relationship",
        "Path" => "Path",
        _ => "Null",
    }
}

pub fn is_numeric(label: &str) -> bool {
    matches!(label, "Integer" | "Float")
}

/// A grid cell. Integers keep every digit; graph values and collections are JSON.
pub fn cell(v: &Value, limits: CellLimits) -> Cell {
    match type_of(v) {
        "Null" => Cell::Null,
        "Boolean" => Cell::Bool { value: inner(v).as_bool().unwrap_or(false) },
        "Integer" => Cell::number(inner_str(v)),
        "Float" => {
            let s = inner_str(v);
            if s.parse::<f64>().is_ok_and(f64::is_finite) { Cell::number(s) } else { Cell::text(s, limits) }
        }
        "String" => Cell::text(inner_str(v), limits),
        "Base64" => BASE64
            .decode(inner_str(v))
            .map_or_else(|_| Cell::text(inner_str(v), limits), |bytes| Cell::binary(&bytes, limits)),
        "Date" | "Time" | "LocalTime" | "LocalDateTime" => Cell::Temporal { value: inner_str(v).to_owned(), zoned: false },
        "DateTime" | "OffsetDateTime" => Cell::Temporal { value: inner_str(v).to_owned(), zoned: true },
        "Duration" => Cell::text(inner_str(v), limits),
        "Point" => Cell::text(point_text(inner_str(v)), limits),
        _ => Cell::json(plain(v).to_string(), limits),
    }
}

/// `SRID=7203;POINT (1.0 2.0)` → `point({srid: 7203, x: 1.0, y: 2.0})`.
pub fn point_text(wkt: &str) -> String {
    let (srid, rest) = wkt.split_once(';').unwrap_or(("", wkt));
    let srid = srid.trim_start_matches("SRID=");
    let coords: Vec<&str> = rest
        .split_once('(')
        .and_then(|(_, r)| r.split(')').next())
        .map(|c| c.split_whitespace().collect())
        .unwrap_or_default();
    let names = ["x", "y", "z"];
    let parts: Vec<String> = coords.iter().zip(names).map(|(c, n)| format!("{n}: {c}")).collect();
    if srid.is_empty() || parts.is_empty() {
        return wkt.to_owned();
    }
    format!("point({{srid: {srid}, {}}})", parts.join(", "))
}

/// Plain JSON for exports and JSON cells: integers stay exact numbers, graph
/// values become `{ elementId, labels, properties }` / `{ elementId, type, start, end, properties }`.
pub fn plain(v: &Value) -> Value {
    match type_of(v) {
        "Null" => Value::Null,
        "Boolean" => inner(v).clone(),
        "Integer" => {
            inner_str(v).parse::<i64>().map_or_else(|_| Value::String(inner_str(v).to_owned()), |n| Value::Number(n.into()))
        }
        "Float" => inner_str(v)
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or_else(|| Value::String(inner_str(v).to_owned()), Value::Number),
        "Point" => Value::String(point_text(inner_str(v))),
        "List" | "Path" => Value::Array(inner(v).as_array().map(|a| a.iter().map(plain).collect()).unwrap_or_default()),
        "Map" => Value::Object(properties(inner(v))),
        "Node" => {
            let n = inner(v);
            json!({
                "elementId": n.get("_element_id").cloned().unwrap_or(Value::Null),
                "labels": n.get("_labels").cloned().unwrap_or_else(|| json!([])),
                "properties": properties(n.get("_properties").unwrap_or(&Value::Null)),
            })
        }
        "Relationship" => {
            let r = inner(v);
            json!({
                "elementId": r.get("_element_id").cloned().unwrap_or(Value::Null),
                "type": r.get("_type").cloned().unwrap_or(Value::Null),
                "start": r.get("_start_node_element_id").cloned().unwrap_or(Value::Null),
                "end": r.get("_end_node_element_id").cloned().unwrap_or(Value::Null),
                "properties": properties(r.get("_properties").unwrap_or(&Value::Null)),
            })
        }
        _ => inner(v).clone(),
    }
}

fn properties(map: &Value) -> Map<String, Value> {
    map.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), plain(v))).collect()).unwrap_or_default()
}

/// A document field (the Documents view): scalars as cells, collections and graph values as trees.
pub fn field(key: &str, v: &Value, limits: CellLimits) -> DocField {
    let label = type_label(v);
    match type_of(v) {
        "List" | "Path" => {
            let items = inner(v).as_array().cloned().unwrap_or_default();
            let children: Vec<DocField> = items.iter().enumerate().map(|(i, x)| field(&i.to_string(), x, limits)).collect();
            DocField::container(key, label, cell(v, limits), children.into_iter())
        }
        "Map" => {
            let children: Vec<DocField> = inner(v)
                .as_object()
                .map(|m| m.iter().map(|(k, x)| field(k, x, limits)).collect())
                .unwrap_or_default();
            DocField::container(key, label, cell(v, limits), children.into_iter())
        }
        "Node" | "Relationship" => {
            let children = entity_fields(inner(v), limits);
            DocField::container(key, label, cell(v, limits), children.into_iter())
        }
        _ => DocField::scalar(key, label, cell(v, limits)),
    }
}

/// The properties of a node or relationship as document fields, sorted by name.
pub fn entity_fields(entity: &Value, limits: CellLimits) -> Vec<DocField> {
    let mut props: Vec<(&String, &Value)> =
        entity.get("_properties").and_then(Value::as_object).map(|m| m.iter().collect()).unwrap_or_default();
    props.sort_by(|a, b| a.0.cmp(b.0));
    props.into_iter().map(|(k, v)| field(k, v, limits)).collect()
}

/// A typed parameter from editor text: integers, floats, `true`/`false`,
/// `null` and JSON arrays/objects keep their type; a JSON string (`"123"`)
/// forces text; anything else is a string.
pub fn param(text: Option<&str>) -> Value {
    let Some(text) = text else {
        return typed("Null", Value::Null);
    };
    let t = text.trim();
    if t.parse::<i64>().is_ok() {
        return typed("Integer", Value::String(t.to_owned()));
    }
    if t.parse::<f64>().is_ok_and(f64::is_finite) && t.chars().any(|c| matches!(c, '.' | 'e' | 'E')) {
        return typed("Float", Value::String(t.to_owned()));
    }
    match t {
        "true" | "false" => return typed("Boolean", Value::Bool(t == "true")),
        "null" => return typed("Null", Value::Null),
        _ => {}
    }
    if t.starts_with(['[', '{', '"'])
        && let Ok(json) = serde_json::from_str::<Value>(t)
    {
        return from_json(&json);
    }
    typed("String", Value::String(text.to_owned()))
}

fn typed(t: &str, v: Value) -> Value {
    json!({ "$type": t, "_value": v })
}

/// Plain JSON to typed JSON: whole numbers are Integers, others Floats.
pub fn from_json(v: &Value) -> Value {
    match v {
        Value::Null => typed("Null", Value::Null),
        Value::Bool(b) => typed("Boolean", Value::Bool(*b)),
        Value::Number(n) if n.is_i64() || n.is_u64() => typed("Integer", Value::String(n.to_string())),
        Value::Number(n) => typed("Float", Value::String(n.to_string())),
        Value::String(s) => typed("String", Value::String(s.clone())),
        Value::Array(a) => typed("List", Value::Array(a.iter().map(from_json).collect())),
        Value::Object(o) => typed("Map", Value::Object(o.iter().map(|(k, x)| (k.clone(), from_json(x))).collect())),
    }
}

/// An imported text value converted by the column's sampled Cypher type
/// (`Integer`, `Float`, `Boolean`, `String`); any other or no type infers like [`param`].
pub fn import_value(data_type: &str, text: Option<&str>) -> Result<Value, String> {
    let Some(t) = text else {
        return Ok(typed("Null", Value::Null));
    };
    match data_type {
        "Integer" => t
            .trim()
            .parse::<i64>()
            .map(|_| typed("Integer", Value::String(t.trim().to_owned())))
            .map_err(|_| format!("`{t}` is not a whole number")),
        "Float" => t
            .trim()
            .parse::<f64>()
            .map(|_| typed("Float", Value::String(t.trim().to_owned())))
            .map_err(|_| format!("`{t}` is not a number")),
        "Boolean" => match t.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Ok(typed("Boolean", Value::Bool(true))),
            "false" | "f" | "no" | "n" | "0" => Ok(typed("Boolean", Value::Bool(false))),
            _ => Err(format!("`{t}` is not a boolean")),
        },
        "String" => Ok(typed("String", Value::String(t.to_owned()))),
        _ => Ok(param(Some(t))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ty: &str, v: Value) -> Value {
        typed(ty, v)
    }

    #[test]
    fn cells_keep_types() {
        let g = CellLimits::GRID;
        assert_eq!(cell(&t("Integer", json!("9007199254740993")), g), Cell::number("9007199254740993"));
        assert_eq!(cell(&t("Float", json!("2.5")), g), Cell::number("2.5"));
        assert_eq!(cell(&t("Float", json!("NaN")), g), Cell::text("NaN", g));
        assert_eq!(cell(&t("Null", Value::Null), g), Cell::Null);
        assert_eq!(cell(&t("Base64", json!("AAEC")), g), Cell::binary(&[0, 1, 2], g));
        assert_eq!(
            cell(&t("OffsetDateTime", json!("2026-10-14T10:06:31.29Z")), g),
            Cell::Temporal { value: "2026-10-14T10:06:31.29Z".into(), zoned: true }
        );
        assert_eq!(cell(&t("Date", json!("2026-10-14")), g), Cell::Temporal { value: "2026-10-14".into(), zoned: false });
        assert_eq!(cell(&t("Point", json!("SRID=7203;POINT (1.0 2.0)")), g), Cell::text("point({srid: 7203, x: 1.0, y: 2.0})", g));
    }

    #[test]
    fn graph_values_as_json() {
        let node = t(
            "Node",
            json!({ "_element_id": "4:x:1", "_labels": ["P"], "_properties": { "n": { "$type": "Integer", "_value": "9007199254740993" } } }),
        );
        let rel = t(
            "Relationship",
            json!({ "_element_id": "5:x:0", "_start_node_element_id": "4:x:0", "_end_node_element_id": "4:x:1", "_type": "KNOWS", "_properties": {} }),
        );
        assert_eq!(plain(&node), json!({ "elementId": "4:x:1", "labels": ["P"], "properties": { "n": 9_007_199_254_740_993_i64 } }));
        assert_eq!(plain(&rel)["type"], "KNOWS");
        let path = t("Path", json!([node, rel]));
        assert_eq!(plain(&path).as_array().unwrap().len(), 2);
        let f = field("n", &node, CellLimits::GRID);
        assert_eq!((f.type_label.as_str(), f.children[0].type_label.as_str()), ("Node", "Integer"));
        assert_eq!(f.children[0].value, Cell::number("9007199254740993"));
    }

    #[test]
    fn parameters_are_typed() {
        assert_eq!(param(Some("42")), t("Integer", json!("42")));
        assert_eq!(param(Some("1.5")), t("Float", json!("1.5")));
        assert_eq!(param(Some("true")), t("Boolean", json!(true)));
        assert_eq!(param(None), t("Null", Value::Null));
        assert_eq!(param(Some("\"123\"")), t("String", json!("123")));
        assert_eq!(param(Some("Ann")), t("String", json!("Ann")));
        assert_eq!(param(Some("[1, \"a\"]")), t("List", json!([{ "$type": "Integer", "_value": "1" }, { "$type": "String", "_value": "a" }])));
        assert_eq!(param(Some("inf")), t("String", json!("inf")));
        assert_eq!(import_value("Integer", Some(" 7 ")).unwrap(), t("Integer", json!("7")));
        assert!(import_value("Integer", Some("x")).is_err());
        assert_eq!(import_value("Boolean", Some("yes")).unwrap(), t("Boolean", json!(true)));
    }
}
