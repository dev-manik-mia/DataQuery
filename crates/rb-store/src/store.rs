//! SQLite-backed metadata store. All methods are synchronous; callers run them
//! on a blocking thread.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rb_core::config::{ConnConfig, Environment};
use rb_core::redact::redact;
use rb_core::{DbError, DbResult, ErrorKind};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    ConnectionDraft, Folder, HistoryEntry, HistoryPage, HistoryQuery, HistoryStatus, NewHistoryEntry, SavedConnection,
    SavedScript, ScriptDraft, Settings,
};

const MIGRATIONS: &[&str] = &[r"
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
"];

/// History keeps at most this many entries; older ones are pruned.
pub const HISTORY_RETENTION: i64 = 20_000;
const MAX_SQL_BYTES: usize = 1024 * 1024;
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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 2000;")
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
        })
    }

    const CONNECTION_COLUMNS: &str =
        "id, name, folder_id, environment, favorite, config, has_password, created_at, last_used_at";

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

    /// Inserts or updates connection metadata. The password is handled by the caller.
    pub fn save_connection(&self, draft: &ConnectionDraft, has_password: bool) -> DbResult<SavedConnection> {
        draft.config.validate()?;
        let name = require_name(&draft.name, "Connection")?;
        let config = serde_json::to_string(&draft.config).map_err(json_error)?;
        let id = draft.id.clone().unwrap_or_else(new_id);
        self.with(|db| {
            db.execute(
                "INSERT INTO connections (id, name, folder_id, environment, favorite, config, has_password, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2, folder_id = ?3, environment = ?4, favorite = ?5, config = ?6, has_password = ?7",
                params![id, name, draft.folder_id, environment_str(draft.environment), draft.favorite, config, has_password, now_ms()],
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

    // ------------------------------------------------------------------ history

    /// Records a statement. SQL is redacted and capped; rows are never stored.
    pub fn add_history(&self, entry: &NewHistoryEntry) -> DbResult<()> {
        let mut sql = redact(&entry.sql);
        if sql.len() > MAX_SQL_BYTES {
            let mut cut = MAX_SQL_BYTES;
            while !sql.is_char_boundary(cut) {
                cut -= 1;
            }
            sql.truncate(cut);
        }
        let error = entry.error.as_deref().map(redact);
        self.with(|db| {
            db.execute(
                "INSERT INTO history (connection_id, connection_name, database, sql, executed_at, duration_ms, status, rows, error) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.connection_id,
                    entry.connection_name,
                    entry.database,
                    sql,
                    now_ms(),
                    entry.duration_ms,
                    entry.status.as_str(),
                    entry.rows,
                    error
                ],
            )?;
            let id = db.last_insert_rowid();
            if id % 500 == 0 {
                db.execute("DELETE FROM history WHERE id <= ?1", params![id - HISTORY_RETENTION])?;
            }
            Ok(())
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
        let filter = "(?1 IS NULL OR sql LIKE ?1 ESCAPE '\\' OR connection_name LIKE ?1 ESCAPE '\\') AND (?2 IS NULL OR connection_id = ?2)";
        self.with(|db| {
            let total = db.query_row(&format!("SELECT count(*) FROM history WHERE {filter}"), params![pattern, query.connection_id], |r| {
                r.get(0)
            })?;
            let entries = db
                .prepare(&format!(
                    "SELECT id, connection_id, connection_name, database, sql, executed_at, duration_ms, status, rows, error \
                     FROM history WHERE {filter} AND (?3 IS NULL OR id < ?3) ORDER BY id DESC LIMIT ?4"
                ))?
                .query_map(params![pattern, query.connection_id, query.before_id, limit], |r| {
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
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(HistoryPage { entries, total })
        })
    }

    pub fn clear_history(&self) -> DbResult<()> {
        self.with(|db| db.execute("DELETE FROM history", [])).map(drop)
    }

    // ------------------------------------------------------------------ scripts

    pub fn scripts(&self) -> DbResult<Vec<SavedScript>> {
        self.with(|db| {
            db.prepare(
                "SELECT id, name, sql, connection_id, created_at, updated_at FROM scripts ORDER BY updated_at DESC",
            )?
            .query_map([], |r| {
                Ok(SavedScript {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    sql: r.get(2)?,
                    connection_id: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
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
        let id = draft.id.clone().unwrap_or_else(new_id);
        let now = now_ms();
        self.with(|db| {
            db.execute(
                "INSERT INTO scripts (id, name, sql, connection_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT (id) DO UPDATE SET name = ?2, sql = ?3, connection_id = ?4, updated_at = ?5",
                params![id, name, draft.sql, draft.connection_id, now],
            )
        })?;
        self.scripts()?.into_iter().find(|s| s.id == id).ok_or_else(|| DbError::internal("script vanished after save"))
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
    use crate::model::PasswordChange;
    use rb_core::config::SslMode;
    use rb_core::engine::Engine;

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
            },
            password: PasswordChange::Set { value: "hunter2-secret".into() },
        }
    }

    #[test]
    fn connections_round_trip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rustbase.db");
        let store = Store::open(&path).unwrap();
        let folder = store.create_folder("Kestrel").unwrap();
        let mut d = draft("kestrel-prod");
        d.folder_id = Some(folder.id.clone());
        let saved = store.save_connection(&d, true).unwrap();
        assert!(saved.has_password);
        assert_eq!(saved.environment, Environment::Production);
        assert_eq!(store.connections().unwrap().len(), 1);

        d.id = Some(saved.id.clone());
        d.name = "renamed".into();
        store.save_connection(&d, false).unwrap();
        let reloaded = store.connection(&saved.id).unwrap();
        assert_eq!((reloaded.name.as_str(), reloaded.has_password), ("renamed", false));

        store.delete_folder(&folder.id).unwrap();
        assert_eq!(store.connection(&saved.id).unwrap().folder_id, None);
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        let wal = std::fs::read(dir.path().join("rustbase.db-wal")).unwrap_or_default();
        let haystack = [bytes, wal].concat();
        assert!(!haystack.windows(14).any(|w| w == b"hunter2-secret"), "secret must never reach the settings file");
    }

    #[test]
    fn history_is_redacted_searchable_and_paged() {
        let store = Store::in_memory().unwrap();
        for i in 0..5 {
            store
                .add_history(&NewHistoryEntry {
                    connection_id: Some("c1".into()),
                    connection_name: "dev".into(),
                    database: "app".into(),
                    sql: format!("SELECT {i} FROM orders_{i}"),
                    duration_ms: 3,
                    status: HistoryStatus::Ok,
                    rows: Some(1),
                    error: None,
                })
                .unwrap();
        }
        store
            .add_history(&NewHistoryEntry {
                connection_id: None,
                connection_name: "dev".into(),
                database: "app".into(),
                sql: "ALTER ROLE app PASSWORD 'topsecret'".into(),
                duration_ms: 1,
                status: HistoryStatus::Error,
                rows: None,
                error: Some("failed for postgres://app:topsecret@x".into()),
            })
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
    fn settings_and_scripts() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.settings().unwrap(), Settings::default());
        let saved = store.save_settings(Settings { row_limit: 0, ..Settings::default() }).unwrap();
        assert_eq!(saved.row_limit, 1);
        let script = store
            .save_script(&ScriptDraft { id: None, name: "paid".into(), sql: "select 1".into(), connection_id: None })
            .unwrap();
        store
            .save_script(&ScriptDraft {
                id: Some(script.id.clone()),
                name: "paid 2".into(),
                sql: "select 2".into(),
                connection_id: None,
            })
            .unwrap();
        assert_eq!(store.scripts().unwrap()[0].name, "paid 2");
        store.set_value("workspace:c1", &serde_json::json!({"tabs": []})).unwrap();
        assert_eq!(store.get_value("workspace:c1").unwrap().unwrap()["tabs"], serde_json::json!([]));
    }
}
