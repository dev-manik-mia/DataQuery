//! Explorer tree nodes.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NodeKind {
    Database,
    Schema,
    Table,
    View,
    MaterializedView,
}

impl NodeKind {
    pub fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }
}

/// Identifies a database object. For engines without schemas `schema` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ObjectRef {
    pub kind: NodeKind,
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Node {
    #[serde(flatten)]
    pub object: ObjectRef,
    pub expandable: bool,
    /// Cheap row estimate (e.g. `pg_class.reltuples`), never an exact count.
    #[ts(type = "number | null")]
    pub row_estimate: Option<i64>,
    pub comment: Option<String>,
    /// For databases: true when this is the database the session connected to.
    pub current: bool,
}

impl Node {
    pub fn leaf(object: ObjectRef) -> Self {
        Self { object, expandable: false, row_estimate: None, comment: None, current: false }
    }

    pub fn container(object: ObjectRef) -> Self {
        Self { expandable: true, ..Self::leaf(object) }
    }
}
