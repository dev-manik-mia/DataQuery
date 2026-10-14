//! Table structure read from the catalog.

use serde::Serialize;
use ts_rs::TS;

use crate::node::ObjectRef;

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ColumnInfo {
    pub name: String,
    /// Full type as the server spells it, e.g. `numeric(12,2)`.
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    /// Identity, auto-increment or generated: skipped on insert unless given.
    pub generated: bool,
    /// The grid can edit this column through a text round-trip.
    pub editable: bool,
    pub numeric: bool,
    pub comment: Option<String>,
    /// Read through a text cast because the adapter has no native decoder for the type.
    #[serde(skip)]
    #[ts(skip)]
    pub read_as_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ConstraintKind {
    PrimaryKey,
    ForeignKey,
    Unique,
    Check,
    Exclusion,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ConstraintInfo {
    pub kind: ConstraintKind,
    pub name: String,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct IndexInfo {
    pub name: String,
    pub definition: String,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct TableStructure {
    pub object: ObjectRef,
    pub columns: Vec<ColumnInfo>,
    pub constraints: Vec<ConstraintInfo>,
    pub indexes: Vec<IndexInfo>,
    /// Columns that identify a row (primary key, else a NOT NULL unique key).
    /// Empty when rows cannot be identified, which disables editing.
    pub row_identity: Vec<String>,
    pub comment: Option<String>,
    #[ts(type = "number | null")]
    pub row_estimate: Option<i64>,
}

impl TableStructure {
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn editable(&self) -> bool {
        self.object.kind == crate::node::NodeKind::Table && !self.row_identity.is_empty()
    }
}
