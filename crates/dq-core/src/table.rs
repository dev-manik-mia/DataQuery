//! Table data view: paging, filter and sort requests.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::edit::ColumnValue;
use crate::node::ObjectRef;
use crate::result::{ColumnMeta, Row};

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

/// Case-insensitive "contains" search on one column, sent as a parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ColumnSearch {
    pub column: String,
    pub contains: String,
}

/// Comparison of one column in a [`FilterCondition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    /// `value` is a comma-separated list.
    In,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    /// The operator takes no value (`IS NULL`, `IS NOT NULL`).
    pub fn unary(self) -> bool {
        matches!(self, Self::IsNull | Self::IsNotNull)
    }
}

/// One row of the visual filter builder: `column op value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FilterCondition {
    pub column: String,
    pub op: FilterOp,
    /// Ignored for unary operators.
    pub value: String,
}

/// Which rows of a table to read. Everything except `filter` travels as parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct RowFilter {
    /// A boolean SQL expression typed by the user, without the `WHERE` keyword.
    pub filter: Option<String>,
    /// Exact matches, e.g. from foreign-key navigation. `None` matches NULL.
    pub matches: Vec<ColumnValue>,
    /// Per-column searches from the grid header.
    pub searches: Vec<ColumnSearch>,
    /// Conditions from the visual filter builder.
    pub conditions: Vec<FilterCondition>,
    /// Conditions are joined with `OR` instead of `AND`.
    pub match_any: bool,
}

impl RowFilter {
    /// Nothing narrows the rows apart from (possibly) the typed `filter`.
    pub fn only_typed_filter(&self) -> bool {
        self.matches.is_empty() && self.searches.iter().all(|s| s.contains.is_empty()) && self.conditions.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TablePageRequest {
    pub object: ObjectRef,
    #[serde(flatten)]
    #[ts(flatten)]
    pub rows: RowFilter,
    pub sort: Option<SortSpec>,
    #[ts(type = "number")]
    pub offset: u64,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct TablePage {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    #[ts(type = "number")]
    pub offset: u64,
    pub has_more: bool,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct RowCount {
    #[ts(type = "number")]
    pub count: i64,
    /// False when the count is a catalog estimate.
    pub exact: bool,
}
