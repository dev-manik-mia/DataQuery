//! Reads saved connections from other database clients: `TablePlus`, `DBeaver`,
//! Sequel Ace and Sequel Pro, and `DataGrip` (or any `JetBrains` IDE with the
//! database plugin).
//!
//! Every file is untrusted input: sizes are capped, values are cleaned, and
//! each connection is validated like one typed into the form. Passwords and
//! other secrets are never read; the other apps keep them in the keychain or
//! in separate credential files, which are not opened.

mod dbeaver;
mod jdbc;
mod jetbrains;
mod locate;
mod sequel;
mod tableplus;
mod xml;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use dq_core::config::{ConnConfig, Environment, SshAuth, SshHop, SshTunnel, SslMode, TlsFiles};
use dq_core::engine::Engine;
use dq_core::{DbError, DbResult, ErrorKind};
use plist::Value;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use locate::{Os, detect};

use crate::model::{ConnectionImport, SavedConnection, clean_text};

/// Largest file read from another app.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Most connections read from one file.
const MAX_CONNECTIONS: usize = 5000;
/// Longest value kept from another app's file; validation applies the real limits.
const MAX_VALUE_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SourceApp {
    Tableplus,
    Dbeaver,
    SequelAce,
    SequelPro,
    /// `DataGrip` and the other `JetBrains` IDEs (`dataSources.xml`).
    Datagrip,
}

impl SourceApp {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tableplus => "TablePlus",
            Self::Dbeaver => "DBeaver",
            Self::SequelAce => "Sequel Ace",
            Self::SequelPro => "Sequel Pro",
            Self::Datagrip => "DataGrip",
        }
    }
}

/// A connection file of another app found at its usual place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct DetectedSource {
    pub app: SourceApp,
    pub path: String,
    /// Which install, workspace or project the file belongs to.
    pub label: String,
}

/// A connection read from another app, ready to import.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ForeignConnection {
    pub connection: ConnectionImport,
    /// Why it cannot be saved as read (e.g. no username).
    pub problem: Option<String>,
    /// Name of a saved connection with the same engine, host, port, user and database.
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ForeignPreview {
    pub app: SourceApp,
    pub connections: Vec<ForeignConnection>,
    /// One line per entry that cannot be imported, with the reason.
    pub skipped: Vec<String>,
}

/// The user's folders, passed in so detection and path expansion are testable.
#[derive(Debug, Clone)]
pub struct UserDirs {
    pub os: Os,
    pub home: PathBuf,
    /// Windows `%APPDATA%` (roaming).
    pub app_data: Option<PathBuf>,
    /// Windows `%LOCALAPPDATA%`.
    pub local_app_data: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`.
    pub config_home: Option<PathBuf>,
    /// `$XDG_DATA_HOME`.
    pub data_home: Option<PathBuf>,
}

impl UserDirs {
    pub fn current() -> DbResult<Self> {
        let home = std::env::home_dir()
            .filter(|h| h.is_absolute())
            .ok_or_else(|| DbError::invalid("Your home folder could not be found."))?;
        let var = |key: &str| std::env::var_os(key).map(PathBuf::from).filter(|p| p.is_absolute());
        Ok(Self {
            os: Os::current(),
            home,
            app_data: var("APPDATA"),
            local_app_data: var("LOCALAPPDATA"),
            config_home: var("XDG_CONFIG_HOME"),
            data_home: var("XDG_DATA_HOME"),
        })
    }

    /// A path from another app's settings with `~/`, `$USER_HOME$` and
    /// `$PROJECT_DIR$` expanded. Relative paths stay relative; validation reports them.
    fn expand(&self, path: &str, project: Option<&Path>) -> String {
        let path = clean_text(path, MAX_VALUE_CHARS);
        let home = self.home.to_string_lossy();
        let mut path = path.replace("$USER_HOME$", &home);
        if let Some(project) = project {
            path = path.replace("$PROJECT_DIR$", &project.to_string_lossy());
        }
        match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            Some(rest) => self.home.join(rest).to_string_lossy().into_owned(),
            None => path,
        }
    }

    /// An SSH key reference: a bare file name is a key in `~/.ssh`.
    fn ssh_key(&self, path: &str, project: Option<&Path>) -> String {
        let path = path.trim();
        if path.is_empty() || path.contains(['/', '\\']) || path.starts_with('$') {
            self.expand(path, project)
        } else {
            self.home.join(".ssh").join(path).to_string_lossy().into_owned()
        }
    }
}

/// Reads the connections in another app's file (or the folder that holds it).
pub fn read(app: SourceApp, path: &Path, dirs: &UserDirs) -> DbResult<ForeignPreview> {
    let mut found = Found::default();
    match app {
        SourceApp::Tableplus => tableplus::read(path, dirs, &mut found)?,
        SourceApp::Dbeaver => dbeaver::read(path, dirs, &mut found)?,
        SourceApp::SequelAce | SourceApp::SequelPro => sequel::read(path, dirs, &mut found)?,
        SourceApp::Datagrip => jetbrains::read(path, dirs, &mut found)?,
    }
    Ok(found.finish(app))
}

/// Marks connections that match a saved one: same engine, host, port, user and database.
pub fn mark_duplicates(preview: &mut ForeignPreview, saved: &[SavedConnection]) {
    let key = |c: &ConnConfig| {
        (
            c.engine,
            c.host.trim().to_ascii_lowercase(),
            c.port,
            c.username.trim().to_owned(),
            c.database.trim().to_owned(),
        )
    };
    for found in &mut preview.connections {
        let wanted = key(&found.connection.config);
        found.duplicate_of = saved.iter().find(|s| key(&s.config) == wanted).map(|s| s.name.clone());
    }
}

// ---------------------------------------------------------------------------
// What each reader produces
// ---------------------------------------------------------------------------

/// A connection as another app stores it, before defaults and validation.
#[derive(Debug, Default)]
struct Raw {
    name: String,
    /// The other app's driver or provider name, for messages.
    driver: String,
    engine: Option<Engine>,
    host: String,
    port: Option<u16>,
    database: String,
    username: String,
    ssl: Option<SslMode>,
    tls: TlsFiles,
    ssh: Option<SshHop>,
    folder: Option<String>,
    environment: Environment,
    read_only: bool,
    properties: BTreeMap<String, String>,
}

impl Raw {
    fn into_connection(self) -> Result<ForeignConnection, String> {
        let name = clean_text(&self.name, 200);
        let Some(engine) = self.engine else {
            let driver = clean_text(&self.driver, 64);
            let what = if driver.is_empty() {
                "The database type is unknown".to_owned()
            } else {
                format!("{driver} is not supported")
            };
            return Err(format!("{}: {what}.", if name.is_empty() { "(unnamed)" } else { &name }));
        };
        let file_based = engine.is_file_based();
        let host = clean_text(&self.host, 255);
        let mut port = self.port.filter(|p| *p != 0).or_else(|| engine.default_port()).unwrap_or(0);
        // DataQuery speaks Neo4j's HTTP API: the Bolt port maps to the HTTP one.
        if engine == Engine::Neo4j && port == 7687 {
            port = if self.ssl == Some(SslMode::Disable) { 7474 } else { 7473 };
        }
        let config = ConnConfig {
            engine,
            host: match (file_based, host.is_empty()) {
                (true, _) => String::new(),
                (false, true) => "localhost".to_owned(),
                (false, false) => host,
            },
            port: if file_based { 0 } else { port },
            database: clean_text(&self.database, MAX_VALUE_CHARS),
            username: clean_text(&self.username, MAX_VALUE_CHARS),
            // The other apps fall back to plain text when the server has no TLS; so does `prefer`.
            ssl_mode: self.ssl.unwrap_or(SslMode::Prefer),
            tls_files: self.tls,
            ssh: self.ssh.filter(|_| !file_based).map(|host| SshTunnel { host, jumps: Vec::new() }),
            read_only: self.read_only,
            properties: self.properties,
            ..ConnConfig::default()
        };
        let name = if name.is_empty() { config.address() } else { name };
        let problem = config.validate().err().map(|e| e.message);
        let folder = self.folder.map(|f| clean_text(&f, 200)).filter(|f| !f.is_empty());
        let connection = ConnectionImport { name, folder, environment: self.environment, favorite: false, config };
        Ok(ForeignConnection { connection, problem, duplicate_of: None })
    }
}

#[derive(Debug, Default)]
struct Found {
    raw: Vec<Raw>,
    skipped: Vec<String>,
}

impl Found {
    fn push(&mut self, raw: Raw) {
        self.raw.push(raw);
    }

    fn skip(&mut self, name: &str, reason: &str) {
        let name = clean_text(name, 200);
        self.skipped.push(format!("{}: {reason}", if name.is_empty() { "(unnamed)" } else { &name }));
    }

    fn finish(self, app: SourceApp) -> ForeignPreview {
        let mut preview = ForeignPreview { app, connections: Vec::new(), skipped: self.skipped };
        let total = self.raw.len();
        for raw in self.raw.into_iter().take(MAX_CONNECTIONS) {
            match raw.into_connection() {
                Ok(c) => preview.connections.push(c),
                Err(reason) => preview.skipped.push(reason),
            }
        }
        if total > MAX_CONNECTIONS {
            preview.skipped.push(format!(
                "{} more connections: at most {MAX_CONNECTIONS} are read from one file.",
                total - MAX_CONNECTIONS
            ));
        }
        preview
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Maps another app's driver, provider or JDBC name onto an engine. Names of
/// databases that only resemble a supported one map to `None`.
fn engine_named(name: &str) -> Option<Engine> {
    const NAMES: &[(&str, Option<Engine>)] = &[
        ("redshift", Some(Engine::Redshift)),
        ("greenplum", Some(Engine::Greenplum)),
        ("cloudberry", Some(Engine::Greenplum)),
        ("libsql", None),
        ("cockroach", Some(Engine::Cockroachdb)),
        ("mariadb", Some(Engine::Mariadb)),
        ("mysql", Some(Engine::Mysql)),
        ("postgre", Some(Engine::Postgres)),
        ("pgsql", Some(Engine::Postgres)),
        ("sqlserver", Some(Engine::Sqlserver)),
        ("sql server", Some(Engine::Sqlserver)),
        ("mssql", Some(Engine::Sqlserver)),
        ("jtds", Some(Engine::Sqlserver)),
        ("oracle", Some(Engine::Oracle)),
        ("clickhouse", Some(Engine::Clickhouse)),
        ("duckdb", Some(Engine::Duckdb)),
        ("sqlite", Some(Engine::Sqlite)),
        ("snowflake", Some(Engine::Snowflake)),
        ("vertica", Some(Engine::Vertica)),
        ("valkey", Some(Engine::Valkey)),
        ("dragonfly", Some(Engine::Dragonfly)),
        ("redis", Some(Engine::Redis)),
        ("mongo", Some(Engine::Mongodb)),
        ("dynamo", Some(Engine::Dynamodb)),
        ("scylla", Some(Engine::Scylladb)),
        ("cassandra", Some(Engine::Cassandra)),
        ("elastic", Some(Engine::Elasticsearch)),
        ("opensearch", Some(Engine::Elasticsearch)),
        ("neo4j", Some(Engine::Neo4j)),
    ];
    let name = name.to_ascii_lowercase();
    NAMES.iter().find(|(key, _)| name.contains(key)).and_then(|(_, engine)| *engine)
}

/// Environment from a label such as `TablePlus` `production` or `DBeaver` `prod`.
fn environment_named(name: &str) -> Option<Environment> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "production" | "prod" | "live" => Environment::Production,
        "staging" | "stage" | "testing" | "test" | "qa" | "uat" => Environment::Staging,
        "development" | "dev" => Environment::Development,
        "local" => Environment::Local,
        _ => return None,
    })
}

/// One SSH hop. A key path means key authentication; `agent` the SSH agent.
fn ssh_hop(host: &str, port: Option<u16>, user: &str, key: Option<String>, agent: bool) -> Option<SshHop> {
    let host = clean_text(host, 255);
    if host.is_empty() {
        return None;
    }
    let auth = match key.filter(|k| !k.is_empty()) {
        Some(path) => SshAuth::PrivateKey { path },
        None if agent => SshAuth::Agent,
        None => SshAuth::Password,
    };
    Some(SshHop { host, port: port.filter(|p| *p != 0).unwrap_or(22), username: clean_text(user, 128), auth })
}

/// Reads a file of at most [`MAX_FILE_BYTES`].
fn read_file(path: &Path) -> DbResult<Vec<u8>> {
    let io = |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot read {}: {e}", path.display()));
    let meta = std::fs::metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(DbError::invalid(format!("{} is not a file.", path.display())));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(DbError::invalid("The file is too large to be a connection list."));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path).map_err(io)?.take(MAX_FILE_BYTES).read_to_end(&mut bytes).map_err(io)?;
    Ok(bytes)
}

fn read_text(path: &Path) -> DbResult<String> {
    let bytes = read_file(path)?;
    let text = String::from_utf8(bytes).map_err(|_| DbError::invalid("The file is not UTF-8 text."))?;
    Ok(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

/// `path` itself, or the first of `names` inside it when it is a folder.
fn file_in(path: &Path, names: &[&str]) -> PathBuf {
    if path.is_dir() {
        names.iter().map(|n| path.join(n)).find(|p| p.is_file()).unwrap_or_else(|| path.join(names[0]))
    } else {
        path.to_owned()
    }
}

/// A property list (XML or binary) or JSON document as one value tree.
fn parse_tree(bytes: &[u8]) -> Option<Value> {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace() && *b != 0xEF && *b != 0xBB && *b != 0xBF)?;
    let body = &bytes[start..];
    match body[0] {
        b'{' | b'[' => from_json(serde_json::from_slice(body).ok()?),
        b'<' => Value::from_reader_xml(body).ok(),
        _ if body.starts_with(b"bplist") => Value::from_reader(std::io::Cursor::new(body)).ok(),
        // Anything else (old ASCII plists, encrypted data) is not a connection list.
        _ => None,
    }
}

/// JSON as a plist value, so plist and JSON files share one set of readers.
fn from_json(json: serde_json::Value) -> Option<Value> {
    Some(match json {
        serde_json::Value::Null => return None,
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i.into()),
            None => Value::Real(n.as_f64()?),
        },
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(items) => Value::Array(items.into_iter().filter_map(from_json).collect()),
        serde_json::Value::Object(map) => {
            Value::Dictionary(map.into_iter().filter_map(|(k, v)| Some((k, from_json(v)?))).collect())
        }
    })
}

/// Typed reads from a dictionary, tolerant of how apps store numbers and flags.
#[derive(Clone, Copy)]
struct Dict<'a>(&'a plist::Dictionary);

impl<'a> Dict<'a> {
    fn of(value: &'a Value) -> Option<Self> {
        value.as_dictionary().map(Self)
    }

    fn get(self, key: &str) -> Option<&'a Value> {
        self.0.get(key)
    }

    /// Trimmed string; empty when missing or not a string.
    fn str(self, key: &str) -> &'a str {
        self.get(key).and_then(Value::as_string).map_or("", str::trim)
    }

    /// First non-empty string among `keys`.
    fn any_str(self, keys: &[&str]) -> &'a str {
        keys.iter().map(|k| self.str(k)).find(|s| !s.is_empty()).unwrap_or("")
    }

    fn int(self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Integer(i) => i.as_signed(),
            Value::String(s) => s.trim().parse().ok(),
            Value::Real(r) if r.fract() == 0.0 && r.abs() < 1e15 => Some(*r as i64),
            _ => None,
        }
    }

    fn port(self, key: &str) -> Option<u16> {
        self.int(key).and_then(|p| u16::try_from(p).ok())
    }

    /// Booleans, non-zero numbers and `true` / `yes` / `1` strings.
    fn flag(self, key: &str) -> bool {
        match self.get(key) {
            Some(Value::Boolean(b)) => *b,
            Some(Value::Integer(i)) => i.as_signed().is_some_and(|i| i != 0),
            Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1"),
            _ => false,
        }
    }

    fn dict(self, key: &str) -> Option<Self> {
        self.get(key).and_then(Self::of)
    }

    fn array(self, key: &str) -> &'a [Value] {
        self.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
    }

    fn entries(self) -> impl Iterator<Item = (&'a str, &'a Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file under `fixtures/`; `home/` mirrors a macOS home folder.
    pub(super) fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/import/fixtures").join(path)
    }

    fn dirs_at(os: Os, home: PathBuf) -> UserDirs {
        UserDirs { os, home, app_data: None, local_app_data: None, config_home: None, data_home: None }
    }

    /// A home folder that does not exist, for predictable path expansion.
    pub(super) fn dirs() -> UserDirs {
        dirs_at(Os::Macos, PathBuf::from("/Users/ada"))
    }

    /// The fixture home folder.
    pub(super) fn fixture_dirs() -> UserDirs {
        dirs_at(Os::Macos, fixture("home"))
    }

    #[test]
    fn maps_driver_names_to_engines() {
        assert_eq!(engine_named("PostgreSQL"), Some(Engine::Postgres));
        assert_eq!(engine_named("postgres-jdbc"), Some(Engine::Postgres));
        assert_eq!(engine_named("mysql8"), Some(Engine::Mysql));
        assert_eq!(engine_named("MariaDB"), Some(Engine::Mariadb));
        assert_eq!(engine_named("MSSQL"), Some(Engine::Sqlserver));
        assert_eq!(engine_named("Mongo"), Some(Engine::Mongodb));
        assert_eq!(engine_named("sqlite.xerial"), Some(Engine::Sqlite));
        assert_eq!(engine_named("Redshift"), Some(Engine::Redshift));
        assert_eq!(engine_named("Greenplum"), Some(Engine::Greenplum));
        assert_eq!(engine_named("LibSQL"), None);
        assert_eq!(engine_named("Vertica"), Some(Engine::Vertica));
    }

    #[test]
    fn expands_home_and_key_names() {
        let d = dirs();
        assert_eq!(d.expand("~/certs/ca.pem", None), "/Users/ada/certs/ca.pem");
        assert_eq!(d.expand("$USER_HOME$/k", None), "/Users/ada/k");
        assert_eq!(d.expand("$PROJECT_DIR$/ca.pem", Some(Path::new("/p"))), "/p/ca.pem");
        assert_eq!(d.ssh_key("id_ed25519", None), "/Users/ada/.ssh/id_ed25519");
        assert_eq!(d.ssh_key("/keys/a", None), "/keys/a");
    }

    #[test]
    fn unsupported_engines_are_skipped_and_invalid_ones_flagged() {
        let mut found = Found::default();
        found.push(Raw { name: "warehouse".into(), driver: "Teradata".into(), ..Raw::default() });
        found.push(Raw { name: "no user".into(), engine: Some(Engine::Postgres), ..Raw::default() });
        let preview = found.finish(SourceApp::Tableplus);
        assert_eq!(preview.skipped, ["warehouse: Teradata is not supported."]);
        let c = &preview.connections[0];
        assert_eq!((c.connection.config.host.as_str(), c.connection.config.port), ("localhost", 5432));
        assert_eq!(c.problem.as_deref(), Some("Username is required."));
    }

    #[test]
    fn duplicates_match_engine_host_port_user_and_database() {
        let mut found = Found::default();
        let raw = |host: &str| Raw {
            name: "a".into(),
            engine: Some(Engine::Postgres),
            host: host.into(),
            username: "app".into(),
            database: "db".into(),
            ..Raw::default()
        };
        found.push(raw("DB.example.com"));
        found.push(raw("other.example.com"));
        let mut preview = found.finish(SourceApp::Dbeaver);
        let saved = SavedConnection {
            id: "1".into(),
            name: "Existing".into(),
            folder_id: None,
            environment: Environment::Local,
            favorite: false,
            config: preview.connections[0].connection.config.clone(),
            has_password: false,
            created_at: 0,
            last_used_at: None,
            ssh_secrets: Vec::new(),
        };
        preview.connections[0].connection.config.host = "db.example.com".into();
        mark_duplicates(&mut preview, &[saved]);
        assert_eq!(preview.connections[0].duplicate_of.as_deref(), Some("Existing"));
        assert_eq!(preview.connections[1].duplicate_of, None);
    }

    #[test]
    fn json_and_plist_read_alike() {
        let json = parse_tree(br#"{"a": "x", "n": "5432", "b": 1, "list": [1, null]}"#).unwrap();
        let d = Dict::of(&json).unwrap();
        assert_eq!((d.str("a"), d.port("n"), d.flag("b"), d.array("list").len()), ("x", Some(5432), true, 1));
        assert!(parse_tree(b"\x00\x01encrypted").is_none());
    }
}
