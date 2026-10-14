//! Document engines (`MongoDB`, `DynamoDB`): the Documents view of screen 13.
//!
//! Adapters convert their native values into [`DocField`] trees with the
//! engine's own type labels (`ObjectId`, `Int64`, `Decimal128` / `S`, `N`, `M`)
//! and keep every number exact as text. Edits travel in the engine's own text
//! form (`MongoDB` shell syntax, `DynamoDB` JSON), so no type is ever guessed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::adapter::BatchFn;
use crate::error::{DbError, DbResult};
use crate::node::ObjectRef;
use crate::result::{ColumnMeta, Row};
use crate::schema::ColumnInfo;
use crate::table::RowCount;
use crate::value::Cell;

/// Most children shown under one object or array; the rest are counted in `more`.
pub const MAX_CHILDREN: usize = 500;
/// Largest editable text form; bigger documents are shown but not edited.
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Documents per page in the Documents view.
pub const MAX_PAGE: u32 = 1_000;

/// One field (or array element) of a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct DocField {
    /// Field name, or the index inside an array.
    pub key: String,
    /// The engine's type name: `ObjectId`, `Int64`, `Decimal128`, `Date`, `String`, `Null`, `Object`, `Array`
    /// (`MongoDB`); `S`, `N`, `BOOL`, `NULL`, `M`, `L`, `SS`, `NS`, `B` (`DynamoDB`).
    pub type_label: String,
    /// Scalars as a cell; objects and arrays as compact JSON.
    pub value: Cell,
    pub children: Vec<DocField>,
    /// Children left out after [`MAX_CHILDREN`].
    pub more: u32,
}

impl DocField {
    pub fn scalar(key: impl Into<String>, type_label: impl Into<String>, value: Cell) -> Self {
        Self { key: key.into(), type_label: type_label.into(), value, children: Vec::new(), more: 0 }
    }

    /// An object or array. `summary` is its compact JSON; `children` are capped at [`MAX_CHILDREN`].
    pub fn container(
        key: impl Into<String>,
        type_label: impl Into<String>,
        summary: Cell,
        children: impl ExactSizeIterator<Item = DocField>,
    ) -> Self {
        let total = children.len();
        let children: Vec<DocField> = children.take(MAX_CHILDREN).collect();
        let more = (total - children.len()) as u32;
        Self { key: key.into(), type_label: type_label.into(), value: summary, children, more }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct Document {
    /// Identity in the engine's text form: `ObjectId('…')` (`MongoDB`) or the key attributes as
    /// `DynamoDB` JSON. Sent back to replace or delete the document.
    pub key: String,
    /// Short identity for headers, e.g. `_id: ObjectId('…')` or `pk: "a" · sk: 3`.
    pub label: String,
    #[ts(type = "number | null")]
    pub size_bytes: Option<u64>,
    /// Editable text form (pretty-printed). Empty with `truncated` when over [`MAX_TEXT_BYTES`].
    pub text: String,
    pub truncated: bool,
    pub fields: Vec<DocField>,
}

impl Document {
    pub fn new(key: String, label: String, size_bytes: Option<u64>, text: String, fields: Vec<DocField>) -> Self {
        let truncated = text.len() > MAX_TEXT_BYTES;
        Self { key, label, size_bytes, text: if truncated { String::new() } else { text }, truncated, fields }
    }
}

/// Which documents to read. `filter`, `projection` and `sort` are in the engine's
/// language (see [`DocumentFeatures`]); empty means none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FindRequest {
    pub object: ObjectRef,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub projection: String,
    #[serde(default)]
    pub sort: String,
    /// Where to continue: from [`DocumentPage::next`]; `None` reads the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    pub limit: u32,
}

impl FindRequest {
    pub fn new(object: ObjectRef, limit: u32) -> Self {
        Self { object, filter: String::new(), projection: String::new(), sort: String::new(), cursor: None, limit }
    }

    pub fn limit(&self) -> u32 {
        self.limit.clamp(1, MAX_PAGE)
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DocumentPage {
    pub documents: Vec<Document>,
    /// Cursor for the next page; `None` when this was the last one.
    pub next: Option<String>,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DocIndex {
    pub name: String,
    /// Key pattern, e.g. `{ status: 1, createdAt: -1 }` or `pk (HASH) · sk (RANGE)`.
    pub keys: String,
    pub unique: bool,
    /// Other options: `sparse`, `partialFilterExpression`, `ttl`, projection, status, …
    pub props: Vec<(String, String)>,
    #[ts(type = "number | null")]
    pub size_bytes: Option<i64>,
}

/// What the Documents view offers for an engine, and how its inputs are spelled.
#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct DocumentFeatures {
    /// `MQL` or `PartiQL`.
    pub language: &'static str,
    /// Example filter shown as the input placeholder.
    pub filter_example: &'static str,
    pub projection_example: &'static str,
    /// Sort input supported (`MongoDB`); `DynamoDB` returns items in key order.
    pub sort: bool,
    pub aggregation: bool,
    pub validation: bool,
    /// Edits use this text form: `shell` (`MongoDB`) or `dynamodb_json`.
    pub text_format: &'static str,
    /// Example new document for the insert editor.
    pub new_document: &'static str,
    /// Counts with a filter are possible; otherwise only the catalog estimate is shown.
    pub filtered_count: bool,
}

/// Document operations of an engine. Reads use catalog connections; writes run
/// one document at a time and fail unless exactly that document matched.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    fn features(&self) -> DocumentFeatures;

    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage>;

    /// Matching documents, or `None` when the engine cannot count with this filter.
    async fn count(&self, object: &ObjectRef, filter: &str) -> DbResult<Option<RowCount>>;

    /// Runs an aggregation pipeline (a JSON-like array) and returns up to `limit` documents.
    async fn aggregate(&self, object: &ObjectRef, pipeline: &str, limit: u32) -> DbResult<DocumentPage> {
        let _ = (object, pipeline, limit);
        Err(DbError::unsupported("Aggregation pipelines are not available for this engine."))
    }

    async fn indexes(&self, object: &ObjectRef) -> DbResult<Vec<DocIndex>>;

    /// Validation rules as text, or `None` when the collection has none.
    async fn validation(&self, object: &ObjectRef) -> DbResult<Option<String>> {
        let _ = object;
        Ok(None)
    }

    /// Inserts one document; returns its key.
    async fn insert(&self, object: &ObjectRef, text: &str) -> DbResult<String>;

    /// Replaces the document with `key`; fails when it no longer exists.
    async fn replace(&self, object: &ObjectRef, key: &str, text: &str) -> DbResult<()>;

    /// Deletes the document with `key`; fails when it no longer exists.
    async fn delete(&self, object: &ObjectRef, key: &str) -> DbResult<()>;

    /// The editor statement equivalent to `request` (for "Open in editor" and exports).
    fn find_statement(&self, request: &FindRequest) -> DbResult<String>;

    /// Inserts imported rows: each value is converted with its column's
    /// `data_type` (a type label sampled from the collection, or empty to
    /// infer). Returns how many documents were written.
    async fn insert_rows(
        &self,
        object: &ObjectRef,
        columns: &[ColumnInfo],
        rows: Vec<Vec<Option<String>>>,
    ) -> DbResult<u64>;
}

/// Grid columns and rows from documents: one column per top-level field, in
/// order of first appearance; nested values are JSON cells.
pub fn to_grid(documents: &[Vec<DocField>]) -> (Vec<ColumnMeta>, Vec<Row>) {
    let mut columns = GridColumns::default();
    for fields in documents {
        columns.observe(fields);
    }
    let rows = documents.iter().map(|fields| columns.row(fields)).collect();
    (columns.into_meta(), rows)
}

/// Column set grown from documents, for streaming exports and query results.
#[derive(Debug, Default)]
pub struct GridColumns {
    names: Vec<String>,
    types: Vec<String>,
}

impl GridColumns {
    /// Adds fields not seen before. Returns false when a field was new.
    pub fn observe(&mut self, fields: &[DocField]) -> bool {
        let mut known = true;
        for f in fields {
            if !self.names.contains(&f.key) {
                self.names.push(f.key.clone());
                self.types.push(f.type_label.clone());
                known = false;
            }
        }
        known
    }

    /// The first field that is not a column yet.
    pub fn unknown<'a>(&self, fields: &'a [DocField]) -> Option<&'a str> {
        fields.iter().find(|f| !self.names.contains(&f.key)).map(|f| f.key.as_str())
    }

    pub fn row(&self, fields: &[DocField]) -> Row {
        self.names
            .iter()
            .map(|name| fields.iter().find(|f| &f.key == name).map_or(Cell::Null, |f| f.value.clone()))
            .collect()
    }

    pub fn meta(&self) -> Vec<ColumnMeta> {
        self.names
            .iter()
            .zip(&self.types)
            .map(|(name, t)| ColumnMeta { name: name.clone(), type_name: t.clone(), numeric: is_numeric_label(t) })
            .collect()
    }

    pub fn into_meta(self) -> Vec<ColumnMeta> {
        self.meta()
    }
}

/// Numeric type labels of both engines, for right-aligned columns.
pub fn is_numeric_label(label: &str) -> bool {
    matches!(label, "Int32" | "Int64" | "Double" | "Decimal128" | "N")
}

/// Streams documents as rows. Columns come from the first batch; a later
/// document with a new top-level field stops the stream instead of silently
/// dropping that field.
pub struct DocumentRows<'a, 'b> {
    columns: Option<GridColumns>,
    seen: u64,
    on_batch: &'a mut BatchFn<'b>,
}

impl<'a, 'b> DocumentRows<'a, 'b> {
    pub fn new(on_batch: &'a mut BatchFn<'b>) -> Self {
        Self { columns: None, seen: 0, on_batch }
    }

    pub fn push(&mut self, batch: &[Vec<DocField>]) -> DbResult<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let columns = self.columns.get_or_insert_with(|| {
            let mut c = GridColumns::default();
            for fields in batch {
                c.observe(fields);
            }
            c
        });
        let mut rows = Vec::with_capacity(batch.len());
        for fields in batch {
            self.seen += 1;
            if let Some(field) = columns.unknown(fields) {
                return Err(DbError::invalid(format!(
                    "Document {} has the field `{field}`, which the first {} documents do not have. Name the fields \
                     with a projection to export every document.",
                    self.seen,
                    self.seen - 1
                )));
            }
            rows.push(columns.row(fields));
        }
        (self.on_batch)(&columns.meta(), rows)
    }

    pub fn rows(&self) -> u64 {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::CellLimits;

    fn text(key: &str, v: &str) -> DocField {
        DocField::scalar(key, "String", Cell::text(v, CellLimits::GRID))
    }

    #[test]
    fn grid_unions_fields_in_order() {
        let docs = vec![
            vec![text("_id", "1"), DocField::scalar("n", "Int64", Cell::number("9007199254740993"))],
            vec![text("_id", "2"), text("extra", "x")],
        ];
        let (cols, rows) = to_grid(&docs);
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["_id", "n", "extra"]);
        assert!(cols[1].numeric);
        assert_eq!(rows[1][1], Cell::Null);
        assert_eq!(rows[0][1], Cell::number("9007199254740993"));
    }

    #[test]
    fn stream_refuses_to_drop_fields() {
        let mut got = 0;
        let mut sink = |_: &[ColumnMeta], rows: Vec<Row>| {
            got += rows.len();
            Ok(())
        };
        let mut stream = DocumentRows::new(&mut sink);
        stream.push(&[vec![text("a", "1")]]).unwrap();
        let err = stream.push(&[vec![text("a", "2"), text("b", "3")]]).unwrap_err();
        assert!(err.message.contains("`b`"));
        assert_eq!(got, 1);
    }

    #[test]
    fn containers_are_capped() {
        let f = DocField::container(
            "a",
            "Array",
            Cell::json("[]", CellLimits::GRID),
            (0..MAX_CHILDREN + 3).map(|i| text(&i.to_string(), "x")),
        );
        assert_eq!(f.children.len(), MAX_CHILDREN);
        assert_eq!(f.more, 3);
    }
}
