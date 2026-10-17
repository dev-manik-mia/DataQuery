//! The Documents view for Neo4j: nodes of a label (or relationships of a type)
//! as documents, read-only. Filter, projection and sort are Cypher fragments on
//! `n`; each must be one expression without clauses, and reads run in READ mode.

use std::fmt::Write as _;
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dq_core::document::{DocField, DocIndex, Document, DocumentFeatures, DocumentPage, DocumentStore, FindRequest};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::schema::ColumnInfo;
use dq_core::sql::SqlFlavor;
use dq_core::sql::lexer::{TokenKind, tokenize};
use dq_core::table::RowCount;
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use serde_json::{Map, Value, json};

use crate::catalog::{self, quote};
use crate::client::Query;
use crate::session::Neo4jSession;
use crate::values::{self, entity_fields, int};

pub const FEATURES: DocumentFeatures = DocumentFeatures {
    language: "Cypher",
    filter_example: "n.age > 30 AND n.name STARTS WITH 'A'",
    projection_example: "n.name, n.age",
    sort: true,
    aggregation: false,
    validation: false,
    text_format: "json",
    new_document: "{\n}",
    filtered_count: true,
};

const IMPORT_BATCH: usize = 500;

/// Clause keywords a fragment may not contain (outside property access like `n.set`).
const CLAUSES: &[&str] = &[
    "CREATE",
    "MERGE",
    "SET",
    "DELETE",
    "DETACH",
    "REMOVE",
    "CALL",
    "LOAD",
    "FOREACH",
    "UNION",
    "RETURN",
    "WITH",
    "MATCH",
    "UNWIND",
    "USE",
    "YIELD",
    "SHOW",
    "TERMINATE",
    "DROP",
    "OPTIONAL",
    "FINISH",
    "SKIP",
    "LIMIT",
    "ORDER",
];

/// Checks that `fragment` is one expression: no `;`, comments or clauses.
pub fn validate_fragment(fragment: &str, what: &str) -> DbResult<()> {
    let tokens = tokenize(fragment, SqlFlavor::Cypher);
    let significant: Vec<_> = tokens.iter().filter(|t| t.kind != TokenKind::Whitespace).collect();
    for (i, t) in significant.iter().enumerate() {
        let bad = match t.kind {
            TokenKind::Semicolon | TokenKind::Comment => true,
            TokenKind::Word => {
                let after_dot = i > 0 && significant[i - 1].text(fragment) == ".";
                let map_key = significant.get(i + 1).is_some_and(|n| n.text(fragment) == ":");
                !after_dot && !map_key && CLAUSES.iter().any(|c| t.text(fragment).eq_ignore_ascii_case(c))
            }
            _ => false,
        };
        if bad {
            return Err(DbError::invalid(format!(
                "The {what} must be a single expression such as {}, without clauses or `;`.",
                match what {
                    "filter" => FEATURES.filter_example,
                    "projection" => FEATURES.projection_example,
                    _ => "n.name DESC",
                }
            )));
        }
    }
    Ok(())
}

/// Parts of `text` separated by commas outside brackets, trimmed and non-empty.
fn split_top_level(text: &str) -> Vec<&str> {
    let tokens = tokenize(text, SqlFlavor::Cypher);
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for t in &tokens {
        match (t.kind, t.text(text)) {
            (TokenKind::OpenParen, _) | (TokenKind::Punct, "[" | "{") => depth += 1,
            (TokenKind::CloseParen, _) | (TokenKind::Punct, "]" | "}") => depth -= 1,
            (TokenKind::Punct, ",") if depth == 0 => {
                parts.push(text[start..t.start].trim());
                start = t.end;
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts.retain(|p| !p.is_empty());
    parts
}

/// `n.name, n.age` → `{`name`: n.name, `age`: n.age}`.
pub fn projection_map(projection: &str) -> String {
    let entries: Vec<String> = split_top_level(projection)
        .into_iter()
        .map(|expr| {
            let key = expr
                .rsplit_once('.')
                .filter(|(head, tail)| {
                    !head.is_empty()
                        && head.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && tail.chars().all(|c| c.is_alphanumeric() || c == '_')
                })
                .map_or(expr, |(_, tail)| tail);
            format!("{}: {expr}", quote(key))
        })
        .collect();
    format!("{{{}}}", entries.join(", "))
}

/// One `ORDER BY` term of the sort input.
#[derive(Debug, PartialEq, Eq)]
struct SortTerm<'a> {
    expr: &'a str,
    descending: bool,
}

/// `n.name DESC, n.age` → its terms. The sort was validated as a fragment.
fn sort_terms(sort: &str) -> Vec<SortTerm<'_>> {
    split_top_level(sort)
        .into_iter()
        .map(|term| {
            let tokens = tokenize(term, SqlFlavor::Cypher);
            let last = tokens.iter().rev().find(|t| t.kind != TokenKind::Whitespace);
            match last.filter(|t| t.kind == TokenKind::Word).map(|t| (t.start, t.text(term).to_ascii_uppercase())) {
                Some((start, word)) if matches!(word.as_str(), "ASC" | "ASCENDING" | "DESC" | "DESCENDING") => {
                    SortTerm { expr: term[..start].trim(), descending: word.starts_with("DESC") }
                }
                _ => SortTerm { expr: term, descending: false },
            }
        })
        .collect()
}

/// Type classes in `ORDER BY` order from zoned datetimes up (maps, entities, lists,
/// paths and points come before; `null` after). A seek compares within a class
/// with `<` / `>` and across classes with type predicates, as `ORDER BY` does.
const TYPE_CLASSES: &[&[&str]] = &[
    &["ZONED DATETIME"],
    &["LOCAL DATETIME"],
    &["DATE"],
    &["ZONED TIME"],
    &["LOCAL TIME"],
    &["DURATION"],
    &["STRING"],
    &["BOOLEAN"],
    &["INTEGER", "FLOAT"],
];
const NUMBER_CLASS: usize = 8;

/// A sort value a page may continue after.
enum SeekValue {
    Null,
    /// An index into [`TYPE_CLASSES`].
    Class(usize),
}

/// The class of a sort value, or `None` when seeking on it is not exact
/// (temporal values lose precision in the typed JSON, NaN and other types do
/// not compare with `<`).
fn seek_class(v: &Value) -> Option<SeekValue> {
    let finite = |v: &Value| {
        v.get("_value").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()).is_some_and(f64::is_finite)
    };
    match values::type_of(v) {
        "Null" => Some(SeekValue::Null),
        "String" => Some(SeekValue::Class(6)),
        "Boolean" => Some(SeekValue::Class(7)),
        "Integer" => Some(SeekValue::Class(NUMBER_CLASS)),
        "Float" if finite(v) => Some(SeekValue::Class(NUMBER_CLASS)),
        _ => None,
    }
}

/// `e` is not null and of a class in `classes`.
fn of_classes(e: &str, classes: &[&[&str]]) -> String {
    let tests: Vec<String> = classes.iter().flat_map(|c| c.iter()).map(|t| format!("{e} IS :: {t} NOT NULL")).collect();
    if tests.is_empty() { "false".to_owned() } else { tests.join(" OR ") }
}

/// The rows after the last one of a page in `ORDER BY <terms>, elementId(n)` order,
/// with its sort values bound as `$seek_<i>` and its id as `$seek_id`. `None` when a
/// value cannot be sought exactly; the page then continues with `SKIP`.
fn seek_predicate(terms: &[SortTerm<'_>], keys: &[Value], params: &mut Map<String, Value>) -> Option<String> {
    if terms.len() != keys.len() {
        return None;
    }
    let mut rest = "elementId(n) > $seek_id".to_owned();
    for (i, (term, key)) in terms.iter().zip(keys).enumerate().rev() {
        let e = format!("({})", term.expr);
        let (after, equal) = match (seek_class(key)?, term.descending) {
            // Nulls sort last ascending, first descending.
            (SeekValue::Null, false) => (None, format!("{e} IS NULL")),
            (SeekValue::Null, true) => (Some(format!("{e} IS NOT NULL")), format!("{e} IS NULL")),
            (SeekValue::Class(class), descending) => {
                let p = format!("$seek_{i}");
                params.insert(format!("seek_{i}"), key.clone());
                let after = if descending {
                    format!("{e} < {p} OR ({e} IS NOT NULL AND NOT ({}))", of_classes(&e, &TYPE_CLASSES[class..]))
                } else {
                    // NaN sorts after every other number.
                    let nan = if class == NUMBER_CLASS { format!(" OR {e} <> {e}") } else { String::new() };
                    format!("{e} > {p}{nan} OR {} OR {e} IS NULL", of_classes(&e, &TYPE_CLASSES[class + 1..]))
                };
                (Some(after), format!("{e} = {p}"))
            }
        };
        rest = match after {
            Some(after) => format!("({after}) OR ({equal} AND ({rest}))"),
            None => format!("{equal} AND ({rest})"),
        };
    }
    Some(rest)
}

/// Where a page starts. Plain numbers are offsets (pages without a sort, and
/// older cursors); `k…` continues a sorted read after its last row, keeping the
/// offset for when seeking is not possible.
#[derive(Debug, Default, PartialEq)]
struct PageCursor {
    offset: u64,
    /// The last row's elementId and sort values.
    after: Option<(String, Vec<Value>)>,
}

impl PageCursor {
    fn parse(cursor: Option<&str>) -> DbResult<Self> {
        let invalid = || DbError::invalid("Invalid cursor.");
        let Some(c) = cursor else { return Ok(Self::default()) };
        if let Some(token) = c.strip_prefix('k') {
            let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
            let offset = v.get("o").and_then(Value::as_u64).ok_or_else(invalid)?;
            let after = match (v.get("id").and_then(Value::as_str), v.get("k").and_then(Value::as_array)) {
                (Some(id), Some(keys)) => Some((id.to_owned(), keys.clone())),
                _ => None,
            };
            return Ok(Self { offset, after });
        }
        Ok(Self { offset: c.parse().map_err(|_| invalid())?, after: None })
    }

    fn token(offset: u64, id: &str, keys: &[Value]) -> String {
        let body = json!({ "o": offset, "id": id, "k": keys });
        format!("k{}", URL_SAFE_NO_PAD.encode(body.to_string()))
    }
}

/// The pattern and the value returned for a label (nodes) or relationship type.
fn pattern(object: &ObjectRef) -> DbResult<String> {
    match object.kind {
        NodeKind::Table => Ok(format!("MATCH (n:{})", quote(&object.name))),
        NodeKind::View => Ok(format!("MATCH (a)-[n:{}]->(b)", quote(&object.name))),
        _ => Err(DbError::invalid("Choose a label or relationship type.")),
    }
}

fn where_clause(filter: &str) -> DbResult<String> {
    let filter = filter.trim();
    if filter.is_empty() {
        return Ok(String::new());
    }
    validate_fragment(filter, "filter")?;
    Ok(format!(" WHERE {filter}"))
}

/// The read for a page, with `SKIP`/`LIMIT` as given (parameters or literals).
/// A sort gets `elementId(n)` as the last term so pages split ties the same way
/// every time. `seek` (a predicate from [`seek_predicate`]) starts after a row;
/// `keys` also returns the sort values as `sort_key` for the next page's cursor.
pub fn find_query(request: &FindRequest, skip: &str, limit: &str) -> DbResult<String> {
    page_query(request, skip, limit, None, false)
}

fn page_query(request: &FindRequest, skip: &str, limit: &str, seek: Option<&str>, keys: bool) -> DbResult<String> {
    let mut q = pattern(&request.object)?;
    let filter = where_clause(&request.filter)?;
    match (filter.is_empty(), seek) {
        (_, None) => q.push_str(&filter),
        (true, Some(seek)) => {
            let _ = write!(q, " WHERE {seek}");
        }
        (false, Some(seek)) => {
            let _ = write!(q, " WHERE ({}) AND ({seek})", request.filter.trim());
        }
    }
    let projection = request.projection.trim();
    let doc = if projection.is_empty() {
        "n".to_owned()
    } else {
        validate_fragment(projection, "projection")?;
        projection_map(projection)
    };
    let _ = write!(q, " RETURN elementId(n) AS id, {doc} AS doc");
    let sort = request.sort.trim();
    if !sort.is_empty() {
        validate_fragment(sort, "sort")?;
        if keys {
            let exprs: Vec<&str> = sort_terms(sort).iter().map(|t| t.expr).collect();
            let _ = write!(q, ", [{}] AS sort_key", exprs.join(", "));
        }
        let _ = write!(q, " ORDER BY {sort}, elementId(n)");
    }
    let _ = write!(q, " SKIP {skip} LIMIT {limit}");
    Ok(q)
}

fn short_id(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

fn document(object: &ObjectRef, id: &str, doc: &Value) -> Document {
    let limits = CellLimits::GRID;
    let label = if object.kind == NodeKind::View {
        format!("[:{}] {}", object.name, short_id(id))
    } else {
        format!("(:{}) {}", object.name, short_id(id))
    };
    let fields: Vec<DocField> = match values::entity(doc) {
        Some(entity) => {
            let mut f = Vec::new();
            for (key, field) in [("start", "_start_node_element_id"), ("end", "_end_node_element_id")] {
                if let Some(v) = entity.get(field).and_then(Value::as_str) {
                    f.push(DocField::scalar(key, "String", dq_core::value::Cell::text(v, limits)));
                }
            }
            f.extend(entity_fields(entity, limits));
            f
        }
        None => doc
            .get("_value")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| values::field(k, v, limits)).collect())
            .unwrap_or_default(),
    };
    let text = serde_json::to_string_pretty(&values::plain(doc)).unwrap_or_default();
    Document::new(id.to_owned(), label, None, text, fields)
}

fn read_only() -> DbError {
    DbError::unsupported("Change the graph with Cypher in the editor.")
}

#[async_trait]
impl DocumentStore for Neo4jSession {
    fn features(&self) -> DocumentFeatures {
        FEATURES
    }

    /// Sorted reads continue after the last row (keyset paging): the sort plus
    /// `elementId(n)` is a total order, so each page costs a scan with a top-`limit`
    /// sort instead of sorting `offset + limit` rows. Without a sort pages use
    /// `SKIP` over the label scan: ordering by `elementId(n)` cannot use the scan's
    /// order and would sort every node on every page.
    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let cursor = PageCursor::parse(request.cursor.as_deref())?;
        let limit = request.limit();
        let sorted = !request.sort.trim().is_empty();
        let terms = sort_terms(request.sort.trim());
        let mut params = Map::new();
        let seek = match &cursor.after {
            Some((id, keys)) if sorted => seek_predicate(&terms, keys, &mut params).inspect(|_| {
                params.insert("seek_id".into(), json!({ "$type": "String", "_value": id }));
            }),
            _ => None,
        };
        let skip = if seek.is_some() { 0 } else { cursor.offset };
        let statement = page_query(request, "$skip", "$limit", seek.as_deref(), sorted)?;
        params.insert("skip".into(), json!({ "$type": "Integer", "_value": skip.to_string() }));
        params.insert("limit".into(), json!({ "$type": "Integer", "_value": (u64::from(limit) + 1).to_string() }));
        let reply = self.read(&request.object.database, &statement, params).await?;
        let more = reply.values.len() > limit as usize;
        let rows = &reply.values[..reply.values.len().min(limit as usize)];
        let ids: Vec<String> = rows.iter().map(|r| r.first().and_then(values::text).unwrap_or_default()).collect();
        let documents: Vec<Document> = rows
            .iter()
            .zip(&ids)
            .map(|(r, id)| document(&request.object, id, r.get(1).unwrap_or(&Value::Null)))
            .collect();
        let offset = cursor.offset + u64::from(limit);
        let next = more.then(|| match (sorted, rows.last(), ids.last()) {
            (true, Some(row), Some(id)) => {
                let keys =
                    row.get(2).and_then(|k| k.get("_value")).and_then(Value::as_array).cloned().unwrap_or_default();
                PageCursor::token(offset, id, &keys)
            }
            _ => offset.to_string(),
        });
        Ok(DocumentPage { next, documents, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn count(&self, object: &ObjectRef, filter: &str) -> DbResult<Option<RowCount>> {
        if filter.trim().is_empty() {
            return Ok(catalog::count(&self.client, object).await?.map(|count| RowCount { count, exact: true }));
        }
        let statement = format!("{}{} RETURN count(n)", pattern(object)?, where_clause(filter)?);
        let reply = self.read(&object.database, &statement, Map::new()).await?;
        Ok(reply.values.first().and_then(|r| r.first()).and_then(int).map(|count| RowCount { count, exact: true }))
    }

    async fn indexes(&self, object: &ObjectRef) -> DbResult<Vec<DocIndex>> {
        Ok(catalog::items_for(&self.client, object)
            .await?
            .into_iter()
            .map(|i| {
                let mut props = vec![("type".to_owned(), i.kind.clone())];
                if let Some(state) = &i.state {
                    props.push(("state".to_owned(), state.clone()));
                }
                if i.constraint {
                    props.push(("constraint".to_owned(), "yes".to_owned()));
                }
                DocIndex {
                    unique: i.kind.contains("UNIQUENESS") || i.owning_constraint.is_some(),
                    keys: format!("({})", i.properties.join(", ")),
                    name: i.name,
                    props,
                    size_bytes: None,
                }
            })
            .collect())
    }

    async fn insert(&self, _object: &ObjectRef, _text: &str) -> DbResult<String> {
        Err(read_only())
    }

    async fn replace(&self, _object: &ObjectRef, _key: &str, _text: &str) -> DbResult<()> {
        Err(read_only())
    }

    async fn delete(&self, _object: &ObjectRef, _key: &str) -> DbResult<()> {
        Err(read_only())
    }

    fn find_statement(&self, request: &FindRequest) -> DbResult<String> {
        let skip = PageCursor::parse(request.cursor.as_deref())?.offset.to_string();
        Ok(find_query(request, &skip, &request.limit().to_string())? + ";")
    }

    /// Creates one node per row with the label, in batches of 500 (one transaction each).
    async fn insert_rows(
        &self,
        object: &ObjectRef,
        columns: &[ColumnInfo],
        rows: Vec<Vec<Option<String>>>,
    ) -> DbResult<u64> {
        if object.kind != NodeKind::Table {
            return Err(DbError::unsupported("Rows can be imported as nodes of a label only."));
        }
        if self.read_only {
            return Err(DbError::new(dq_core::ErrorKind::Permission, "This connection is read-only."));
        }
        let statement = format!("UNWIND $rows AS row CREATE (n:{}) SET n = row", quote(&object.name));
        let mut written = 0u64;
        for batch in rows.chunks(IMPORT_BATCH) {
            let mut maps = Vec::with_capacity(batch.len());
            for row in batch {
                let mut map = Map::new();
                for (c, v) in columns.iter().zip(row) {
                    let value = values::import_value(&c.data_type, v.as_deref())
                        .map_err(|m| DbError::invalid(format!("{} {m}", c.name)))?;
                    map.insert(c.name.clone(), value);
                }
                maps.push(json!({ "$type": "Map", "_value": map }));
            }
            let query =
                Query::write(&object.database, &statement).with("rows", json!({ "$type": "List", "_value": maps }));
            let reply = self.client.run(&query, false).await?;
            let created = reply.counters.as_ref().and_then(|c| c.get("nodesCreated")).and_then(Value::as_u64);
            written += created.unwrap_or(batch.len() as u64);
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(filter: &str, projection: &str, sort: &str) -> FindRequest {
        FindRequest {
            object: ObjectRef::new(NodeKind::Table, "neo4j", None, "Person"),
            filter: filter.into(),
            projection: projection.into(),
            sort: sort.into(),
            cursor: None,
            limit: 20,
        }
    }

    #[test]
    fn builds_find_queries() {
        let q = find_query(&request("n.age > 30", "n.name, n.age", "n.name DESC"), "$skip", "$limit").unwrap();
        assert_eq!(
            q,
            "MATCH (n:Person) WHERE n.age > 30 RETURN elementId(n) AS id, {name: n.name, age: n.age} AS doc ORDER BY n.name DESC, elementId(n) SKIP $skip LIMIT $limit"
        );
        let rel =
            FindRequest { object: ObjectRef::new(NodeKind::View, "neo4j", None, "ACTED IN"), ..request("", "", "") };
        assert!(find_query(&rel, "0", "20").unwrap().starts_with("MATCH (a)-[n:`ACTED IN`]->(b) RETURN"));
        assert_eq!(
            projection_map("size(n.tags), n.`full name`"),
            "{`size(n.tags)`: size(n.tags), `n.``full name```: n.`full name`}"
        );
    }

    #[test]
    fn sorted_pages_seek_after_the_last_row() {
        let terms = sort_terms("n.name DESC, size(n.tags), n.age ascending");
        assert_eq!(
            terms,
            [
                SortTerm { expr: "n.name", descending: true },
                SortTerm { expr: "size(n.tags)", descending: false },
                SortTerm { expr: "n.age", descending: false },
            ]
        );
        let mut params = Map::new();
        let keys = [json!({ "$type": "String", "_value": "b" }), json!({ "$type": "Null", "_value": null })];
        let seek = seek_predicate(&sort_terms("n.name DESC, n.age"), &keys, &mut params).unwrap();
        assert_eq!(
            seek,
            "((n.name) < $seek_0 OR ((n.name) IS NOT NULL AND NOT ((n.name) IS :: STRING NOT NULL OR (n.name) IS :: \
             BOOLEAN NOT NULL OR (n.name) IS :: INTEGER NOT NULL OR (n.name) IS :: FLOAT NOT NULL))) OR ((n.name) = \
             $seek_0 AND ((n.age) IS NULL AND (elementId(n) > $seek_id)))"
        );
        assert_eq!(params.keys().collect::<Vec<_>>(), ["seek_0"]);
        let nan = [json!({ "$type": "Float", "_value": "NaN" })];
        assert!(seek_predicate(&sort_terms("n.x"), &nan, &mut params).is_none());
        let token = PageCursor::token(40, "4:x:7", &keys);
        let cursor = PageCursor::parse(Some(&token)).unwrap();
        assert_eq!((cursor.offset, cursor.after.unwrap().0.as_str()), (40, "4:x:7"));
        assert_eq!(PageCursor::parse(Some("20")).unwrap(), PageCursor { offset: 20, after: None });
        assert!(PageCursor::parse(Some("kxyz")).is_err());
        let mut req = request("n.age > 1", "", "n.age");
        req.cursor = Some(token);
        let q = page_query(&req, "0", "21", Some("SEEK"), true).unwrap();
        assert!(q.contains("WHERE (n.age > 1) AND (SEEK) RETURN elementId(n) AS id, n AS doc, [n.age] AS sort_key"));
    }

    #[test]
    fn rejects_clauses_in_fragments() {
        assert!(find_query(&request("RETURN 1; MATCH (n) DELETE n", "", ""), "0", "1").is_err());
        assert!(find_query(&request("n.x = 1 SET n.y = 2", "", ""), "0", "1").is_err());
        assert!(find_query(&request("true // x", "", ""), "0", "1").is_err());
        assert!(find_query(&request("", "n.name CALL { MATCH (m) DETACH DELETE m }", ""), "0", "1").is_err());
        assert!(find_query(&request("n.set = 1 AND n.status IN ['a', 'b']", "", ""), "0", "1").is_ok());
        assert!(find_query(&request("n.p = {match: 1}", "", ""), "0", "1").is_ok());
    }
}
