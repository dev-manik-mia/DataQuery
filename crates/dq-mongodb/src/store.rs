//! The Documents view: find, count, aggregate, indexes, validation and
//! single-document edits in shell syntax.

use std::fmt::Write;
use std::time::Instant;

use async_trait::async_trait;
use dq_core::document::{DocIndex, DocumentFeatures, DocumentPage, DocumentStore, FindRequest};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::schema::ColumnInfo;
use dq_core::table::RowCount;
use dq_core::{DbError, DbResult, ErrorKind};
use mongodb::bson::{Bson, Decimal128, Document, doc, oid::ObjectId};

use crate::catalog;
use crate::client::int_field;
use crate::literal::{self, compact, parse_document, pretty};
use crate::session::{MongoSession, command, documents};
use crate::values;

/// Reads take at most this long in the Documents view.
const READ_MS: i64 = 60_000;

pub const FEATURES: DocumentFeatures = DocumentFeatures {
    language: "MQL",
    filter_example: "{ status: \"paid\", total: { $gte: 100 } }",
    projection_example: "{ email: 1, total: 1 }",
    sort: true,
    aggregation: true,
    validation: true,
    text_format: "shell",
    new_document: "{\n  \n}",
    filtered_count: true,
};

fn changed() -> DbError {
    DbError::not_found("The document changed or was deleted. Reload.")
}

/// Parses the `_id` sent back by the UI.
fn parse_key(key: &str) -> DbResult<Bson> {
    literal::parse(key)
}

/// Parses an edited document.
fn parse_edit(text: &str) -> DbResult<Document> {
    match literal::parse(text)? {
        Bson::Document(d) => Ok(d),
        _ => Err(DbError::new(ErrorKind::Syntax, "A document must be an object: { … }.")),
    }
}

fn pipeline(text: &str) -> DbResult<Vec<Bson>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    match literal::parse(text)? {
        Bson::Array(stages) if stages.iter().all(|s| matches!(s, Bson::Document(_))) => Ok(stages),
        Bson::Document(stage) => Ok(vec![Bson::Document(stage)]),
        _ => Err(DbError::new(ErrorKind::Syntax, "A pipeline is an array of stages: [{ $match: { … } }, …].")),
    }
}

/// A text value converted to the type of its column (or inferred when the type is unknown).
pub fn convert(value: Option<&str>, label: &str) -> DbResult<Bson> {
    let Some(text) = value else { return Ok(Bson::Null) };
    let t = text.trim();
    let bad = |what: &str| DbError::invalid(format!("`{text}` is not {what}."));
    Ok(match label {
        // A value too large for Int32 is widened rather than refused.
        "Int32" => {
            let v: i64 = t.parse().map_err(|_| bad("a whole number"))?;
            i32::try_from(v).map_or(Bson::Int64(v), Bson::Int32)
        }
        "Int64" => Bson::Int64(t.parse().map_err(|_| bad("a whole number"))?),
        "Double" => Bson::Double(t.parse().map_err(|_| bad("a number"))?),
        "Decimal128" => Bson::Decimal128(t.parse::<Decimal128>().map_err(|_| bad("a decimal number"))?),
        "Boolean" => match t.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Bson::Boolean(true),
            "false" | "f" | "no" | "n" | "0" => Bson::Boolean(false),
            _ => return Err(bad("true or false")),
        },
        "Date" => Bson::DateTime(literal::parse_date(t).ok_or_else(|| bad("an ISO 8601 date"))?),
        "ObjectId" => Bson::ObjectId(ObjectId::parse_str(t).map_err(|_| bad("an ObjectId (24 hex digits)"))?),
        "Object" | "Array" => literal::parse(t)?,
        "Null" | "" => infer(text),
        _ => Bson::String(text.to_owned()),
    })
}

/// Type of an imported value in a field the collection does not have yet.
fn infer(text: &str) -> Bson {
    let t = text.trim();
    if let Ok(v) = t.parse::<i64>() {
        return i32::try_from(v).map_or(Bson::Int64(v), Bson::Int32);
    }
    if t.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c))
        && let Ok(v) = t.parse::<f64>()
    {
        return Bson::Double(v);
    }
    match t {
        "true" => return Bson::Boolean(true),
        "false" => return Bson::Boolean(false),
        _ => {}
    }
    if ((t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')))
        && let Ok(v) = literal::parse(t)
    {
        return v;
    }
    Bson::String(text.to_owned())
}

impl MongoSession {
    fn find_command(request: &FindRequest, skip: u64, limit: u32) -> DbResult<Document> {
        let o = &request.object;
        let mut c = doc! { "find": &o.name, "skip": skip as i64, "limit": i64::from(limit) + 1, "maxTimeMS": READ_MS };
        let filter = parse_document(&request.filter, "filter")?;
        let projection = parse_document(&request.projection, "projection")?;
        let sort = parse_document(&request.sort, "sort")?;
        if !filter.is_empty() {
            c.insert("filter", filter);
        }
        if !projection.is_empty() {
            c.insert("projection", projection);
        }
        if !sort.is_empty() {
            c.insert("sort", sort);
        }
        Ok(c)
    }

    fn page(docs: Vec<Document>, limit: u32, next: impl FnOnce() -> String, started: Instant) -> DocumentPage {
        let more = docs.len() > limit as usize;
        DocumentPage {
            documents: docs.iter().take(limit as usize).map(values::document).collect(),
            next: more.then(next),
            duration_ms: started.elapsed().as_millis() as u64,
        }
    }

    fn writable(&self, object: &ObjectRef) -> DbResult<()> {
        self.refuse_if_read_only()?;
        if object.kind == NodeKind::View {
            return Err(DbError::invalid("Views are read-only; change the documents in the source collection."));
        }
        Ok(())
    }

    async fn write(&self, object: &ObjectRef, cmd: Document) -> DbResult<Document> {
        command(&self.client, &object.database, cmd, None).await
    }
}

#[async_trait]
impl DocumentStore for MongoSession {
    fn features(&self) -> DocumentFeatures {
        FEATURES
    }

    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let limit = request.limit();
        let skip = match &request.cursor {
            None => 0,
            Some(c) => c.parse::<u64>().map_err(|_| DbError::invalid("Invalid page cursor."))?,
        };
        let cmd = Self::find_command(request, skip, limit)?;
        let docs = documents(&self.client, &request.object.database, cmd, limit as usize + 1).await?;
        Ok(Self::page(docs, limit, || (skip + u64::from(limit)).to_string(), started))
    }

    async fn count(&self, object: &ObjectRef, filter: &str) -> DbResult<Option<RowCount>> {
        let filter = parse_document(filter, "filter")?;
        if filter.is_empty() && object.kind == NodeKind::Table {
            let estimate = catalog::estimate(self, &object.database, &object.name).await?;
            if estimate > dq_core::ops::EXACT_COUNT_THRESHOLD {
                return Ok(Some(RowCount { count: estimate, exact: false }));
            }
        }
        let cmd = doc! {
            "aggregate": &object.name,
            "pipeline": [{ "$match": filter }, { "$count": "n" }],
            "cursor": {},
            "maxTimeMS": READ_MS,
        };
        let docs = documents(&self.client, &object.database, cmd, 1).await?;
        let count = docs.first().and_then(|d| int_field(d, "n")).unwrap_or(0);
        Ok(Some(RowCount { count, exact: true }))
    }

    async fn aggregate(&self, object: &ObjectRef, pipeline_text: &str, limit: u32) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let limit = limit.clamp(1, dq_core::document::MAX_PAGE);
        let stages = pipeline(pipeline_text)?;
        let writes =
            stages.iter().any(|s| s.as_document().is_some_and(|d| d.contains_key("$out") || d.contains_key("$merge")));
        if writes {
            self.refuse_if_read_only()?;
            return Err(DbError::invalid("$out and $merge write data; run them from the query editor."));
        }
        let mut full = stages;
        full.push(Bson::Document(doc! { "$limit": i64::from(limit) + 1 }));
        let cmd = doc! { "aggregate": &object.name, "pipeline": full, "cursor": {}, "maxTimeMS": READ_MS };
        let docs = documents(&self.client, &object.database, cmd, limit as usize + 1).await?;
        // There is no next page: refine the pipeline instead.
        let mut page = Self::page(docs, limit, String::new, started);
        page.next = None;
        Ok(page)
    }

    async fn indexes(&self, object: &ObjectRef) -> DbResult<Vec<DocIndex>> {
        let sizes = catalog::storage(self, object)
            .await
            .and_then(|s| s.get_document("indexSizes").ok().cloned())
            .unwrap_or_default();
        Ok(catalog::list_indexes(self, object)
            .await?
            .into_iter()
            .map(|ix| {
                let name = ix.get_str("name").unwrap_or_default().to_owned();
                let props = ix
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "name" | "key" | "v" | "ns" | "unique"))
                    .map(|(k, v)| {
                        let label = if k == "expireAfterSeconds" { "ttl" } else { k.as_str() };
                        (
                            label.to_owned(),
                            match v {
                                Bson::String(s) => s.clone(),
                                other => compact(other),
                            },
                        )
                    })
                    .collect();
                DocIndex {
                    keys: ix.get("key").map(compact).unwrap_or_default(),
                    unique: ix.get_bool("unique").unwrap_or(false) || name == "_id_",
                    props,
                    size_bytes: int_field(&sizes, &name),
                    name,
                }
            })
            .collect())
    }

    async fn validation(&self, object: &ObjectRef) -> DbResult<Option<String>> {
        let spec = catalog::spec(self, object).await?;
        let mut rules = Document::new();
        for key in ["validator", "validationLevel", "validationAction"] {
            if let Some(v) = spec.options.get(key) {
                rules.insert(key, v.clone());
            }
        }
        Ok(rules.contains_key("validator").then(|| pretty(&Bson::Document(rules))))
    }

    async fn insert(&self, object: &ObjectRef, text: &str) -> DbResult<String> {
        self.writable(object)?;
        let mut doc = parse_edit(text)?;
        if !doc.contains_key("_id") {
            let mut with_id = doc! { "_id": ObjectId::new() };
            with_id.extend(doc);
            doc = with_id;
        }
        let id = doc.get("_id").cloned().unwrap_or(Bson::Null);
        self.write(object, doc! { "insert": &object.name, "documents": [doc] }).await?;
        Ok(compact(&id))
    }

    async fn replace(&self, object: &ObjectRef, key: &str, text: &str) -> DbResult<()> {
        self.writable(object)?;
        let id = parse_key(key)?;
        let mut doc = parse_edit(text)?;
        match doc.get("_id") {
            None => {}
            Some(v) if *v == id => {}
            Some(_) => return Err(DbError::invalid("The _id cannot be changed. Insert a new document instead.")),
        }
        doc.remove("_id");
        let reply = self
            .write(
                object,
                doc! { "update": &object.name, "updates": [{ "q": { "_id": id }, "u": doc, "multi": false }] },
            )
            .await?;
        if int_field(&reply, "n") != Some(1) {
            return Err(changed());
        }
        Ok(())
    }

    async fn delete(&self, object: &ObjectRef, key: &str) -> DbResult<()> {
        self.writable(object)?;
        let id = parse_key(key)?;
        let reply = self
            .write(object, doc! { "delete": &object.name, "deletes": [{ "q": { "_id": id }, "limit": 1 }] })
            .await?;
        if int_field(&reply, "n") != Some(1) {
            return Err(changed());
        }
        Ok(())
    }

    fn find_statement(&self, request: &FindRequest) -> DbResult<String> {
        let part = |text: &str, what: &str| -> DbResult<Option<String>> {
            let d = parse_document(text, what)?;
            Ok((!d.is_empty()).then(|| compact(&Bson::Document(d))))
        };
        let filter = part(&request.filter, "filter")?;
        let projection = part(&request.projection, "projection")?;
        let sort = part(&request.sort, "sort")?;
        let mut out = format!("db.getCollection({}).find(", literal::quote(&request.object.name));
        match (&filter, &projection) {
            (_, Some(p)) => {
                let _ = write!(out, "{}, {p}", filter.as_deref().unwrap_or("{}"));
            }
            (Some(f), None) => out.push_str(f),
            (None, None) => {}
        }
        out.push(')');
        if let Some(s) = sort {
            let _ = write!(out, ".sort({s})");
        }
        Ok(out)
    }

    async fn insert_rows(
        &self,
        object: &ObjectRef,
        columns: &[ColumnInfo],
        rows: Vec<Vec<Option<String>>>,
    ) -> DbResult<u64> {
        self.writable(object)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let docs = rows
            .into_iter()
            .map(|row| {
                let mut d = Document::new();
                for (column, value) in columns.iter().zip(row) {
                    d.insert(column.name.clone(), convert(value.as_deref(), &column.data_type)?);
                }
                Ok(Bson::Document(d))
            })
            .collect::<DbResult<Vec<_>>>()?;
        let reply = self.write(object, doc! { "insert": &object.name, "documents": docs, "ordered": true }).await?;
        Ok(int_field(&reply, "n").unwrap_or(0).max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_convert_by_type() {
        assert_eq!(convert(Some("9007199254740993"), "Int64").unwrap(), Bson::Int64(9_007_199_254_740_993));
        assert_eq!(
            convert(Some("12.50"), "Decimal128").unwrap().to_string(),
            Bson::Decimal128("12.50".parse().unwrap()).to_string()
        );
        assert_eq!(convert(None, "String").unwrap(), Bson::Null);
        assert!(convert(Some("x"), "Int32").is_err());
        assert_eq!(convert(Some("5"), "").unwrap(), Bson::Int32(5));
        assert_eq!(convert(Some("5000000000"), "").unwrap(), Bson::Int64(5_000_000_000));
        assert_eq!(convert(Some("2.5"), "").unwrap(), Bson::Double(2.5));
        assert_eq!(convert(Some("true"), "").unwrap(), Bson::Boolean(true));
        assert_eq!(convert(Some("{ a: 1 }"), "").unwrap(), Bson::Document(doc! { "a": 1 }));
        assert_eq!(convert(Some("e-mail"), "").unwrap(), Bson::String("e-mail".into()));
        assert!(matches!(convert(Some("2026-10-01"), "Date").unwrap(), Bson::DateTime(_)));
    }
}
