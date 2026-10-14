//! Tables and their keys and indexes, from `ListTables` and `DescribeTable`.

use std::collections::HashMap;
use std::fmt::Write;

use dq_core::document::DocIndex;
use dq_core::schema::{ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, ModelColumn};
use dq_core::{DbError, DbResult};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::values::{self, Item};

/// `DescribeTable` calls in flight at once while listing tables.
const DESCRIBE_CONCURRENCY: usize = 8;

/// Table names in order, at most `max`. The flag says more exist.
pub async fn table_names(client: &Client, max: usize) -> DbResult<(Vec<String>, bool)> {
    let mut names = Vec::new();
    let mut start: Option<String> = None;
    loop {
        let mut body = json!({ "Limit": 100 });
        if let Some(s) = &start {
            body["ExclusiveStartTableName"] = Value::String(s.clone());
        }
        let page = client.call("ListTables", &body).await?;
        names.extend(page["TableNames"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
        start = page["LastEvaluatedTableName"].as_str().map(str::to_owned);
        if names.len() > max {
            names.truncate(max);
            return Ok((names, true));
        }
        if start.is_none() {
            return Ok((names, false));
        }
    }
}

/// The `Table` object of `DescribeTable`.
pub async fn describe(client: &Client, table: &str) -> DbResult<Value> {
    let out = client.call("DescribeTable", &json!({ "TableName": table })).await?;
    Ok(out.get("Table").cloned().unwrap_or(Value::Null))
}

/// Describes several tables a few at a time; failures become `None`.
pub async fn describe_many(client: &Client, names: &[String]) -> Vec<(String, Option<Value>)> {
    stream::iter(names.iter().cloned())
        .map(|name| async move {
            let table = describe(client, &name).await.ok();
            (name, table)
        })
        .buffered(DESCRIBE_CONCURRENCY)
        .collect()
        .await
}

/// Partition key, then sort key.
pub fn key_names(table: &Value) -> Vec<String> {
    key_schema(&table["KeySchema"]).into_iter().map(|(name, _)| name).collect()
}

/// `(attribute, HASH | RANGE)` in key order.
fn key_schema(schema: &Value) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = schema
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| Some((k["AttributeName"].as_str()?.to_owned(), k["KeyType"].as_str()?.to_owned())))
        .collect();
    keys.sort_by_key(|(_, kind)| kind != "HASH");
    keys
}

fn key_text(schema: &Value) -> String {
    key_schema(schema).iter().map(|(name, kind)| format!("{name} ({kind})")).collect::<Vec<_>>().join(" · ")
}

/// Attribute name → `S` / `N` / `B` for key and index attributes.
fn attribute_types(table: &Value) -> HashMap<String, String> {
    table["AttributeDefinitions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["AttributeName"].as_str()?.to_owned(), a["AttributeType"].as_str()?.to_owned())))
        .collect()
}

pub fn item_count(table: &Value) -> Option<i64> {
    table["ItemCount"].as_i64()
}

/// Secondary indexes with their kind: `GSI` or `LSI`.
fn secondary(table: &Value) -> impl Iterator<Item = (&'static str, &Value)> {
    let of = |field: &str, kind: &'static str| {
        table[field].as_array().into_iter().flatten().map(move |i| (kind, i)).collect::<Vec<_>>()
    };
    of("GlobalSecondaryIndexes", "GSI").into_iter().chain(of("LocalSecondaryIndexes", "LSI"))
}

fn projection_text(index: &Value) -> String {
    let p = &index["Projection"];
    let kind = p["ProjectionType"].as_str().unwrap_or("ALL");
    match p["NonKeyAttributes"].as_array() {
        Some(attrs) if !attrs.is_empty() => {
            format!("{kind} ({})", attrs.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        }
        _ => kind.to_owned(),
    }
}

/// The primary key and the secondary indexes, for the Indexes tab.
pub fn doc_indexes(table: &Value) -> Vec<DocIndex> {
    let mut out = vec![DocIndex {
        name: "Primary key".into(),
        keys: key_text(&table["KeySchema"]),
        unique: true,
        props: vec![("type".into(), "table".into())],
        size_bytes: table["TableSizeBytes"].as_i64(),
    }];
    for (kind, index) in secondary(table) {
        let mut props = vec![("type".into(), kind.to_owned()), ("projection".into(), projection_text(index))];
        if let Some(status) = index["IndexStatus"].as_str() {
            props.push(("status".into(), status.to_owned()));
        }
        if let Some(count) = index["ItemCount"].as_i64() {
            props.push(("items".into(), count.to_string()));
        }
        out.push(DocIndex {
            name: index["IndexName"].as_str().unwrap_or_default().to_owned(),
            keys: key_text(&index["KeySchema"]),
            unique: false,
            props,
            size_bytes: index["IndexSizeBytes"].as_i64(),
        });
    }
    out
}

/// Indexes for the structure view.
pub fn index_infos(table: &Value) -> Vec<IndexInfo> {
    let columns = |schema: &Value| key_schema(schema).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    let mut out = vec![IndexInfo {
        name: "Primary key".into(),
        definition: key_text(&table["KeySchema"]),
        unique: true,
        primary: true,
        columns: columns(&table["KeySchema"]),
        size_bytes: None,
    }];
    out.extend(secondary(table).map(|(kind, index)| IndexInfo {
        name: index["IndexName"].as_str().unwrap_or_default().to_owned(),
        definition: format!("{kind} {} · {}", key_text(&index["KeySchema"]), projection_text(index)),
        unique: false,
        primary: false,
        columns: columns(&index["KeySchema"]),
        size_bytes: index["IndexSizeBytes"].as_i64(),
    }));
    out
}

pub fn constraints(table: &Value) -> Vec<ConstraintInfo> {
    vec![ConstraintInfo {
        kind: ConstraintKind::PrimaryKey,
        name: "Primary key".into(),
        definition: key_text(&table["KeySchema"]),
    }]
}

/// Key attributes first (typed by the table definition), then attributes seen
/// in `sample` with their most common type.
pub fn columns(table: &Value, sample: &[Item]) -> Vec<ColumnInfo> {
    let keys = key_names(table);
    let types = attribute_types(table);
    let column = |name: &str, label: &str, key: bool| ColumnInfo {
        name: name.to_owned(),
        data_type: label.to_owned(),
        nullable: !key,
        default: None,
        primary_key: key,
        generated: false,
        identity: false,
        editable: true,
        numeric: label == "N",
        comment: None,
        read_as_text: false,
    };
    let mut out: Vec<ColumnInfo> =
        keys.iter().map(|k| column(k, types.get(k).map_or("S", String::as_str), true)).collect();
    // name → (label → count), in order of first appearance.
    let mut seen: Vec<(String, HashMap<String, usize>)> = Vec::new();
    for item in sample {
        for (name, av) in item {
            if keys.contains(name) {
                continue;
            }
            let label = values::tagged(av).map_or("?", |(t, _)| t).to_owned();
            match seen.iter_mut().find(|(n, _)| n == name) {
                Some((_, counts)) => *counts.entry(label).or_default() += 1,
                None => seen.push((name.clone(), HashMap::from([(label, 1)]))),
            }
        }
    }
    for (name, counts) in seen {
        let label = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0))).map(|(l, _)| l);
        out.push(column(&name, label.as_deref().unwrap_or("S"), false));
    }
    out
}

/// Key columns for the schema model.
pub fn model_columns(table: &Value) -> Vec<ModelColumn> {
    let types = attribute_types(table);
    key_names(table)
        .into_iter()
        .map(|name| ModelColumn {
            data_type: types.get(&name).cloned().unwrap_or_else(|| "S".into()),
            name,
            nullable: false,
            primary_key: true,
        })
        .collect()
}

/// A `CreateTable` request equivalent to the table, as pretty JSON.
pub fn ddl(table: &Value, ttl: Option<&Value>) -> DbResult<String> {
    if table.is_null() {
        return Err(DbError::not_found("The table was not found."));
    }
    let mut out = Map::new();
    let copy = |out: &mut Map<String, Value>, from: &Value, key: &str| {
        if let Some(v) = from.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_owned(), v.clone());
        }
    };
    for key in ["TableName", "KeySchema", "AttributeDefinitions"] {
        copy(&mut out, table, key);
    }
    let on_demand = table["BillingModeSummary"]["BillingMode"].as_str() == Some("PAY_PER_REQUEST")
        || table["ProvisionedThroughput"]["ReadCapacityUnits"].as_i64() == Some(0);
    let throughput = |from: &Value| {
        json!({
            "ReadCapacityUnits": from["ProvisionedThroughput"]["ReadCapacityUnits"],
            "WriteCapacityUnits": from["ProvisionedThroughput"]["WriteCapacityUnits"],
        })
    };
    if on_demand {
        out.insert("BillingMode".into(), "PAY_PER_REQUEST".into());
    } else {
        out.insert("BillingMode".into(), "PROVISIONED".into());
        out.insert("ProvisionedThroughput".into(), throughput(table));
    }
    let index = |i: &Value, global: bool| {
        let mut m = Map::new();
        for key in ["IndexName", "KeySchema", "Projection"] {
            copy(&mut m, i, key);
        }
        if global && !on_demand {
            m.insert("ProvisionedThroughput".into(), throughput(i));
        }
        Value::Object(m)
    };
    for (field, global) in [("GlobalSecondaryIndexes", true), ("LocalSecondaryIndexes", false)] {
        if let Some(list) = table[field].as_array().filter(|l| !l.is_empty()) {
            out.insert(field.into(), Value::Array(list.iter().map(|i| index(i, global)).collect()));
        }
    }
    if table["StreamSpecification"]["StreamEnabled"].as_bool() == Some(true) {
        copy(&mut out, table, "StreamSpecification");
    }
    if let Some(sse) = table["SSEDescription"]["SSEType"].as_str() {
        out.insert("SSESpecification".into(), json!({ "Enabled": true, "SSEType": sse }));
    }
    if table["DeletionProtectionEnabled"].as_bool() == Some(true) {
        out.insert("DeletionProtectionEnabled".into(), Value::Bool(true));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default();
    if let Some(ttl) = ttl.filter(|t| t["TimeToLiveStatus"].as_str() == Some("ENABLED")) {
        let attribute = ttl["AttributeName"].as_str().unwrap_or_default();
        let _ = write!(text, "\n\n// Time to live: UpdateTimeToLive on attribute \"{attribute}\"");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Value {
        json!({
            "TableName": "orders",
            "KeySchema": [{"AttributeName": "sk", "KeyType": "RANGE"}, {"AttributeName": "pk", "KeyType": "HASH"}],
            "AttributeDefinitions": [
                {"AttributeName": "pk", "AttributeType": "S"},
                {"AttributeName": "sk", "AttributeType": "N"},
                {"AttributeName": "status", "AttributeType": "S"}
            ],
            "BillingModeSummary": {"BillingMode": "PAY_PER_REQUEST"},
            "ItemCount": 7,
            "TableSizeBytes": 700,
            "GlobalSecondaryIndexes": [{
                "IndexName": "by_status",
                "KeySchema": [{"AttributeName": "status", "KeyType": "HASH"}],
                "Projection": {"ProjectionType": "INCLUDE", "NonKeyAttributes": ["total"]},
                "IndexStatus": "ACTIVE", "ItemCount": 3
            }]
        })
    }

    #[test]
    fn keys_indexes_and_ddl() {
        let t = table();
        assert_eq!(key_names(&t), ["pk", "sk"]);
        let idx = doc_indexes(&t);
        assert_eq!(idx[0].keys, "pk (HASH) · sk (RANGE)");
        assert_eq!(idx[1].name, "by_status");
        assert!(idx[1].props.contains(&("projection".into(), "INCLUDE (total)".into())));
        let ddl = ddl(&t, None).unwrap();
        assert!(ddl.contains("\"BillingMode\": \"PAY_PER_REQUEST\"") && ddl.contains("by_status"));
        assert!(!ddl.contains("ItemCount"));
    }

    #[test]
    fn sampled_columns() {
        let sample: Vec<Item> = [
            r#"{"pk":{"S":"a"},"sk":{"N":"1"},"total":{"N":"1.5"},"note":{"S":"x"}}"#,
            r#"{"pk":{"S":"b"},"sk":{"N":"2"},"total":{"S":"n/a"},"tags":{"SS":["q"]}}"#,
            r#"{"pk":{"S":"c"},"sk":{"N":"3"},"total":{"N":"2"}}"#,
        ]
        .iter()
        .map(|s| values::parse_item(s).unwrap())
        .collect();
        let cols = columns(&table(), &sample);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[..2], ["pk", "sk"]);
        let total = cols.iter().find(|c| c.name == "total").unwrap();
        assert_eq!((total.data_type.as_str(), total.numeric, total.nullable), ("N", true, true));
        assert!(cols[1].primary_key && cols[1].numeric);
    }
}
