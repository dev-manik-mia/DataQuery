//! JSON values as cells and typed document fields, and responses as grids.
//!
//! Numbers keep the text `serde_json` reads: integers up to `u64` are exact;
//! larger integers and decimals arrive as `f64` and print in their shortest
//! round-trip form.

use std::collections::HashMap;

use dq_core::document::{DocField, GridColumns};
use dq_core::result::{ColumnMeta, Row};
use dq_core::value::{Cell, CellLimits};
use serde_json::{Map, Value};

/// Field types by dotted path, from the index mapping.
pub type FieldTypes = HashMap<String, String>;

/// Largest aggregations notice; bigger results are cut.
const MAX_NOTICE: usize = 64 * 1024;

/// Elasticsearch numeric field types (and the label of a bare JSON number).
pub fn is_numeric_type(label: &str) -> bool {
    matches!(
        label,
        "Number" | "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float"
            | "unsigned_long"
    )
}

pub fn cell(value: &Value, limits: CellLimits) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool { value: *b },
        Value::Number(n) => Cell::number(n.to_string()),
        Value::String(s) => Cell::text(s.clone(), limits),
        other => Cell::json(other.to_string(), limits),
    }
}

fn json_label(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

/// One field; `path` is its dotted path, used to look up the mapped type.
pub fn field(key: &str, path: &str, value: &Value, types: &FieldTypes, limits: CellLimits) -> DocField {
    let mapped = types.get(path).map(String::as_str);
    match value {
        Value::Object(map) => DocField::container(
            key,
            mapped.unwrap_or("Object"),
            cell(value, limits),
            map.iter()
                .map(|(k, v)| field(k, &format!("{path}.{k}"), v, types, limits))
                .collect::<Vec<_>>()
                .into_iter(),
        ),
        // Arrays hold values of the mapped type; the container is labelled `Array`.
        Value::Array(items) => DocField::container(
            key,
            "Array",
            cell(value, limits),
            items.iter().enumerate().map(|(i, v)| field(&i.to_string(), path, v, types, limits)).collect::<Vec<_>>().into_iter(),
        ),
        _ => DocField::scalar(key, mapped.unwrap_or_else(|| json_label(value)), cell(value, limits)),
    }
}

pub fn object_fields(map: &Map<String, Value>, types: &FieldTypes, limits: CellLimits) -> Vec<DocField> {
    map.iter().map(|(k, v)| field(k, k, v, types, limits)).collect()
}

/// `_index`, `_id`, `_score`, then the `_source` fields.
pub fn hit_fields(hit: &Value, types: &FieldTypes, limits: CellLimits) -> Vec<DocField> {
    let mut fields = vec![
        DocField::scalar("_index", "keyword", cell(&hit["_index"], limits)),
        DocField::scalar("_id", "keyword", cell(&hit["_id"], limits)),
    ];
    if !hit["_score"].is_null() {
        fields.push(DocField::scalar("_score", "float", cell(&hit["_score"], limits)));
    }
    if let Some(source) = hit["_source"].as_object() {
        fields.extend(object_fields(source, types, limits));
    }
    fields
}

/// Grid columns with numeric alignment for Elasticsearch types.
pub fn meta(columns: &GridColumns) -> Vec<ColumnMeta> {
    columns
        .meta()
        .into_iter()
        .map(|mut c| {
            c.numeric = c.numeric || is_numeric_type(&c.type_name);
            c
        })
        .collect()
}

/// A response as a grid, with notices (totals, timing, aggregations, bulk errors).
#[derive(Debug, Default)]
pub struct Grid {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    pub notices: Vec<String>,
}

fn documents_grid(docs: &[Vec<DocField>]) -> (Vec<ColumnMeta>, Vec<Row>) {
    let mut columns = GridColumns::default();
    for d in docs {
        columns.observe(d);
    }
    let rows = docs.iter().map(|d| columns.row(d)).collect();
    (meta(&columns), rows)
}

pub fn grid(body: &crate::client::Body, limits: CellLimits) -> Grid {
    let none = FieldTypes::new();
    let value = match body {
        crate::client::Body::Text(text) => {
            return Grid {
                columns: vec![ColumnMeta { name: "response".into(), type_name: "text".into(), numeric: false }],
                rows: text.lines().filter(|l| !l.trim().is_empty()).map(|l| vec![Cell::text(l, limits)]).collect(),
                notices: Vec::new(),
            };
        }
        crate::client::Body::Json(v) => v,
    };
    let mut out = Grid::default();
    if let Some(hits) = value["hits"]["hits"].as_array() {
        let docs: Vec<Vec<DocField>> = hits.iter().map(|h| hit_fields(h, &none, limits)).collect();
        (out.columns, out.rows) = documents_grid(&docs);
        let total = &value["hits"]["total"];
        let total = total["value"].as_u64().map_or_else(
            || total.as_u64().map(|t| t.to_string()),
            |t| Some(format!("{t}{}", if total["relation"] == "gte" { "+" } else { "" })),
        );
        if let Some(total) = total {
            out.notices.push(format!("{total} hits in {} ms.", value["took"].as_u64().unwrap_or(0)));
        }
        if let Some(aggs) = value.get("aggregations") {
            let mut text = serde_json::to_string_pretty(aggs).unwrap_or_default();
            if text.len() > MAX_NOTICE {
                let mut cut = MAX_NOTICE;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n…");
            }
            out.notices.push(format!("Aggregations:\n{text}"));
        }
        return out;
    }
    // `_sql`: named, typed columns and rows of values.
    if let (Some(columns), Some(rows)) = (value["columns"].as_array(), value["rows"].as_array()) {
        out.columns = columns
            .iter()
            .map(|c| {
                let t = c["type"].as_str().unwrap_or("").to_owned();
                ColumnMeta { name: c["name"].as_str().unwrap_or("").to_owned(), numeric: is_numeric_type(&t), type_name: t }
            })
            .collect();
        out.rows = rows
            .iter()
            .map(|r| r.as_array().map(|r| r.iter().map(|v| cell(v, limits)).collect()).unwrap_or_default())
            .collect();
        if value["cursor"].is_string() {
            out.notices.push("More rows exist; ask for them with the returned cursor.".into());
        }
        return out;
    }
    if let Some(items) = value.as_array() {
        if items.iter().all(Value::is_object) {
            let docs: Vec<Vec<DocField>> =
                items.iter().filter_map(Value::as_object).map(|o| object_fields(o, &none, limits)).collect();
            (out.columns, out.rows) = documents_grid(&docs);
        } else {
            out.columns = vec![ColumnMeta { name: "value".into(), type_name: "json".into(), numeric: false }];
            out.rows = items.iter().map(|v| vec![cell(v, limits)]).collect();
        }
        return out;
    }
    if let Some(map) = value.as_object() {
        (out.columns, out.rows) = documents_grid(&[object_fields(map, &none, limits)]);
        if value["errors"] == Value::Bool(true) {
            let first = value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i.as_object().and_then(|o| o.values().next()))
                .find_map(|r| r["error"]["reason"].as_str());
            out.notices.push(format!("Some items failed: {}", first.unwrap_or("see the items column")));
        }
    }
    out
}

/// Rows the request changed: `deleted`/`updated` by query, successful bulk items, 1 for a document write.
pub fn affected(value: &Value) -> Option<u64> {
    if let Some(items) = value["items"].as_array() {
        let ok = items
            .iter()
            .filter(|i| i.as_object().and_then(|o| o.values().next()).is_some_and(|r| r["error"].is_null()))
            .count();
        return Some(ok as u64);
    }
    for key in ["deleted", "updated"] {
        if let Some(n) = value[key].as_u64().filter(|n| *n > 0 || value["total"].is_u64()) {
            return Some(n);
        }
    }
    match value["result"].as_str() {
        Some("created" | "updated" | "deleted") => Some(1),
        Some("noop" | "not_found") => Some(0),
        _ => None,
    }
}

/// Field types by dotted path from a `_mapping` response's `properties` tree.
pub fn mapping_types(properties: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    let Some(map) = properties.as_object() else { return };
    for (name, def) in map {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let kind = def["type"].as_str().unwrap_or("object");
        out.push((path.clone(), kind.to_owned()));
        mapping_types(&def["properties"], &path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_body;

    #[test]
    fn hits_become_rows() {
        let body = parse_body(
            r#"{"took":3,"hits":{"total":{"value":2,"relation":"eq"},"hits":[
              {"_index":"o","_id":"1","_score":1.0,"_source":{"n":9007199254740993,"c":{"a":1},"s":"x"}},
              {"_index":"o","_id":"2","_score":0.5,"_source":{"n":2,"extra":true}}]},
              "aggregations":{"by":{"buckets":[]}}}"#,
        );
        let g = grid(&body, CellLimits::GRID);
        // Source key order depends on serde_json's `preserve_order`, so look columns up by name.
        let at = |name: &str| g.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!((at("_index"), at("_id"), at("_score")), (0, 1, 2));
        assert_eq!(g.columns.len(), 7);
        assert_eq!(g.rows[0][at("n")], Cell::number("9007199254740993"));
        assert_eq!(g.rows[0][at("c")], Cell::json("{\"a\":1}", CellLimits::GRID));
        assert_eq!(g.rows[1][at("extra")], Cell::Bool { value: true });
        assert!(g.columns[at("n")].numeric);
        assert!(g.notices[0].starts_with("2 hits"));
        assert!(g.notices[1].starts_with("Aggregations:"));
    }

    #[test]
    fn other_shapes() {
        let cat = grid(&parse_body(r#"[{"index":"a","docs.count":"3"},{"index":"b","docs.count":"4"}]"#), CellLimits::GRID);
        assert_eq!((cat.columns.len(), cat.rows.len()), (2, 2));
        let sql = grid(
            &parse_body(r#"{"columns":[{"name":"n","type":"long"},{"name":"s","type":"keyword"}],"rows":[[1,"a"],[2,null]]}"#),
            CellLimits::GRID,
        );
        assert!(sql.columns[0].numeric);
        assert_eq!(sql.rows[1][1], Cell::Null);
        let text = grid(&parse_body("green open a\ngreen open b\n"), CellLimits::GRID);
        assert_eq!((text.columns[0].name.as_str(), text.rows.len()), ("response", 2));
        let obj = grid(&parse_body(r#"{"acknowledged":true}"#), CellLimits::GRID);
        assert_eq!(obj.rows, vec![vec![Cell::Bool { value: true }]]);
        let bulk = parse_body(r#"{"errors":true,"items":[{"index":{"result":"created"}},{"index":{"error":{"reason":"bad"}}}]}"#);
        assert!(grid(&bulk, CellLimits::GRID).notices[0].contains("bad"));
        assert_eq!(affected(bulk.json()), Some(1));
        assert_eq!(affected(&serde_json::json!({"deleted": 4, "total": 4})), Some(4));
        assert_eq!(affected(&serde_json::json!({"result": "created"})), Some(1));
    }

    #[test]
    fn mapped_types_label_fields() {
        let mut types = Vec::new();
        mapping_types(
            &serde_json::json!({"n":{"type":"long"},"c":{"properties":{"a":{"type":"keyword"}}}}),
            "",
            &mut types,
        );
        let types: FieldTypes = types.into_iter().collect();
        let f = object_fields(serde_json::json!({"n": 1, "c": {"a": "x"}}).as_object().unwrap(), &types, CellLimits::GRID);
        let by = |k: &str| f.iter().find(|x| x.key == k).unwrap();
        assert_eq!((by("n").type_label.as_str(), by("c").type_label.as_str()), ("long", "object"));
        assert_eq!(by("c").children[0].type_label, "keyword");
    }
}
