//! Table data view: paging, filter and sort requests.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::node::ObjectRef;
use crate::result::{ColumnMeta, Row};

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TablePageRequest {
    pub object: ObjectRef,
    /// A boolean SQL expression typed by the user, without the `WHERE` keyword.
    pub filter: Option<String>,
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
