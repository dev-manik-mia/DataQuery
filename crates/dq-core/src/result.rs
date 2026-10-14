//! Query results.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::value::Cell;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    /// Numeric columns are right-aligned with tabular figures.
    pub numeric: bool,
}

pub type Row = Vec<Cell>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ExecOptions {
    /// Rows to fetch before pausing. Always bounded.
    pub row_limit: u32,
    /// Server-side statement timeout; 0 disables it.
    pub timeout_secs: u32,
}

impl ExecOptions {
    pub const MAX_ROW_LIMIT: u32 = 100_000;

    #[must_use]
    pub fn clamped(self) -> Self {
        Self { row_limit: self.row_limit.clamp(1, Self::MAX_ROW_LIMIT), timeout_secs: self.timeout_secs.min(24 * 3600) }
    }
}

/// Outcome of one statement.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct StatementResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    /// More rows are available through `fetch_more`.
    pub has_more: bool,
    /// Command tag such as `SELECT`, `UPDATE`.
    pub command: String,
    #[ts(type = "number | null")]
    pub affected: Option<u64>,
    #[ts(type = "number")]
    pub duration_ms: u64,
    pub notices: Vec<String>,
}

impl StatementResult {
    pub fn returns_rows(&self) -> bool {
        !self.columns.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct RowsPage {
    pub rows: Vec<Row>,
    pub has_more: bool,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct TlsInfo {
    pub version: Option<String>,
    /// The certificate chain (and for verify-full, the host name) was verified.
    pub verified: bool,
    pub mode: crate::config::SslMode,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ServerInfo {
    pub engine: crate::engine::Engine,
    /// Product and version, e.g. `PostgreSQL 16.4`.
    pub version: String,
    #[ts(type = "number")]
    pub latency_ms: u64,
    pub tls: Option<TlsInfo>,
    pub user: String,
    pub database: String,
    pub read_only: bool,
}
