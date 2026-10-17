//! The Documents view: find, count, aggregate, indexes, validation and
//! single-document edits in shell syntax.

use std::fmt::Write;
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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

/// BSON types in sort order, grouped as queries compare them (numbers with
/// numbers, strings with symbols); `$gt` / `$lt` only match within a group.
const TYPE_ORDER: &[&[&str]] = &[
    &["minKey"],
    &["undefined"],
    &["null"],
    &["double", "int", "long", "decimal"],
    &["symbol", "string"],
    &["object"],
    &["array"],
    &["binData"],
    &["objectId"],
    &["bool"],
    &["date"],
    &["timestamp"],
    &["regex"],
    &["dbPointer"],
    &["javascript"],
    &["javascriptWithScope"],
    &["maxKey"],
];

/// The [`TYPE_ORDER`] group of an `_id` a page may continue after. `None` for
/// values that compare with everything (`MinKey`, `MaxKey`), deprecated
/// `undefined` and NaN: those pages continue by offset.
fn type_group(v: &Bson) -> Option<usize> {
    Some(match v {
        Bson::Null => 2,
        Bson::Double(d) if d.is_nan() => return None,
        Bson::Double(_) | Bson::Int32(_) | Bson::Int64(_) | Bson::Decimal128(_) => 3,
        Bson::Symbol(_) | Bson::String(_) => 4,
        Bson::Document(_) => 5,
        Bson::Array(_) => 6,
        Bson::Binary(_) => 7,
        Bson::ObjectId(_) => 8,
        Bson::Boolean(_) => 9,
        Bson::DateTime(_) => 10,
        Bson::Timestamp(_) => 11,
        Bson::RegularExpression(_) => 12,
        Bson::DbPointer(_) => 13,
        Bson::JavaScriptCode(_) => 14,
        Bson::JavaScriptCodeWithScope(_) => 15,
        Bson::MinKey | Bson::MaxKey | Bson::Undefined => return None,
    })
}

/// Documents after `id` in `_id` order: a greater value of the same type group,
/// or any value of a later group (BSON order across types, as the sort uses).
fn after_id(id: &Bson, ascending: bool) -> Option<Document> {
    let group = type_group(id)?;
    let others: Vec<&str> = TYPE_ORDER
        .iter()
        .enumerate()
        .filter(|(g, _)| if ascending { *g > group } else { *g < group })
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let mut or = vec![Bson::Document(doc! { "_id": { if ascending { "$gt" } else { "$lt" }: id.clone() } })];
    if !others.is_empty() {
        or.push(Bson::Document(doc! { "_id": { "$type": others } }));
    }
    Some(doc! { "$or": or })
}

/// A sort key's direction: 1 or -1 (`None` for `{ $meta: … }` and other forms).
fn direction(v: &Bson) -> Option<i64> {
    let n = match v {
        Bson::Int32(n) => i64::from(*n),
        Bson::Int64(n) => *n,
        Bson::Double(d) if d.fract() == 0.0 => *d as i64,
        _ => return None,
    };
    matches!(n, 1 | -1).then_some(n)
}

/// `Some(ascending)` when pages are in `_id` order: no sort (on collections) or `{ _id: ±1 }`.
fn id_order(sort: &Document) -> Option<bool> {
    match sort.iter().collect::<Vec<_>>().as_slice() {
        [] => Some(true),
        [(key, dir)] if key.as_str() == "_id" => direction(dir).map(|d| d == 1),
        _ => None,
    }
}

/// True when `index` returns documents in `sort` order (or its reverse), so ties
/// come back in a stable order without an `_id` tiebreaker.
fn index_serves(index: &Document, sort: &Document) -> bool {
    if index.get_bool("sparse").unwrap_or(false) || index.contains_key("partialFilterExpression") {
        return false;
    }
    let Ok(key) = index.get_document("key") else { return false };
    if key.len() < sort.len() {
        return false;
    }
    let mut reversed = None;
    sort.iter().zip(key.iter()).all(|((sk, sv), (ik, iv))| {
        let (Some(a), Some(b)) = (direction(sv), direction(iv)) else { return false };
        sk == ik && *reversed.get_or_insert(a != b) == (a != b)
    })
}

/// Where a page starts. A plain number is an offset (views, time series, user
/// sorts, older cursors); `k…` is a page in `_id` order continuing after an `_id`,
/// with the offset kept for when the last document had no usable `_id`.
#[derive(Debug, Default, PartialEq)]
struct PageCursor {
    offset: u64,
    /// Pages in `_id` order.
    seek: bool,
    /// The `_id` to continue after.
    after: Option<Bson>,
}

impl PageCursor {
    fn parse(cursor: Option<&str>) -> DbResult<Self> {
        let invalid = || DbError::invalid("Invalid page cursor.");
        let Some(c) = cursor else { return Ok(Self::default()) };
        if let Some(token) = c.strip_prefix('k') {
            let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
            let d = Document::from_reader(&mut bytes.as_slice()).map_err(|_| invalid())?;
            let offset = d.get_i64("o").map_err(|_| invalid())?;
            return Ok(Self { offset: offset.max(0) as u64, seek: true, after: d.get("id").cloned() });
        }
        Ok(Self { offset: c.parse().map_err(|_| invalid())?, ..Self::default() })
    }

    fn token(offset: u64, id: Option<&Bson>) -> String {
        let mut d = doc! { "o": offset as i64 };
        if let Some(id) = id {
            d.insert("id", id.clone());
        }
        let mut bytes = Vec::new();
        let _ = d.to_writer(&mut bytes);
        format!("k{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

impl MongoSession {
    fn find_command(
        name: &str,
        filter: Document,
        projection: Document,
        sort: Document,
        skip: u64,
        limit: u32,
    ) -> Document {
        let mut c = doc! { "find": name, "skip": skip as i64, "limit": i64::from(limit) + 1, "maxTimeMS": READ_MS };
        if !filter.is_empty() {
            c.insert("filter", filter);
        }
        if !projection.is_empty() {
            c.insert("projection", projection);
        }
        if !sort.is_empty() {
            c.insert("sort", sort);
        }
        c
    }

    /// Pages in `_id` order on a collection: the first page unless the
    /// collection is a time series (no `_id` index to walk), and every page whose
    /// cursor is a seek token.
    async fn id_paging(&self, object: &ObjectRef, sort: &Document, cursor: &PageCursor) -> DbResult<Option<bool>> {
        let Some(ascending) = id_order(sort).filter(|_| object.kind == NodeKind::Table) else { return Ok(None) };
        Ok(match cursor {
            PageCursor { seek: true, .. } => Some(ascending),
            _ if !sort.is_empty() => Some(ascending),
            PageCursor { offset: 0, .. } => {
                let spec = catalog::spec(self, object).await.ok();
                (!spec.is_some_and(|s| s.options.contains_key("timeseries"))).then_some(ascending)
            }
            // An offset without a sort: natural order, as the first page was.
            _ => None,
        })
    }

    /// `sort` plus `_id` when no index returns it in a stable order: Mongo
    /// returns equal sort values in any order, so offset pages could repeat or
    /// miss documents. A sort an index serves is left alone so it stays an index walk.
    async fn stable_sort(&self, object: &ObjectRef, mut sort: Document) -> Document {
        if sort.is_empty() || sort.contains_key("_id") || sort.values().any(|v| direction(v).is_none()) {
            return sort;
        }
        let indexes = catalog::list_indexes(self, object).await.unwrap_or_default();
        if !indexes.iter().any(|ix| index_serves(ix, &sort)) {
            sort.insert("_id", 1);
        }
        sort
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

    /// Collections without a sort (or sorted on `_id` alone) page in `_id` order,
    /// each page continuing after the last `_id` (keyset paging): an index seek
    /// whatever the page number. Other sorts page by offset with an `_id`
    /// tiebreaker; seeking on them is not exact, as a field's sort key and its
    /// `$gt` matches differ for arrays (the sort uses the smallest or largest
    /// element, `$gt` matches any element) and missing values.
    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let limit = request.limit();
        let object = &request.object;
        let cursor = PageCursor::parse(request.cursor.as_deref())?;
        let filter = parse_document(&request.filter, "filter")?;
        let projection = parse_document(&request.projection, "projection")?;
        let sort = parse_document(&request.sort, "sort")?;
        let next_offset = cursor.offset + u64::from(limit);
        let Some(ascending) = self.id_paging(object, &sort, &cursor).await? else {
            let sort = self.stable_sort(object, sort).await;
            let cmd = Self::find_command(&object.name, filter, projection, sort, cursor.offset, limit);
            let docs = documents(&self.client, &object.database, cmd, limit as usize + 1).await?;
            return Ok(Self::page(docs, limit, || next_offset.to_string(), started));
        };
        let after = cursor.after.and_then(|id| after_id(&id, ascending));
        let skip = if after.is_some() { 0 } else { cursor.offset };
        let filter = match after {
            None => filter,
            Some(after) if filter.is_empty() => after,
            Some(after) => doc! { "$and": [filter, after] },
        };
        let sort = doc! { "_id": if ascending { 1 } else { -1 } };
        let cmd = Self::find_command(&object.name, filter, projection, sort, skip, limit);
        let docs = documents(&self.client, &object.database, cmd, limit as usize + 1).await?;
        // A projection may leave `_id` out: the next page then continues by offset.
        let last = docs.get(limit as usize - 1).and_then(|d| d.get("_id")).cloned();
        Ok(Self::page(docs, limit, || PageCursor::token(next_offset, last.as_ref()), started))
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
    fn pages_seek_after_the_last_id() {
        let oid = ObjectId::new();
        let token = PageCursor::token(40, Some(&Bson::ObjectId(oid)));
        assert_eq!(
            PageCursor::parse(Some(&token)).unwrap(),
            PageCursor { offset: 40, seek: true, after: Some(Bson::ObjectId(oid)) }
        );
        assert_eq!(PageCursor::parse(Some(&PageCursor::token(5, None))).unwrap().after, None);
        assert_eq!(PageCursor::parse(Some("20")).unwrap(), PageCursor { offset: 20, ..PageCursor::default() });
        assert!(PageCursor::parse(Some("k!")).is_err() && PageCursor::parse(Some("x")).is_err());

        let after = after_id(&Bson::String("m".into()), true).unwrap();
        let or = after.get_array("$or").unwrap();
        assert_eq!(or[0], Bson::Document(doc! { "_id": { "$gt": "m" } }));
        let later = or[1].as_document().unwrap().get_document("_id").unwrap().get_array("$type").unwrap();
        assert_eq!(later.first(), Some(&Bson::String("object".into())));
        assert!(!later.contains(&Bson::String("string".into())));
        let before = after_id(&Bson::Int32(5), false).unwrap();
        assert_eq!(
            before.get_array("$or").unwrap()[1],
            Bson::Document(doc! { "_id": { "$type": ["minKey", "undefined", "null"] } })
        );
        assert_eq!(after_id(&Bson::MaxKey, true), None);
        assert_eq!(after_id(&Bson::Double(f64::NAN), true), None);

        assert_eq!(id_order(&doc! {}), Some(true));
        assert_eq!(id_order(&doc! { "_id": -1 }), Some(false));
        assert_eq!(id_order(&doc! { "a": 1 }), None);
        let ix = |key: Document| doc! { "name": "x", "key": key };
        assert!(index_serves(&ix(doc! { "a": 1, "b": -1 }), &doc! { "a": -1, "b": 1 }));
        assert!(index_serves(&ix(doc! { "a": 1, "b": -1 }), &doc! { "a": 1 }));
        assert!(!index_serves(&ix(doc! { "a": 1, "b": 1 }), &doc! { "a": 1, "b": -1 }));
        assert!(!index_serves(&ix(doc! { "a": "text" }), &doc! { "a": 1 }));
        assert!(!index_serves(&doc! { "key": { "a": 1 }, "sparse": true }, &doc! { "a": 1 }));
    }

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
