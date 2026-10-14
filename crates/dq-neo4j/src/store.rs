//! The Documents view for Neo4j: nodes of a label (or relationships of a type)
//! as documents, read-only. Filter, projection and sort are Cypher fragments on
//! `n`; each must be one expression without clauses, and reads run in READ mode.

use std::fmt::Write as _;
use std::time::Instant;

use async_trait::async_trait;
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

/// `n.name, n.age` → `{`name`: n.name, `age`: n.age}`.
pub fn projection_map(projection: &str) -> String {
    let tokens = tokenize(projection, SqlFlavor::Cypher);
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for t in &tokens {
        match (t.kind, t.text(projection)) {
            (TokenKind::OpenParen, _) | (TokenKind::Punct, "[" | "{") => depth += 1,
            (TokenKind::CloseParen, _) | (TokenKind::Punct, "]" | "}") => depth -= 1,
            (TokenKind::Punct, ",") if depth == 0 => {
                parts.push(projection[start..t.start].trim());
                start = t.end;
            }
            _ => {}
        }
    }
    parts.push(projection[start..].trim());
    let entries: Vec<String> = parts
        .into_iter()
        .filter(|p| !p.is_empty())
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
pub fn find_query(request: &FindRequest, skip: &str, limit: &str) -> DbResult<String> {
    let mut q = pattern(&request.object)?;
    q.push_str(&where_clause(&request.filter)?);
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
        let _ = write!(q, " ORDER BY {sort}");
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

    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let skip: u64 =
            request.cursor.as_deref().map_or(Ok(0), str::parse).map_err(|_| DbError::invalid("Invalid cursor."))?;
        let limit = request.limit();
        let statement = find_query(request, "$skip", "$limit")?;
        let mut params = Map::new();
        params.insert("skip".into(), json!({ "$type": "Integer", "_value": skip.to_string() }));
        params.insert("limit".into(), json!({ "$type": "Integer", "_value": (u64::from(limit) + 1).to_string() }));
        let reply = self.read(&request.object.database, &statement, params).await?;
        let mut documents: Vec<Document> = reply
            .values
            .iter()
            .map(|r| {
                let id = r.first().and_then(values::text).unwrap_or_default();
                document(&request.object, &id, r.get(1).unwrap_or(&Value::Null))
            })
            .collect();
        let more = documents.len() > limit as usize;
        documents.truncate(limit as usize);
        Ok(DocumentPage {
            next: more.then(|| (skip + u64::from(limit)).to_string()),
            documents,
            duration_ms: started.elapsed().as_millis() as u64,
        })
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
        let skip = request.cursor.as_deref().unwrap_or("0");
        if skip.parse::<u64>().is_err() {
            return Err(DbError::invalid("Invalid cursor."));
        }
        Ok(find_query(request, skip, &request.limit().to_string())? + ";")
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
            "MATCH (n:Person) WHERE n.age > 30 RETURN elementId(n) AS id, {name: n.name, age: n.age} AS doc ORDER BY n.name DESC SKIP $skip LIMIT $limit"
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
    fn rejects_clauses_in_fragments() {
        assert!(find_query(&request("RETURN 1; MATCH (n) DELETE n", "", ""), "0", "1").is_err());
        assert!(find_query(&request("n.x = 1 SET n.y = 2", "", ""), "0", "1").is_err());
        assert!(find_query(&request("true // x", "", ""), "0", "1").is_err());
        assert!(find_query(&request("", "n.name CALL { MATCH (m) DETACH DELETE m }", ""), "0", "1").is_err());
        assert!(find_query(&request("n.set = 1 AND n.status IN ['a', 'b']", "", ""), "0", "1").is_ok());
        assert!(find_query(&request("n.p = {match: 1}", "", ""), "0", "1").is_ok());
    }
}
