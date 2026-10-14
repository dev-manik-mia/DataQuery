use std::collections::BTreeMap;

use dq_core::config::{ConnConfig, Environment};
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
    /// Per SSH hop (jump hosts first): a password or key passphrase is in the keychain.
    pub ssh_secrets: Vec<bool>,
}

impl SavedConnection {
    /// Keychain id of the connection's database password.
    pub fn password_key(id: &str) -> String {
        id.to_owned()
    }

    /// Keychain id of the secret for SSH hop `hop` (jump hosts first).
    pub fn ssh_secret_key(id: &str, hop: usize) -> String {
        format!("{id}:ssh:{hop}")
    }
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
    /// One change per SSH hop, in connection order. Missing entries keep the saved secret.
    #[serde(default)]
    pub ssh_secrets: Vec<PasswordChange>,
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

/// Which model provider the opt-in AI assistant uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AiProvider {
    /// Anthropic's API with the user's own key (kept in the keychain).
    #[default]
    Anthropic,
    /// A local or self-hosted server with an OpenAI-compatible chat API, e.g. Ollama.
    OpenAiCompatible,
}

/// User preferences. Missing fields take their defaults, so older files load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct Settings {
    pub theme: Theme,
    /// UI language code from the string table, e.g. `en`.
    pub language: String,
    pub row_limit: u32,
    pub timeout_secs: u32,
    pub confirm_destructive: bool,
    pub confirm_production_writes: bool,
    pub keep_history: bool,
    pub editor_font_size: u16,
    pub editor_font_family: String,
    /// Line height in percent of the font size.
    pub editor_line_height: u16,
    pub editor_ligatures: bool,
    /// Command id → shortcut such as `mod+shift+p`; only changed bindings are stored.
    pub keymap: BTreeMap<String, String>,
    pub page_size: u32,
    /// Statements run inside a transaction that stays open until Commit or Rollback.
    pub manual_commit: bool,
    /// Hide values of columns whose names match `mask_patterns` until revealed.
    pub mask_columns: bool,
    pub mask_patterns: Vec<String>,
    /// History keeps at most this many entries (0 = no limit).
    pub history_max_entries: u32,
    /// History entries older than this many days are removed (0 = keep).
    pub history_max_days: u32,
    /// Replace string literals in recorded SQL with `'…'`.
    pub history_redact_literals: bool,
    /// Edits to a tab opened from a saved script are saved back automatically.
    pub autosave_scripts: bool,
    /// The AI assistant is off unless the user turns it on.
    pub ai_enabled: bool,
    pub ai_provider: AiProvider,
    pub ai_model: String,
    /// Base URL for the OpenAI-compatible provider, e.g. `http://localhost:11434/v1`.
    pub ai_endpoint: String,
    /// Look for a signed update when the app starts. Off by default: no network calls unless asked.
    pub check_updates_on_launch: bool,
}

pub const DEFAULT_MASK_PATTERNS: &[&str] =
    &["password", "passwd", "secret", "token", "api_key", "apikey", "ssn", "card_number", "cvv", "iban"];

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            language: "en".into(),
            row_limit: 500,
            timeout_secs: 30,
            confirm_destructive: true,
            confirm_production_writes: true,
            keep_history: true,
            editor_font_size: 13,
            editor_font_family: "JetBrains Mono".into(),
            editor_line_height: 150,
            editor_ligatures: false,
            keymap: BTreeMap::new(),
            page_size: 100,
            manual_commit: false,
            mask_columns: true,
            mask_patterns: DEFAULT_MASK_PATTERNS.iter().map(|p| (*p).to_owned()).collect(),
            history_max_entries: 20_000,
            history_max_days: 0,
            history_redact_literals: false,
            autosave_scripts: true,
            ai_enabled: false,
            ai_provider: AiProvider::Anthropic,
            ai_model: String::new(),
            ai_endpoint: "http://localhost:11434/v1".into(),
            check_updates_on_launch: false,
        }
    }
}

fn clean_text(s: &str, max: usize) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(max).collect()
}

impl Settings {
    /// Clamps values into supported ranges.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.row_limit = self.row_limit.clamp(1, dq_core::result::ExecOptions::MAX_ROW_LIMIT);
        self.timeout_secs = self.timeout_secs.min(24 * 3600);
        self.editor_font_size = self.editor_font_size.clamp(9, 28);
        self.page_size = self.page_size.clamp(10, dq_core::ops::MAX_PAGE_SIZE);
        self.editor_line_height = self.editor_line_height.clamp(100, 250);
        self.editor_font_family = clean_text(&self.editor_font_family, 100);
        if self.editor_font_family.is_empty() {
            self.editor_font_family = Self::default().editor_font_family;
        }
        self.language = clean_text(&self.language, 16);
        self.keymap = std::mem::take(&mut self.keymap)
            .into_iter()
            .filter(|(k, v)| k.len() <= 64 && v.len() <= 64 && !k.is_empty())
            .take(500)
            .collect();
        self.mask_patterns = std::mem::take(&mut self.mask_patterns)
            .iter()
            .map(|p| clean_text(p, 64).to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .take(200)
            .collect();
        self.history_max_days = self.history_max_days.min(3650);
        self.ai_model = clean_text(&self.ai_model, 100);
        self.ai_endpoint = clean_text(&self.ai_endpoint, 500);
        self
    }
}
