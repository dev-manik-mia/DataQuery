//! Indices, aliases and data streams (`_cat`, `_data_stream`), mappings as
//! columns, and the `PUT` request that recreates an index.

use std::collections::BTreeMap;

use dq_core::document::DocIndex;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::schema::{ColumnInfo, TableSize, TableStructure};
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::values::{FieldTypes, is_numeric_type, mapping_types};

/// Index settings worth recreating; the rest are internal (uuid, creation date, version…).
const KEPT_SETTINGS: &[&str] = &[
    "number_of_shards",
    "number_of_replicas",
    "refresh_interval",
    "max_result_window",
    "analysis",
    "codec",
    "sort",
    "similarity",
    "mapping",
    "default_pipeline",
    "final_pipeline",
    "hidden",
    "lifecycle",
    "routing_path",
    "mode",
];

/// Percent-encodes a name for a URL path segment (index names are mostly safe already).
pub fn segment(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' | b',' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// One `_cat/indices` row.
#[derive(Debug, Clone)]
pub struct IndexRow {
    pub name: String,
    pub docs: Option<i64>,
    pub bytes: Option<i64>,
    pub health: String,
    pub status: String,
    pub primaries: Option<i64>,
    pub replicas: Option<i64>,
}

/// Indices matching `target` (`*` for all), hidden ones (`.name`) last.
pub async fn indices(client: &Client, target: &str) -> DbResult<Vec<IndexRow>> {
    let path = format!(
        "/_cat/indices/{}?format=json&bytes=b&expand_wildcards=open,hidden&h=index,docs.count,store.size,health,status,pri,rep",
        segment(target)
    );
    let rows = match client.get(&path).await {
        Ok(v) => v,
        Err(e) if e.kind == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<IndexRow> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| IndexRow {
            name: r["index"].as_str().unwrap_or_default().to_owned(),
            docs: number(&r["docs.count"]),
            bytes: number(&r["store.size"]),
            health: r["health"].as_str().unwrap_or_default().to_owned(),
            status: r["status"].as_str().unwrap_or_default().to_owned(),
            primaries: number(&r["pri"]),
            replicas: number(&r["rep"]),
        })
        .collect();
    out.sort_by(|a, b| (a.name.starts_with('.'), &a.name).cmp(&(b.name.starts_with('.'), &b.name)));
    Ok(out)
}

/// `(alias, index)` pairs.
pub async fn aliases(client: &Client) -> DbResult<Vec<(String, String)>> {
    let rows = client.get("/_cat/aliases?format=json&h=alias,index").await?;
    let mut out: Vec<(String, String)> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            (r["alias"].as_str().unwrap_or_default().to_owned(), r["index"].as_str().unwrap_or_default().to_owned())
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Data stream names; empty when the server has none or does not support them.
pub async fn data_streams(client: &Client) -> Vec<String> {
    client.get("/_data_stream").await.ok().map_or_else(Vec::new, |v| {
        v["data_streams"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["name"].as_str().map(str::to_owned))
            .collect()
    })
}

/// Explorer children of the cluster node: indices (backing indices of data
/// streams hidden), data streams, then aliases.
pub async fn children(client: &Client, database: &str) -> DbResult<Vec<Node>> {
    let (indices, aliases, streams) = tokio::join!(indices(client, "*"), aliases(client), data_streams(client));
    let mut nodes = Vec::new();
    for row in indices? {
        if row.name.starts_with(".ds-") {
            continue;
        }
        let mut node = Node::leaf(ObjectRef::new(NodeKind::Table, database, None, row.name));
        node.row_estimate = row.docs;
        nodes.push(node);
    }
    for stream in streams {
        let mut node = Node::leaf(ObjectRef::new(NodeKind::Table, database, None, stream));
        node.comment = Some("data stream".into());
        nodes.push(node);
    }
    let mut seen = Vec::new();
    for (alias, index) in aliases? {
        if seen.contains(&alias) {
            continue;
        }
        let mut node = Node::leaf(ObjectRef::new(NodeKind::View, database, None, alias.clone()));
        node.comment = Some(format!("alias of {index}"));
        seen.push(alias);
        nodes.push(node);
    }
    Ok(nodes)
}

/// Merged mapping properties of every index behind `target`.
pub async fn mapping(client: &Client, target: &str) -> DbResult<Value> {
    let response = client.get(&format!("/{}/_mapping", segment(target))).await?;
    let mut merged = Map::new();
    for index in response.as_object().into_iter().flatten().map(|(_, v)| v) {
        for (name, def) in index["mappings"]["properties"].as_object().into_iter().flatten() {
            merged.entry(name.clone()).or_insert_with(|| def.clone());
        }
    }
    Ok(Value::Object(merged))
}

/// Field types by dotted path.
pub async fn field_types(client: &Client, target: &str) -> DbResult<FieldTypes> {
    let mut types = Vec::new();
    mapping_types(&mapping(client, target).await?, "", &mut types);
    Ok(types.into_iter().collect())
}

fn column(name: &str, data_type: &str, primary_key: bool) -> ColumnInfo {
    ColumnInfo {
        name: name.to_owned(),
        data_type: data_type.to_owned(),
        nullable: !primary_key,
        default: None,
        primary_key,
        generated: primary_key,
        identity: false,
        editable: false,
        numeric: is_numeric_type(data_type),
        comment: None,
        read_as_text: false,
    }
}

/// `_id`, then every mapped field by dotted path.
pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let mut types = Vec::new();
    mapping_types(&mapping(client, &object.name).await?, "", &mut types);
    let mut columns = vec![column("_id", "keyword", true)];
    columns.extend(types.iter().map(|(path, kind)| column(path, kind, false)));
    let stats = client.get(&format!("/{}/_stats/docs,store", segment(&object.name))).await.ok();
    let all = stats.as_ref().map(|s| &s["_all"]);
    let docs = all.and_then(|a| a["primaries"]["docs"]["count"].as_i64());
    let bytes = all.and_then(|a| a["total"]["store"]["size_in_bytes"].as_i64());
    Ok(TableStructure {
        object: object.clone(),
        columns,
        constraints: Vec::new(),
        indexes: Vec::new(),
        row_identity: vec!["_id".into()],
        full_row_key: Vec::new(),
        comment: None,
        row_estimate: docs,
        foreign_keys: Vec::new(),
        size: bytes.map(|b| TableSize { total_bytes: b, table_bytes: b, index_bytes: 0 }),
    })
}

/// Keeps the settings that define the index, as a nested object.
fn kept_settings(index_settings: &Value) -> Value {
    let kept: Map<String, Value> = index_settings
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| KEPT_SETTINGS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    json!({ "index": kept })
}

/// `PUT /index` with its settings and mappings, or `POST _aliases` for an alias.
pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    if object.kind == NodeKind::View {
        let pairs = aliases(client).await?;
        let actions: Vec<Value> = pairs
            .iter()
            .filter(|(alias, _)| *alias == object.name)
            .map(|(alias, index)| json!({ "add": { "index": index, "alias": alias } }))
            .collect();
        if actions.is_empty() {
            return Err(DbError::not_found(format!("No alias named {}.", object.name)));
        }
        return Ok(format!(
            "POST /_aliases\n{}",
            serde_json::to_string_pretty(&json!({ "actions": actions })).unwrap_or_default()
        ));
    }
    let name = segment(&object.name);
    let (settings_path, mapping_path) = (format!("/{name}/_settings"), format!("/{name}/_mapping"));
    let (settings, mapping) = tokio::join!(client.get(&settings_path), client.get(&mapping_path));
    let (settings, mapping) = (settings?, mapping?);
    let settings = settings
        .as_object()
        .and_then(|m| m.values().next())
        .map(|i| i["settings"]["index"].clone())
        .unwrap_or_default();
    let mappings =
        mapping.as_object().and_then(|m| m.values().next()).map(|i| i["mappings"].clone()).unwrap_or_default();
    let body = json!({ "settings": kept_settings(&settings), "mappings": mappings });
    Ok(format!("PUT /{}\n{}", object.name, serde_json::to_string_pretty(&body).unwrap_or_default()))
}

/// Index-level summaries for the Indexes tab: shards, replicas, health, documents, size, aliases.
pub async fn index_summaries(client: &Client, target: &str) -> DbResult<Vec<DocIndex>> {
    let (rows, pairs) = tokio::join!(indices(client, target), aliases(client));
    let pairs = pairs.unwrap_or_default();
    let mut by_index: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (alias, index) in &pairs {
        by_index.entry(index.as_str()).or_default().push(alias.as_str());
    }
    Ok(rows?
        .into_iter()
        .map(|r| {
            let mut props = vec![
                ("health".to_owned(), r.health.clone()),
                ("status".to_owned(), r.status.clone()),
                ("docs".to_owned(), r.docs.map_or_else(|| "—".into(), |d| d.to_string())),
            ];
            if let Some(a) = by_index.get(r.name.as_str()) {
                props.push(("aliases".into(), a.join(", ")));
            }
            DocIndex {
                keys: format!(
                    "{} shards · {} replicas",
                    r.primaries.map_or_else(|| "?".into(), |p| p.to_string()),
                    r.replicas.map_or_else(|| "?".into(), |p| p.to_string())
                ),
                name: r.name,
                unique: false,
                props,
                size_bytes: r.bytes,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_defining_settings_only() {
        let s =
            kept_settings(&json!({"number_of_shards": "1", "uuid": "x", "creation_date": "1", "analysis": {"a": 1}}));
        assert_eq!(s, json!({"index": {"number_of_shards": "1", "analysis": {"a": 1}}}));
        assert_eq!(segment("logs-2026.10/x y"), "logs-2026.10%2Fx%20y");
    }
}
