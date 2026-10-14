use rb_core::config::{ConnConfig, Environment};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Folder {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    pub folder_id: Option<String>,
    pub environment: Environment,
    pub favorite: bool,
    pub config: ConnConfig,
    /// A password for this connection is stored in the keychain.
    pub has_password: bool,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number | null")]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(export)]
pub enum PasswordChange {
    Keep,
    Set { value: String },
    Clear,
}

impl std::fmt::Display for PasswordChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "keep",
            Self::Set { .. } => "set",
            Self::Clear => "clear",
        })
    }
}

/// Create or update request from the connection form.
#[derive(Clone, Deserialize, TS)]
#[ts(export)]
pub struct ConnectionDraft {
    pub id: Option<String>,
    pub name: String,
    pub folder_id: Option<String>,
    pub environment: Environment,
    pub favorite: bool,
    pub config: ConnConfig,
    pub password: PasswordChange,
}

impl std::fmt::Debug for ConnectionDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionDraft")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("password", &self.password.to_string())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum HistoryStatus {
    Ok,
    Error,
    Cancelled,
}

impl HistoryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "error" => Self::Error,
            "cancelled" => Self::Cancelled,
            _ => Self::Ok,
        }
    }
}

/// One executed statement. Result rows are never stored.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HistoryEntry {
    #[ts(type = "number")]
    pub id: i64,
    pub connection_id: Option<String>,
    pub connection_name: String,
    pub database: String,
    pub sql: String,
    #[ts(type = "number")]
    pub executed_at: i64,
    #[ts(type = "number")]
    pub duration_ms: i64,
    pub status: HistoryStatus,
    #[ts(type = "number | null")]
    pub rows: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewHistoryEntry {
    pub connection_id: Option<String>,
    pub connection_name: String,
    pub database: String,
    pub sql: String,
    pub duration_ms: i64,
    pub status: HistoryStatus,
    pub rows: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, TS)]
#[ts(export)]
pub struct HistoryQuery {
    pub search: Option<String>,
    pub connection_id: Option<String>,
    /// Return entries older than this id (pagination).
    #[ts(type = "number | null")]
    pub before_id: Option<i64>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    #[ts(type = "number")]
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct SavedScript {
    pub id: String,
    pub name: String,
    pub sql: String,
    pub connection_id: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct ScriptDraft {
    pub id: Option<String>,
    pub name: String,
    pub sql: String,
    pub connection_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

/// User preferences. Missing fields take their defaults, so older files load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct Settings {
    pub theme: Theme,
    pub row_limit: u32,
    pub timeout_secs: u32,
    pub confirm_destructive: bool,
    pub confirm_production_writes: bool,
    pub keep_history: bool,
    pub editor_font_size: u16,
    pub page_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            row_limit: 500,
            timeout_secs: 30,
            confirm_destructive: true,
            confirm_production_writes: true,
            keep_history: true,
            editor_font_size: 13,
            page_size: 100,
        }
    }
}

impl Settings {
    /// Clamps values into supported ranges.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.row_limit = self.row_limit.clamp(1, rb_core::result::ExecOptions::MAX_ROW_LIMIT);
        self.timeout_secs = self.timeout_secs.min(24 * 3600);
        self.editor_font_size = self.editor_font_size.clamp(9, 28);
        self.page_size = self.page_size.clamp(10, rb_core::ops::MAX_PAGE_SIZE);
        self
    }
}
