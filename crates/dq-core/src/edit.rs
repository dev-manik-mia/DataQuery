//! Grid edits sent from the UI and applied in one transaction.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ColumnValue {
    pub column: String,
    /// `None` is SQL NULL; `Some("")` is an empty string.
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(export)]
pub enum RowChange {
    /// Columns that are not listed take their default.
    Insert {
        values: Vec<ColumnValue>,
    },
    /// `key` holds the row-identity columns with their original values.
    Update {
        key: Vec<ColumnValue>,
        values: Vec<ColumnValue>,
    },
    Delete {
        key: Vec<ColumnValue>,
    },
}

/// A statement with text parameters, ready for an adapter to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundStatement {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// When set, the transaction is rolled back unless exactly this many rows change.
    pub expect_rows: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, TS)]
#[ts(export)]
pub struct ApplyReport {
    pub inserted: u32,
    pub updated: u32,
    pub deleted: u32,
    #[ts(type = "number")]
    pub duration_ms: u64,
}
