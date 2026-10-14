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
    Index,
    Sequence,
    Trigger,
    Function,
    Procedure,
    /// A Redis key; `database` is the database index.
    Key,
    /// A user-defined type (CQL UDT).
    Type,
    /// A folder in the tree, e.g. "Functions"; `name` is one of the `GROUP_*` ids.
    Group,
}

impl NodeKind {
    pub fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }

    /// Objects whose definition can be shown (`Session::ddl`).
    pub fn has_definition(self) -> bool {
        !matches!(self, Self::Database | Self::Schema | Self::Group | Self::Key)
    }
}

pub const GROUP_SEQUENCES: &str = "sequences";
pub const GROUP_FUNCTIONS: &str = "functions";
pub const GROUP_PROCEDURES: &str = "procedures";
pub const GROUP_TYPES: &str = "types";

/// Identifies a database object. For engines without schemas `schema` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ObjectRef {
    pub kind: NodeKind,
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
    /// The owning table of an index or trigger, or the argument list of a
    /// function or procedure (which may be overloaded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent: Option<String>,
}

impl ObjectRef {
    pub fn new(kind: NodeKind, database: impl Into<String>, schema: Option<String>, name: impl Into<String>) -> Self {
        Self { kind, database: database.into(), schema, name: name.into(), parent: None }
    }

    #[must_use]
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// The table that owns an index or trigger.
    pub fn owner_table(&self) -> Option<ObjectRef> {
        matches!(self.kind, NodeKind::Index | NodeKind::Trigger)
            .then(|| self.parent.clone())
            .flatten()
            .map(|t| Self::new(NodeKind::Table, self.database.clone(), self.schema.clone(), t))
    }
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
