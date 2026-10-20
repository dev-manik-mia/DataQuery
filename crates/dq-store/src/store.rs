//! SQLite-backed metadata store. All methods are synchronous; callers run them
//! on a blocking thread.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use dq_core::config::{ConnConfig, Environment};
use dq_core::redact::redact;
use dq_core::{DbError, DbResult, ErrorKind};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    ConnectionDraft, ConnectionImport, ConnectionImportReport, Folder, HistoryEntry, HistoryPage, HistoryQuery,
    HistoryStatus, NewHistoryEntry, PasswordChange, Retention, SavedConnection, SavedScript, ScriptDraft, Settings,
    Snippet, SnippetDraft,
};

const MIGRATIONS: &[&str] = &[
    r"
    CREATE TABLE folders (
        id       TEXT PRIMARY KEY,
        name     TEXT NOT NULL,
        position INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE connections (
        id           TEXT PRIMARY KEY,
        name         TEXT NOT NULL,
        folder_id    TEXT REFERENCES folders(id) ON DELETE SET NULL,
        environment  TEXT NOT NULL,
        favorite     INTEGER NOT NULL DEFAULT 0,
        config       TEXT NOT NULL,
        has_password INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        last_used_at INTEGER
    );
    CREATE TABLE history (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id   TEXT,
        connection_name TEXT NOT NULL,
        database        TEXT NOT NULL,
        sql             TEXT NOT NULL,
        executed_at     INTEGER NOT NULL,
        duration_ms     INTEGER NOT NULL,
        status          TEXT NOT NULL,
        rows            INTEGER,
        error           TEXT
    );
    CREATE INDEX history_executed_at ON history (executed_at DESC);
    CREATE TABLE scripts (
        id            TEXT PRIMARY KEY,
        name          TEXT NOT NULL,
        sql           TEXT NOT NULL,
        connection_id TEXT,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    );
    CREATE TABLE kv (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
",
    r"
    ALTER TABLE connections ADD COLUMN ssh_secrets TEXT NOT NULL DEFAULT '[]';
",
    r"
    ALTER TABLE scripts ADD COLUMN folder TEXT;
    ALTER TABLE scripts ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE scripts ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE snippets (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        prefix     TEXT NOT NULL,
        body       TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX history_connection ON history (connection_id, id DESC);
",
];

const MAX_SQL_BYTES: usize = 1024 * 1024;
/// Extra history SQL kept past the cap while redacting (see `add_history_batch`).
const REDACT_SLACK_BYTES: usize = 64 * 1024;
const MAX_KV_BYTES: usize = 4 * 1024 * 1024;
const SETTINGS_KEY: &str = "settings";

pub struct Store {
    db: Mutex<Connection>,
}

fn storage_error(e: rusqlite::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Local storage error: {e}"))
}

fn json_error(e: serde_json::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Local storage data is invalid: {e}"))
}

/// Longest prefix of `text` of at most `max` bytes, cut on a char boundary.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn environment_str(e: Environment) -> &'static str {
    match e {
        Environment::Local => "Local",
        Environment::Development => "Development",
        Environment::Staging => "Staging",
        Environment::Production => "Production",
    }
}

fn parse_environment(s: &str) -> Environment {
    match s {
        "Development" => Environment::Development,
        "Staging" => Environment::Staging,
        "Production" => Environment::Production,
        _ => Environment::Local,
    }
}

fn require_name(name: &str, what: &str) -> DbResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(DbError::invalid(format!("{what} name must be 1–200 characters.")));
    }
    Ok(name.to_owned())
}

impl Store {
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot create data directory: {e}")))?;
        }
        Self::init(Connection::open(path).map_err(storage_error)?)
    }

    pub fn in_memory() -> DbResult<Self> {
        Self::init(Connection::open_in_memory().map_err(storage_error)?)
    }

    fn init(conn: Connection) -> DbResult<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 2000;")
            .map_err(storage_error)?;
        let version: usize = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(storage_error)?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            conn.execute_batch(&format!("BEGIN; {migration}; PRAGMA user_version = {}; COMMIT;", i + 1))
                .map_err(storage_error)?;
        }
        Ok(Self { db: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> DbResult<T> {
        let db = self.db.lock().map_err(|_| DbError::internal("storage lock poisoned"))?;
        f(&db).map_err(storage_error)
    }

    // ------------------------------------------------------------------ folders

    pub fn folders(&self) -> DbResult<Vec<Folder>> {
        self.with(|db| {
            db.prepare("SELECT id, name FROM folders ORDER BY position, name COLLATE NOCASE")?
                .query_map([], |r| Ok(Folder { id: r.get(0)?, name: r.get(1)? }))?
                .collect()
        })
    }

    pub fn create_folder(&self, name: &str) -> DbResult<Folder> {
        let folder = Folder { id: new_id(), name: require_name(name, "Folder")? };
        self.with(|db| {
            db.execute(
                "INSERT INTO folders (id, name, position) VALUES (?1, ?2, (SELECT coalesce(max(position), 0) + 1 FROM folders))",
                params![folder.id, folder.name],
            )
        })?;
        Ok(folder)
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> DbResult<()> {
        let name = require_name(name, "Folder")?;
        self.with(|db| db.execute("UPDATE folders SET name = ?2 WHERE id = ?1", params![id, name])).map(drop)
    }

    /// Inserts or renames a folder with a known id (from sync).
    pub fn put_folder(&self, folder: &Folder) -> DbResult<()> {
        let name = require_name(&folder.name, "Folder")?;
        self.with(|db| {
            db.execute(
                "INSERT INTO folders (id, name, position) VALUES (?1, ?2, (SELECT coalesce(max(position), 0) + 1 FROM folders)) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2",
                params![folder.id, name],
            )
        })
        .map(drop)
    }

    /// Deletes a folder; its connections move to "no folder".
    pub fn delete_folder(&self, id: &str) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM folders WHERE id = ?1", params![id])).map(drop)
    }

    // -------------------------------------------------------------- connections

    fn connection_from_row(r: &Row<'_>) -> rusqlite::Result<SavedConnection> {
        let config: String = r.get(5)?;
        let config: ConnConfig = serde_json::from_str(&config)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(SavedConnection {
            id: r.get(0)?,
            name: r.get(1)?,
            folder_id: r.get(2)?,
            environment: parse_environment(&r.get::<_, String>(3)?),
            favorite: r.get(4)?,
            config,
            has_password: r.get(6)?,
            created_at: r.get(7)?,
            last_used_at: r.get(8)?,
            ssh_secrets: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        })
    }

    const CONNECTION_COLUMNS: &str =
        "id, name, folder_id, environment, favorite, config, has_password, created_at, last_used_at, ssh_secrets";

    pub fn connections(&self) -> DbResult<Vec<SavedConnection>> {
        self.with(|db| {
            db.prepare(&format!("SELECT {} FROM connections ORDER BY name COLLATE NOCASE", Self::CONNECTION_COLUMNS))?
                .query_map([], Self::connection_from_row)?
                .collect()
        })
    }

    pub fn connection(&self, id: &str) -> DbResult<SavedConnection> {
        self.with(|db| {
            db.query_row(
                &format!("SELECT {} FROM connections WHERE id = ?1", Self::CONNECTION_COLUMNS),
                params![id],
                Self::connection_from_row,
            )
            .optional()
        })?
        .ok_or_else(|| DbError::not_found("This connection no longer exists."))
    }

    /// Inserts or updates connection metadata. Secrets are handled by the
    /// caller; only whether each one exists is recorded here.
    pub fn save_connection(
        &self,
        draft: &ConnectionDraft,
        has_password: bool,
        ssh_secrets: &[bool],
    ) -> DbResult<SavedConnection> {
        draft.config.validate()?;
        let name = require_name(&draft.name, "Connection")?;
        let config = serde_json::to_string(&draft.config).map_err(json_error)?;
        let ssh = serde_json::to_string(ssh_secrets).map_err(json_error)?;
        let id = draft.id.clone().unwrap_or_else(new_id);
        self.with(|db| {
            db.execute(
                "INSERT INTO connections (id, name, folder_id, environment, favorite, config, has_password, created_at, ssh_secrets) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2, folder_id = ?3, environment = ?4, favorite = ?5, config = ?6, has_password = ?7, ssh_secrets = ?9",
                params![id, name, draft.folder_id, environment_str(draft.environment), draft.favorite, config, has_password, now_ms(), ssh],
            )
        })?;
        self.connection(&id)
    }

    pub fn delete_connection(&self, id: &str) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM connections WHERE id = ?1", params![id])).map(drop)
    }

    pub fn set_favorite(&self, id: &str, favorite: bool) -> DbResult<()> {
        self.with(|db| db.execute("UPDATE connections SET favorite = ?2 WHERE id = ?1", params![id, favorite]))
            .map(drop)
    }

    pub fn touch_connection(&self, id: &str) -> DbResult<()> {
        self.with(|db| db.execute("UPDATE connections SET last_used_at = ?2 WHERE id = ?1", params![id, now_ms()]))
            .map(drop)
    }

    /// Creates each connection under a new id, without secrets, and the folders
    /// they name. Invalid connections are reported, not saved.
    pub fn import_connections(&self, items: Vec<ConnectionImport>) -> DbResult<ConnectionImportReport> {
        let mut report = ConnectionImportReport::default();
        let mut folders = self.folders()?;
        for c in items {
            if let Err(e) = c.config.validate() {
                report.skipped.push(format!("{}: {}", c.name, e.message));
                continue;
            }
            let folder_id = match c.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
                Some(name) => Some(if let Some(f) = folders.iter().find(|f| f.name == name) {
                    f.id.clone()
                } else {
                    let folder = self.create_folder(name)?;
                    folders.push(folder.clone());
                    folder.id
                }),
                None => None,
            };
            let hops = c.config.ssh.as_ref().map_or(0, |t| t.hops().count());
            let draft = ConnectionDraft {
                id: None,
                name: c.name,
                folder_id,
                environment: c.environment,
                favorite: c.favorite,
                config: c.config,
                password: PasswordChange::Clear,
                ssh_secrets: Vec::new(),
            };
            match self.save_connection(&draft, false, &vec![false; hops]) {
                Ok(_) => report.imported += 1,
                Err(e) => report.skipped.push(format!("{}: {}", draft.name, e.message)),
            }
        }
        Ok(report)
    }

    // ------------------------------------------------------------------ history

    /// Records a statement. SQL is redacted and capped; rows are never stored.
    /// Entries beyond the retention limits are pruned.
    pub fn add_history(&self, entry: &NewHistoryEntry, retention: Retention) -> DbResult<()> {
        self.add_history_batch(std::slice::from_ref(entry), retention)
    }

    /// Records several statements (one script run) in a single transaction.
    pub fn add_history_batch(&self, entries: &[NewHistoryEntry], retention: Retention) -> DbResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        // Cap before redacting so huge statements are not scanned in full; the
        // slack keeps a secret that straddles the cap whole for `redact`.
        let rows: Vec<(String, Option<String>)> = entries
            .iter()
            .map(|e| {
                let sql = redact(truncate(&e.sql, MAX_SQL_BYTES + REDACT_SLACK_BYTES));
                (truncate(&sql, MAX_SQL_BYTES).to_owned(), e.error.as_deref().map(redact))
            })
            .collect();
        self.with(|db| {
            let tx = db.unchecked_transaction()?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO history (connection_id, connection_name, database, sql, executed_at, duration_ms, status, rows, error) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for (entry, (sql, error)) in entries.iter().zip(&rows) {
                    insert.execute(params![
                        entry.connection_id,
                        entry.connection_name,
                        entry.database,
                        sql,
                        entry.executed_at,
                        entry.duration_ms,
                        entry.status.as_str(),
                        entry.rows,
                        error
                    ])?;
                }
            }
            // Ids are consecutive inside the transaction; prune when the batch crossed a multiple of 100.
            let last = tx.last_insert_rowid();
            let first = last - entries.len() as i64 + 1;
            if last / 100 > (first - 1) / 100 {
                Self::prune(&tx, retention, last)?;
            }
            tx.commit()
        })
    }

    pub fn history(&self, query: &HistoryQuery) -> DbResult<HistoryPage> {
        let pattern = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let limit = i64::from(query.limit.clamp(1, 500));
        let status = query.status.map(HistoryStatus::as_str);
        let filter = "(?1 IS NULL OR sql LIKE ?1 ESCAPE '\\' OR connection_name LIKE ?1 ESCAPE '\\') \
                      AND (?2 IS NULL OR connection_id = ?2) AND (?5 IS NULL OR status = ?5) \
                      AND (?6 IS NULL OR executed_at >= ?6) AND (?7 IS NULL OR executed_at < ?7)";
        self.with(|db| {
            let total = db.query_row(
                &format!("SELECT count(*) FROM history WHERE {filter} AND ?3 IS NULL AND ?4 IS NULL"),
                params![pattern, query.connection_id, None::<i64>, None::<i64>, status, query.from_ms, query.to_ms],
                |r| r.get(0),
            )?;
            let entries = db
                .prepare(&format!(
                    "SELECT id, connection_id, connection_name, database, sql, executed_at, duration_ms, status, rows, error \
                     FROM history WHERE {filter} AND (?3 IS NULL OR id < ?3) ORDER BY id DESC LIMIT ?4"
                ))?
                .query_map(
                    params![pattern, query.connection_id, query.before_id, limit, status, query.from_ms, query.to_ms],
                    |r| {
                        Ok(HistoryEntry {
                            id: r.get(0)?,
                            connection_id: r.get(1)?,
                            connection_name: r.get(2)?,
                            database: r.get(3)?,
                            sql: r.get(4)?,
                            executed_at: r.get(5)?,
                            duration_ms: r.get(6)?,
                            status: HistoryStatus::parse(&r.get::<_, String>(7)?),
                            rows: r.get(8)?,
                            error: r.get(9)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
            Ok(HistoryPage { entries, total })
        })
    }

    fn prune(db: &Connection, retention: Retention, last_id: i64) -> rusqlite::Result<()> {
        if retention.max_entries > 0 {
            db.execute("DELETE FROM history WHERE id <= ?1", params![last_id - i64::from(retention.max_entries)])?;
        }
        if retention.max_days > 0 {
            let cutoff = now_ms() - i64::from(retention.max_days) * 86_400_000;
            db.execute("DELETE FROM history WHERE executed_at < ?1", params![cutoff])?;
        }
        Ok(())
    }

    /// Applies the retention limits now, e.g. after the user changed them.
    pub fn apply_retention(&self, retention: Retention) -> DbResult<()> {
        self.with(|db| {
            let last: i64 = db.query_row("SELECT coalesce(max(id), 0) FROM history", [], |r| r.get(0))?;
            Self::prune(db, retention, last)
        })
    }

    /// Clears all history, or only one connection's.
    pub fn clear_history(&self, connection_id: Option<&str>) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM history WHERE ?1 IS NULL OR connection_id = ?1", params![connection_id]))
            .map(drop)
    }

    // ------------------------------------------------------------------ scripts

    pub fn scripts(&self) -> DbResult<Vec<SavedScript>> {
        self.with(|db| {
            db.prepare(
                "SELECT id, name, sql, connection_id, created_at, updated_at, folder, tags, pinned FROM scripts \
                 ORDER BY pinned DESC, updated_at DESC",
            )?
            .query_map([], |r| {
                Ok(SavedScript {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    sql: r.get(2)?,
                    connection_id: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    folder: r.get(6)?,
                    tags: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                    pinned: r.get(8)?,
                })
            })?
            .collect()
        })
    }

    pub fn save_script(&self, draft: &ScriptDraft) -> DbResult<SavedScript> {
        let name = require_name(&draft.name, "Script")?;
        if draft.sql.len() > MAX_SQL_BYTES {
            return Err(DbError::invalid("Scripts are limited to 1 MB."));
        }
        let folder =
            draft.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(|f| require_name(f, "Folder"));
        let folder = folder.transpose()?;
        let mut tags: Vec<String> = draft
            .tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty() && t.chars().count() <= 40)
            .collect();
        tags.sort();
        tags.dedup();
        tags.truncate(20);
        let tags = serde_json::to_string(&tags).map_err(json_error)?;
        let id = draft.id.clone().unwrap_or_else(new_id);
        let now = now_ms();
        self.with(|db| {
            db.execute(
                "INSERT INTO scripts (id, name, sql, connection_id, created_at, updated_at, folder, tags, pinned) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2, sql = ?3, connection_id = ?4, updated_at = ?5, folder = ?6, tags = ?7, pinned = ?8",
                params![id, name, draft.sql, draft.connection_id, now, folder, tags, draft.pinned],
            )
        })?;
        self.scripts()?.into_iter().find(|s| s.id == id).ok_or_else(|| DbError::internal("script vanished after save"))
    }

    /// Updates only the SQL of a saved script (autosave from an open tab).
    /// With `base`, the update only applies while the stored SQL is still
    /// `base`; otherwise it fails with code `script_conflict` (the script was
    /// changed elsewhere, e.g. its file in the scripts folder).
    pub fn update_script_sql(&self, id: &str, sql: &str, base: Option<&str>) -> DbResult<()> {
        if sql.len() > MAX_SQL_BYTES {
            return Err(DbError::invalid("Scripts are limited to 1 MB."));
        }
        let changed = self.with(|db| {
            db.execute(
                "UPDATE scripts SET sql = ?2, updated_at = ?3 WHERE id = ?1 AND (?4 IS NULL OR sql = ?4)",
                params![id, sql, now_ms(), base],
            )
        })?;
        if changed == 0 {
            let exists = self
                .with(|db| db.query_row("SELECT 1 FROM scripts WHERE id = ?1", params![id], |_| Ok(())).optional())?;
            return Err(match exists {
                Some(()) => DbError::invalid("The script changed outside this tab.").with_code("script_conflict"),
                None => DbError::not_found("The script no longer exists."),
            });
        }
        Ok(())
    }

    // ----------------------------------------------------------------- snippets

    pub fn snippets(&self) -> DbResult<Vec<Snippet>> {
        self.with(|db| {
            db.prepare("SELECT id, name, prefix, body, updated_at FROM snippets ORDER BY prefix COLLATE NOCASE")?
                .query_map([], |r| {
                    Ok(Snippet {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        prefix: r.get(2)?,
                        body: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                })?
                .collect()
        })
    }

    pub fn save_snippet(&self, draft: &SnippetDraft) -> DbResult<Snippet> {
        let name = require_name(&draft.name, "Snippet")?;
        let prefix = draft.prefix.trim();
        if prefix.is_empty()
            || prefix.len() > 40
            || !prefix.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(DbError::invalid("The prefix must be 1–40 letters, digits, `_` or `-`."));
        }
        if draft.body.is_empty() || draft.body.len() > 64 * 1024 {
            return Err(DbError::invalid("A snippet body must be 1 byte to 64 KB."));
        }
        let id = draft.id.clone().unwrap_or_else(new_id);
        self.with(|db| {
            db.execute(
                "INSERT INTO snippets (id, name, prefix, body, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2, prefix = ?3, body = ?4, updated_at = ?5",
                params![id, name, prefix, draft.body, now_ms()],
            )
        })?;
        self.snippets()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| DbError::internal("snippet vanished after save"))
    }

    pub fn delete_snippet(&self, id: &str) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM snippets WHERE id = ?1", params![id])).map(drop)
    }

    pub fn delete_script(&self, id: &str) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM scripts WHERE id = ?1", params![id])).map(drop)
    }

    // ------------------------------------------------------- settings and state

    pub fn settings(&self) -> DbResult<Settings> {
        Ok(self
            .get_value(SETTINGS_KEY)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(json_error)?
            .unwrap_or_default())
    }

    pub fn save_settings(&self, settings: Settings) -> DbResult<Settings> {
        let settings = settings.normalized();
        self.set_value(SETTINGS_KEY, &serde_json::to_value(&settings).map_err(json_error)?)?;
        Ok(settings)
    }

    /// Opaque UI state (layout, open tabs). Never contains secrets or results.
    pub fn get_value(&self, key: &str) -> DbResult<Option<serde_json::Value>> {
        let raw: Option<String> =
            self.with(|db| db.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| r.get(0)).optional())?;
        raw.map(|s| serde_json::from_str(&s)).transpose().map_err(json_error)
    }

    pub fn set_value(&self, key: &str, value: &serde_json::Value) -> DbResult<()> {
        let raw = serde_json::to_string(value).map_err(json_error)?;
        if raw.len() > MAX_KV_BYTES || key.is_empty() || key.len() > 200 {
            return Err(DbError::invalid("UI state is too large to save."));
        }
        self.with(|db| {
            db.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
                params![key, raw],
            )
        })
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::config::SslMode;
    use dq_core::engine::Engine;

    fn draft(name: &str) -> ConnectionDraft {
        ConnectionDraft {
            id: None,
            name: name.into(),
            folder_id: None,
            environment: Environment::Production,
            favorite: false,
            config: ConnConfig {
                engine: Engine::Postgres,
                host: "db.local".into(),
                port: 5432,
                database: "app".into(),
                username: "app".into(),
                ssl_mode: SslMode::VerifyFull,
                connect_timeout_secs: 10,
                ..ConnConfig::default()
            },
            password: PasswordChange::Set { value: "hunter2-secret".into() },
            ssh_secrets: Vec::new(),
        }
    }

    #[test]
    fn imports_connections_into_shared_folders() {
        let store = Store::in_memory().unwrap();
        let item = |name: &str, user: &str| ConnectionImport {
            name: name.into(),
            folder: Some("Team".into()),
            environment: Environment::Staging,
            favorite: false,
            config: ConnConfig { username: user.into(), ..draft(name).config },
        };
        let report = store.import_connections(vec![item("a", "app"), item("b", "app"), item("c", "")]).unwrap();
        assert_eq!((report.imported, report.skipped), (2, vec!["c: Username is required.".to_owned()]));
        let folders = store.folders().unwrap();
        assert_eq!(folders.len(), 1);
        let saved = store.connections().unwrap();
        assert!(saved.iter().all(|c| c.folder_id.as_deref() == Some(folders[0].id.as_str()) && !c.has_password));
    }

    #[test]
    fn connections_round_trip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dataquery.db");
        let store = Store::open(&path).unwrap();
        let folder = store.create_folder("Kestrel").unwrap();
        let mut d = draft("kestrel-prod");
        d.folder_id = Some(folder.id.clone());
        let saved = store.save_connection(&d, true, &[]).unwrap();
        assert!(saved.has_password);
        assert_eq!(saved.environment, Environment::Production);
        assert_eq!(store.connections().unwrap().len(), 1);

        d.id = Some(saved.id.clone());
        d.name = "renamed".into();
        store.save_connection(&d, false, &[]).unwrap();
        let reloaded = store.connection(&saved.id).unwrap();
        assert_eq!((reloaded.name.as_str(), reloaded.has_password), ("renamed", false));

        store.delete_folder(&folder.id).unwrap();
        assert_eq!(store.connection(&saved.id).unwrap().folder_id, None);
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        let wal = std::fs::read(dir.path().join("dataquery.db-wal")).unwrap_or_default();
        let haystack = [bytes, wal].concat();
        assert!(!haystack.windows(14).any(|w| w == b"hunter2-secret"), "secret must never reach the settings file");
    }

    #[test]
    fn history_is_redacted_searchable_and_paged() {
        let store = Store::in_memory().unwrap();
        for i in 0..5 {
            store
                .add_history(
                    &NewHistoryEntry {
                        connection_id: Some("c1".into()),
                        connection_name: "dev".into(),
                        database: "app".into(),
                        sql: format!("SELECT {i} FROM orders_{i}"),
                        executed_at: now_ms(),
                        duration_ms: 3,
                        status: HistoryStatus::Ok,
                        rows: Some(1),
                        error: None,
                    },
                    Retention::default(),
                )
                .unwrap();
        }
        store
            .add_history(
                &NewHistoryEntry {
                    connection_id: None,
                    connection_name: "dev".into(),
                    database: "app".into(),
                    sql: "ALTER ROLE app PASSWORD 'topsecret'".into(),
                    executed_at: now_ms(),
                    duration_ms: 1,
                    status: HistoryStatus::Error,
                    rows: None,
                    error: Some("failed for postgres://app:topsecret@x".into()),
                },
                Retention::default(),
            )
            .unwrap();
        let page =
            store.history(&HistoryQuery { search: Some("orders_".into()), limit: 2, ..Default::default() }).unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.entries.len(), 2);
        let next = store
            .history(&HistoryQuery {
                search: Some("orders".into()),
                before_id: Some(page.entries[1].id),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(next.entries.len(), 3);
        let all = store.history(&HistoryQuery { limit: 50, ..Default::default() }).unwrap();
        assert!(!all.entries[0].sql.contains("topsecret"));
        assert!(!all.entries[0].error.as_deref().unwrap().contains("topsecret"));
        // `_` is a literal, not a wildcard.
        assert_eq!(
            store
                .history(&HistoryQuery { search: Some("s_lect".into()), limit: 10, ..Default::default() })
                .unwrap()
                .total,
            0
        );
    }

    #[test]
    fn history_filters_and_retention() {
        let store = Store::in_memory().unwrap();
        let entry = |status, conn: &str| NewHistoryEntry {
            connection_id: Some(conn.into()),
            connection_name: conn.into(),
            database: "app".into(),
            sql: "select 1".into(),
            executed_at: now_ms(),
            duration_ms: 1,
            status,
            rows: None,
            error: None,
        };
        for i in 0..6 {
            let status = if i % 2 == 0 { HistoryStatus::Ok } else { HistoryStatus::Error };
            store.add_history(&entry(status, if i < 3 { "a" } else { "b" }), Retention::default()).unwrap();
        }
        let errors = store
            .history(&HistoryQuery { status: Some(HistoryStatus::Error), limit: 50, ..Default::default() })
            .unwrap();
        assert_eq!(errors.total, 3);
        let b_ok = store
            .history(&HistoryQuery {
                status: Some(HistoryStatus::Ok),
                connection_id: Some("b".into()),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(b_ok.total, 1);
        let future =
            store.history(&HistoryQuery { from_ms: Some(now_ms() + 60_000), limit: 50, ..Default::default() }).unwrap();
        assert_eq!(future.total, 0);
        store.apply_retention(Retention { max_entries: 2, max_days: 0 }).unwrap();
        assert_eq!(store.history(&HistoryQuery { limit: 50, ..Default::default() }).unwrap().total, 2);
        store.clear_history(Some("b")).unwrap();
        assert_eq!(store.history(&HistoryQuery { limit: 50, ..Default::default() }).unwrap().total, 0);
    }

    #[test]
    fn history_batch_keeps_order_timing_and_redaction() {
        let store = Store::in_memory().unwrap();
        let entry = |i: i64| NewHistoryEntry {
            connection_id: Some("c1".into()),
            connection_name: "dev".into(),
            database: "app".into(),
            sql: if i == 1 { "ALTER ROLE app PASSWORD 'topsecret'".into() } else { format!("select {i}") },
            executed_at: 1_000 + i,
            duration_ms: i,
            status: if i == 2 { HistoryStatus::Error } else { HistoryStatus::Ok },
            rows: Some(i),
            error: (i == 2).then(|| "token=abc123".into()),
        };
        let batch: Vec<_> = (0..250).map(entry).collect();
        store.add_history_batch(&batch, Retention { max_entries: 120, max_days: 0 }).unwrap();
        store.add_history_batch(&[], Retention::default()).unwrap();
        let page = store.history(&HistoryQuery { limit: 500, ..Default::default() }).unwrap();
        // The batch crossed id 200, so retention pruned it down to the last 120 + those after id 200.
        assert_eq!(page.total, 120);
        let oldest = page.entries.last().unwrap();
        assert_eq!((oldest.sql.as_str(), oldest.executed_at, oldest.duration_ms), ("select 130", 1_130, 130));
        assert_eq!(page.entries[0].sql, "select 249");

        store.clear_history(None).unwrap();
        store.add_history_batch(&batch[..3], Retention::default()).unwrap();
        let page = store.history(&HistoryQuery { limit: 10, ..Default::default() }).unwrap();
        let sqls: Vec<_> = page.entries.iter().map(|e| e.sql.as_str()).collect();
        assert_eq!(sqls, ["select 2", "ALTER ROLE app PASSWORD '***'", "select 0"]);
        assert_eq!(page.entries[0].error.as_deref(), Some("token=***"));
        assert_eq!(page.entries[0].status, HistoryStatus::Error);
    }

    #[test]
    fn history_sql_is_capped_on_a_char_boundary() {
        let store = Store::in_memory().unwrap();
        let sql = format!("select '{}' -- password=hunter2", "é".repeat(MAX_SQL_BYTES));
        let entry = NewHistoryEntry {
            connection_id: None,
            connection_name: "dev".into(),
            database: "app".into(),
            sql,
            executed_at: now_ms(),
            duration_ms: 1,
            status: HistoryStatus::Ok,
            rows: None,
            error: None,
        };
        store.add_history(&entry, Retention::default()).unwrap();
        let page = store.history(&HistoryQuery { limit: 1, ..Default::default() }).unwrap();
        let stored = &page.entries[0].sql;
        assert!(stored.len() <= MAX_SQL_BYTES && stored.len() > MAX_SQL_BYTES - 4);
        assert!(stored.starts_with("select 'éé"));
    }

    #[test]
    fn settings_and_scripts() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.settings().unwrap(), Settings::default());
        let saved = store.save_settings(Settings { row_limit: 0, ..Settings::default() }).unwrap();
        assert_eq!(saved.row_limit, 1);
        let script = store
            .save_script(&ScriptDraft { name: "paid".into(), sql: "select 1".into(), ..ScriptDraft::default() })
            .unwrap();
        store
            .save_script(&ScriptDraft {
                id: Some(script.id.clone()),
                name: "paid 2".into(),
                sql: "select 2".into(),
                folder: Some(" Reports ".into()),
                tags: vec!["b".into(), "a".into(), "a".into(), " ".into()],
                ..ScriptDraft::default()
            })
            .unwrap();
        let other = store
            .save_script(&ScriptDraft {
                name: "z".into(),
                sql: "select 3".into(),
                pinned: true,
                ..ScriptDraft::default()
            })
            .unwrap();
        let scripts = store.scripts().unwrap();
        assert_eq!(scripts[0].id, other.id, "pinned scripts come first");
        assert_eq!(
            (scripts[1].folder.as_deref(), scripts[1].tags.clone()),
            (Some("Reports"), vec!["a".to_owned(), "b".to_owned()])
        );
        store.update_script_sql(&script.id, "select 4", None).unwrap();
        assert_eq!(store.scripts().unwrap()[1].sql, "select 4");
        assert!(store.update_script_sql("missing", "x", None).is_err());
        let stale = store.update_script_sql(&script.id, "select 5", Some("select 2")).unwrap_err();
        assert_eq!(stale.code.as_deref(), Some("script_conflict"));
        store.update_script_sql(&script.id, "select 5", Some("select 4")).unwrap();

        let snippet = store
            .save_snippet(&SnippetDraft {
                id: None,
                name: "Count".into(),
                prefix: "cnt".into(),
                body: "SELECT count(*) FROM ${1:table};".into(),
            })
            .unwrap();
        assert!(store.save_snippet(&SnippetDraft { prefix: "bad prefix".into(), ..SnippetDraft::default() }).is_err());
        assert_eq!(store.snippets().unwrap().len(), 1);
        store.delete_snippet(&snippet.id).unwrap();
        assert!(store.snippets().unwrap().is_empty());
        store.set_value("workspace:c1", &serde_json::json!({"tabs": []})).unwrap();
        assert_eq!(store.get_value("workspace:c1").unwrap().unwrap()["tabs"], serde_json::json!([]));
    }
}
