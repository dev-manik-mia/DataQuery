//! Explorer, structure and definitions from `listDatabases`,
//! `listCollections` and `listIndexes`. Fields are sampled from documents.

use std::collections::HashMap;

use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, Dependencies, Dependency, IndexInfo, ModelColumn, ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::{DbError, DbResult};
use futures_util::{StreamExt, stream};
use mongodb::bson::{Bson, Document, doc};

use crate::client::int_field;
use crate::literal::{compact, pretty};
use crate::session::{MongoSession, command, documents};
use crate::values::{is_numeric, type_label};

/// Documents read to infer a collection's fields.
pub const SAMPLE: i64 = 100;
/// Collections whose document count is read for the explorer.
const MAX_ESTIMATES: usize = 200;

/// One entry of `listCollections`.
pub struct CollectionSpec {
    pub name: String,
    pub view: bool,
    pub options: Document,
}

pub async fn list_collections(session: &MongoSession, database: &str, filter: Document) -> DbResult<Vec<CollectionSpec>> {
    let found = documents(
        &session.client,
        database,
        doc! { "listCollections": 1, "filter": filter, "authorizedCollections": true },
        usize::MAX,
    )
    .await?;
    Ok(found
        .into_iter()
        .filter_map(|d| {
            let name = d.get_str("name").ok()?.to_owned();
            let kind = d.get_str("type").unwrap_or("collection");
            if name.starts_with("system.") || kind == "timeseries-buckets" {
                return None;
            }
            Some(CollectionSpec { name, view: kind == "view", options: d.get_document("options").cloned().unwrap_or_default() })
        })
        .collect())
}

pub async fn spec(session: &MongoSession, object: &ObjectRef) -> DbResult<CollectionSpec> {
    list_collections(session, &object.database, doc! { "name": &object.name })
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| DbError::not_found(format!("{}.{} no longer exists.", object.database, object.name)))
}

fn object(database: &str, spec: &CollectionSpec) -> ObjectRef {
    ObjectRef::new(if spec.view { NodeKind::View } else { NodeKind::Table }, database, None, spec.name.clone())
}

pub async fn databases(session: &MongoSession) -> DbResult<Vec<Node>> {
    let reply = command(
        &session.client,
        "admin",
        doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": true },
        None,
    )
    .await?;
    let current = &session.info_database();
    let mut names: Vec<String> = reply
        .get_array("databases")
        .map(|d| d.iter().filter_map(|d| d.as_document()?.get_str("name").ok().map(str::to_owned)).collect())
        .unwrap_or_default();
    // The connection's database may not exist yet (no data written).
    if !names.contains(current) {
        names.push(current.clone());
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| {
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = &name == current;
            node
        })
        .collect())
}

/// Document count from the collection's metadata (fast, may lag after a crash).
pub async fn estimate(session: &MongoSession, database: &str, collection: &str) -> DbResult<i64> {
    let reply = command(&session.client, database, doc! { "count": collection }, None).await?;
    Ok(int_field(&reply, "n").unwrap_or(0))
}

pub async fn collections(session: &MongoSession, database: &str) -> DbResult<Vec<Node>> {
    let mut specs = list_collections(session, database, Document::new()).await?;
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    let counts: HashMap<String, i64> = stream::iter(specs.iter().filter(|s| !s.view).take(MAX_ESTIMATES))
        .map(|s| async move { estimate(session, database, &s.name).await.ok().map(|n| (s.name.clone(), n)) })
        .buffer_unordered(8)
        .filter_map(|r| async move { r })
        .collect()
        .await;
    Ok(specs
        .iter()
        .map(|s| {
            let mut node = Node::leaf(object(database, s));
            node.row_estimate = counts.get(&s.name).copied();
            node.comment = s.options.get_str("viewOn").ok().map(|v| format!("view on {v}"));
            node
        })
        .collect())
}

pub async fn search(session: &MongoSession, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = mongodb::bson::Regex { pattern: escape_regex(query), options: "i".into() };
    let mut out = Vec::new();
    for db in databases(session).await? {
        if out.len() >= limit as usize {
            break;
        }
        let Ok(specs) = list_collections(session, &db.object.name, doc! { "name": pattern.clone() }).await else {
            continue;
        };
        out.extend(specs.iter().map(|s| Node::leaf(object(&db.object.name, s))));
    }
    out.truncate(limit as usize);
    Ok(out)
}

fn escape_regex(text: &str) -> String {
    text.chars().fold(String::new(), |mut s, c| {
        if "\\^$.|?*+()[]{}".contains(c) {
            s.push('\\');
        }
        s.push(c);
        s
    })
}

/// Up to [`SAMPLE`] documents: a random sample of large collections, else the first ones.
pub async fn sample(session: &MongoSession, object: &ObjectRef) -> DbResult<Vec<Document>> {
    let large = object.kind == NodeKind::Table
        && estimate(session, &object.database, &object.name).await.unwrap_or(0) > SAMPLE * 10;
    let pipeline = if large {
        vec![doc! { "$sample": { "size": SAMPLE } }]
    } else {
        vec![doc! { "$limit": SAMPLE }]
    };
    documents(
        &session.client,
        &object.database,
        doc! { "aggregate": &object.name, "pipeline": pipeline, "cursor": {}, "maxTimeMS": 30_000_i64 },
        SAMPLE as usize,
    )
    .await
}

/// Top-level fields of `docs` with their most common type, `_id` first.
pub fn sampled_columns(docs: &[Document]) -> Vec<ColumnInfo> {
    let mut order: Vec<String> = Vec::new();
    let mut types: HashMap<String, HashMap<&'static str, usize>> = HashMap::new();
    for d in docs {
        for (k, v) in d {
            if !types.contains_key(k) {
                order.push(k.clone());
            }
            *types.entry(k.clone()).or_default().entry(type_label(v)).or_default() += 1;
        }
    }
    if let Some(i) = order.iter().position(|k| k == "_id") {
        let id = order.remove(i);
        order.insert(0, id);
    }
    order
        .into_iter()
        .map(|name| {
            // The most common type that is not null.
            let label = types[&name]
                .iter()
                .filter(|(t, _)| **t != "Null")
                .max_by_key(|(t, n)| (**n, std::cmp::Reverse(**t)))
                .map_or("Null", |(t, _)| *t);
            ColumnInfo {
                primary_key: name == "_id",
                nullable: name != "_id",
                numeric: is_numeric(label),
                data_type: label.to_owned(),
                name,
                default: None,
                generated: false,
                identity: false,
                editable: true,
                comment: None,
                read_as_text: false,
            }
        })
        .collect()
}

pub async fn list_indexes(session: &MongoSession, object: &ObjectRef) -> DbResult<Vec<Document>> {
    if object.kind == NodeKind::View {
        return Ok(Vec::new());
    }
    documents(&session.client, &object.database, doc! { "listIndexes": &object.name, "cursor": {} }, 10_000).await
}

/// Storage sizes: collection data, total index size and per-index sizes.
pub async fn storage(session: &MongoSession, object: &ObjectRef) -> Option<Document> {
    documents(
        &session.client,
        &object.database,
        doc! { "aggregate": &object.name, "pipeline": [{ "$collStats": { "storageStats": {} } }], "cursor": {} },
        1,
    )
    .await
    .ok()?
    .into_iter()
    .next()?
    .get_document("storageStats")
    .ok()
    .cloned()
}

pub async fn structure(session: &MongoSession, object: &ObjectRef) -> DbResult<TableStructure> {
    let spec = spec(session, object).await?;
    let object = self::object(&object.database, &spec);
    let docs = sample(session, &object).await?;
    let mut columns = sampled_columns(&docs);
    if columns.is_empty() && !spec.view {
        columns = sampled_columns(&[doc! { "_id": Bson::Null }]);
        columns[0].data_type = "ObjectId".into();
    }
    let stats = storage(session, &object).await;
    let index_sizes = stats.as_ref().and_then(|s| s.get_document("indexSizes").ok()).cloned().unwrap_or_default();
    let indexes = list_indexes(session, &object)
        .await?
        .iter()
        .map(|ix| {
            let name = ix.get_str("name").unwrap_or_default().to_owned();
            let keys = ix.get_document("key").cloned().unwrap_or_default();
            IndexInfo {
                definition: compact(&Bson::Document(keys.clone())),
                unique: ix.get_bool("unique").unwrap_or(false) || name == "_id_",
                primary: name == "_id_",
                columns: keys.keys().cloned().collect(),
                size_bytes: int_field(&index_sizes, &name),
                name,
            }
        })
        .collect();
    let size = stats.as_ref().map(|s| TableSize {
        total_bytes: int_field(s, "storageSize").unwrap_or(0) + int_field(s, "totalIndexSize").unwrap_or(0),
        table_bytes: int_field(s, "storageSize").unwrap_or(0),
        index_bytes: int_field(s, "totalIndexSize").unwrap_or(0),
    });
    Ok(TableStructure {
        row_identity: if spec.view { Vec::new() } else { vec!["_id".into()] },
        row_estimate: stats.as_ref().and_then(|s| int_field(s, "count")),
        comment: spec.options.get_str("viewOn").ok().map(|v| format!("view on {v}")),
        object,
        columns,
        constraints: Vec::new(),
        indexes,
        full_row_key: Vec::new(),
        foreign_keys: Vec::new(),
        size,
    })
}

/// `db.createCollection(…)` / `db.createView(…)` and the indexes, as shell text.
pub async fn ddl(session: &MongoSession, object: &ObjectRef) -> DbResult<String> {
    let spec = spec(session, object).await?;
    let quoted = |s: &str| crate::literal::quote(s);
    if spec.view {
        let source = spec.options.get_str("viewOn").unwrap_or_default();
        let pipeline = spec.options.get("pipeline").cloned().unwrap_or(Bson::Array(Vec::new()));
        return Ok(format!("db.createView({}, {}, {})", quoted(&spec.name), quoted(source), pretty(&pipeline)));
    }
    let mut out = if spec.options.is_empty() {
        format!("db.createCollection({})", quoted(&spec.name))
    } else {
        format!("db.createCollection({}, {})", quoted(&spec.name), pretty(&Bson::Document(spec.options.clone())))
    };
    for ix in list_indexes(session, object).await? {
        let name = ix.get_str("name").unwrap_or_default();
        if name == "_id_" {
            continue;
        }
        let keys = ix.get("key").cloned().unwrap_or(Bson::Document(Document::new()));
        let mut options = ix.clone();
        for k in ["key", "v", "ns"] {
            options.remove(k);
        }
        out.push_str(&format!(
            "\ndb.getCollection({}).createIndex({}, {})",
            quoted(&spec.name),
            compact(&keys),
            compact(&Bson::Document(options))
        ));
    }
    Ok(out)
}

pub async fn dependencies(session: &MongoSession, object: &ObjectRef) -> DbResult<Dependencies> {
    let specs = list_collections(session, &object.database, Document::new()).await?;
    let mut deps = Dependencies::default();
    if let Some(me) = specs.iter().find(|s| s.name == object.name)
        && let Ok(source) = me.options.get_str("viewOn")
        && let Some(src) = specs.iter().find(|s| s.name == source)
    {
        deps.uses.push(Dependency { object: self::object(&object.database, src), relation: "view source".into() });
    }
    for s in specs.iter().filter(|s| s.options.get_str("viewOn").ok() == Some(object.name.as_str())) {
        deps.used_by.push(Dependency { object: self::object(&object.database, s), relation: "view".into() });
    }
    Ok(deps)
}

pub async fn schema_model(session: &MongoSession, database: &str) -> DbResult<SchemaModel> {
    let specs = list_collections(session, database, Document::new()).await?;
    let truncated = specs.len() > SchemaModel::MAX_TABLES;
    let tables = stream::iter(specs.iter().take(SchemaModel::MAX_TABLES))
        .map(|s| async move {
            let object = object(database, s);
            let docs = sample(session, &object).await.unwrap_or_default();
            let columns = sampled_columns(&docs)
                .into_iter()
                .map(|c| ModelColumn { name: c.name, data_type: c.data_type, nullable: c.nullable, primary_key: c.primary_key })
                .collect();
            ModelTable { object, columns }
        })
        .buffered(8)
        .collect()
        .await;
    Ok(SchemaModel { tables, foreign_keys: Vec::new(), truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_picks_common_types() {
        let cols = sampled_columns(&[
            doc! { "a": 1, "_id": 1, "b": Bson::Null },
            doc! { "_id": 2, "a": "x", "b": 2.5 },
            doc! { "_id": 3, "a": 3 },
        ]);
        let got: Vec<(&str, &str)> = cols.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
        assert_eq!(got, [("_id", "Int32"), ("a", "Int32"), ("b", "Double")]);
        assert!(cols[0].primary_key && cols[1].numeric);
        assert_eq!(escape_regex("a.b*"), "a\\.b\\*");
    }
}
