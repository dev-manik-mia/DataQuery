//! Table structure read from the catalog.

use serde::{Deserialize, Serialize};
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
    /// Identity (PostgreSQL) or `AUTO_INCREMENT` (MySQL).
    pub identity: bool,
    /// The grid can edit this column through a text round-trip.
    pub editable: bool,
    pub numeric: bool,
    pub comment: Option<String>,
    /// Read through a text cast because the adapter has no native decoder for the type.
    #[serde(skip)]
    #[ts(skip)]
    pub read_as_text: bool,
}

/// How a column takes part in a wide-column primary key (CQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum KeyRoleKind {
    /// Part of the partition key (`PK`).
    Partition,
    /// A clustering column (`CK ↑` / `CK ↓`).
    Clustering,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct KeyRole {
    pub column: String,
    pub role: KeyRoleKind,
    /// Clustering order is descending.
    pub descending: bool,
}

impl ColumnInfo {
    /// A document field the collection does not have yet: any type, inferred from the value.
    pub fn new_field(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            data_type: String::new(),
            nullable: true,
            default: None,
            primary_key: false,
            generated: false,
            identity: false,
            editable: true,
            numeric: false,
            comment: None,
            read_as_text: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
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
    /// Indexed columns in order; empty for expression-only indexes.
    pub columns: Vec<String>,
    #[ts(type = "number | null")]
    pub size_bytes: Option<i64>,
}

/// A foreign key, with its columns paired in order with the target's columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct ForeignKey {
    pub name: String,
    /// The table that holds the key.
    pub source: ObjectRef,
    pub columns: Vec<String>,
    pub target: ObjectRef,
    pub target_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, TS)]
#[ts(export)]
pub struct TableSize {
    /// Table, indexes and TOAST/overflow together.
    #[ts(type = "number")]
    pub total_bytes: i64,
    #[ts(type = "number")]
    pub table_bytes: i64,
    #[ts(type = "number")]
    pub index_bytes: i64,
}

/// One edge in the dependency view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct Dependency {
    pub object: ObjectRef,
    /// How the two objects are related, e.g. `foreign key orders_customer_fk`, `view`, `trigger`.
    pub relation: String,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct Dependencies {
    /// Objects this one depends on.
    pub uses: Vec<Dependency>,
    /// Objects that depend on this one.
    pub used_by: Vec<Dependency>,
}

/// Columns of one table, for autocomplete, the ER diagram and the AI schema summary.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ModelTable {
    pub object: ObjectRef,
    pub columns: Vec<ModelColumn>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ModelColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

/// Tables, columns and foreign keys of a database (or one schema), bounded.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct SchemaModel {
    pub tables: Vec<ModelTable>,
    pub foreign_keys: Vec<ForeignKey>,
    /// More tables exist than were returned.
    pub truncated: bool,
}

impl SchemaModel {
    pub const MAX_TABLES: usize = 2_000;
    pub const MAX_COLUMNS: usize = 40_000;
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
    /// For tables without a key: every column that can be compared, used to
    /// match a row only after the user confirms. Each change must still match
    /// exactly one row, or nothing is saved. Filled by [`crate::ops::structure`].
    pub full_row_key: Vec<String>,
    pub comment: Option<String>,
    #[ts(type = "number | null")]
    pub row_estimate: Option<i64>,
    pub foreign_keys: Vec<ForeignKey>,
    pub size: Option<TableSize>,
}

impl TableStructure {
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn editable(&self) -> bool {
        self.object.kind == crate::node::NodeKind::Table && !self.row_identity.is_empty()
    }
}
