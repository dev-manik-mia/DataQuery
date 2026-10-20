//! Database backup and restore.
//!
//! Server engines wrap their native dump tools (`pg_dump` / `pg_restore` /
//! `psql`, `mysqldump` / `mysql` or the `MariaDB` equivalents, `mongodump` /
//! `mongorestore`); embedded engines copy pages themselves through
//! [`NativeBackup`]. Engine differences live in [`BackupFamily`] and the
//! argument builders in [`args`]; discovery is in [`tools`] and the process
//! runner (progress, cancel, cleanup) in [`run`].

mod args;
mod run;
pub mod tools;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::adapter::ConnectParams;
use crate::engine::Engine;
use crate::error::{DbError, DbResult, ErrorKind};
use crate::transfer::TransferControl;

pub use tools::{Tool, ToolStatus, ToolVersion};

/// How an engine is backed up: which tools, which file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BackupFamily {
    /// `pg_dump`, restored by `pg_restore` (archives) or `psql` (SQL).
    Postgres,
    /// `mysqldump` / `mariadb-dump`, restored by `mysql` / `mariadb`.
    Mysql,
    /// `mongodump --archive`, restored by `mongorestore`.
    Mongodb,
    /// `SQLite`'s online backup API, no external tool.
    Sqlite,
}

impl BackupFamily {
    /// The family of `engine`, or `None` when it has no backup support.
    /// `CockroachDB` speaks the `PostgreSQL` protocol but rejects `pg_dump`'s
    /// catalog queries; it has its own `BACKUP` statement.
    pub fn of(engine: Engine) -> Option<Self> {
        Some(match engine {
            Engine::Postgres => Self::Postgres,
            Engine::Mysql | Engine::Mariadb | Engine::Singlestore => Self::Mysql,
            Engine::Mongodb => Self::Mongodb,
            Engine::Sqlite => Self::Sqlite,
            _ => return None,
        })
    }

    /// Backup formats, the default first.
    pub fn formats(self) -> &'static [BackupFormat] {
        use BackupFormat as F;
        match self {
            Self::Postgres => &[F::Custom, F::Plain, F::Directory, F::Tar],
            Self::Mysql => &[F::Plain],
            Self::Mongodb => &[F::Archive],
            Self::Sqlite => &[F::File],
        }
    }

    pub fn dump_tool(self) -> Option<Tool> {
        match self {
            Self::Postgres => Some(Tool::PgDump),
            Self::Mysql => Some(Tool::Mysqldump),
            Self::Mongodb => Some(Tool::Mongodump),
            Self::Sqlite => None,
        }
    }

    /// The tool that restores files of `format`.
    pub fn restore_tool(self, format: BackupFormat) -> Option<Tool> {
        match self {
            Self::Postgres if format == BackupFormat::Plain => Some(Tool::Psql),
            Self::Postgres => Some(Tool::PgRestore),
            Self::Mysql => Some(Tool::Mysql),
            Self::Mongodb => Some(Tool::Mongorestore),
            Self::Sqlite => None,
        }
    }

    /// Every tool the family uses.
    pub fn tools(self) -> &'static [Tool] {
        match self {
            Self::Postgres => &[Tool::PgDump, Tool::PgRestore, Tool::Psql],
            Self::Mysql => &[Tool::Mysqldump, Tool::Mysql],
            Self::Mongodb => &[Tool::Mongodump, Tool::Mongorestore],
            Self::Sqlite => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BackupFormat {
    /// `pg_dump -Fc`: compressed archive, selective restore with `pg_restore`.
    Custom,
    /// A SQL script.
    Plain,
    /// `pg_dump -Fd`: a directory with one file per table.
    Directory,
    /// `pg_dump -Ft`.
    Tar,
    /// `mongodump --archive`, optionally gzip-compressed.
    Archive,
    /// A copy of the database file.
    File,
}

/// Which part of the database a backup or restore covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BackupContent {
    #[default]
    All,
    SchemaOnly,
    DataOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct BackupOptions {
    pub database: String,
    pub format: BackupFormat,
    pub content: BackupContent,
    /// Only these schemas (`PostgreSQL`); empty for all.
    pub schemas: Vec<String>,
    /// Only these tables (`schema.table` for `PostgreSQL`) or one collection; empty for all.
    pub tables: Vec<String>,
    /// Leave out ownership commands (`PostgreSQL`).
    pub no_owner: bool,
    /// Drop objects before creating them (plain SQL): `--clean --if-exists`, `DROP TABLE`.
    pub clean: bool,
    /// A consistent snapshot of `InnoDB` tables without locking (`MySQL`).
    pub single_transaction: bool,
    pub routines: bool,
    pub triggers: bool,
    pub events: bool,
    /// gzip-compress the archive (`MongoDB`).
    pub compress: bool,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            database: String::new(),
            format: BackupFormat::Custom,
            content: BackupContent::All,
            schemas: Vec::new(),
            tables: Vec::new(),
            no_owner: false,
            clean: false,
            single_transaction: true,
            routines: true,
            triggers: true,
            events: false,
            compress: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct RestoreOptions {
    /// The database to restore into.
    pub database: String,
    /// The file's format; detected from its contents when `None`.
    pub format: Option<BackupFormat>,
    pub content: BackupContent,
    /// Drop objects before recreating them (`pg_restore --clean --if-exists`, `mongorestore --drop`).
    pub clean: bool,
    pub no_owner: bool,
    /// All or nothing: one transaction (`pg_restore -1`, `psql --single-transaction`).
    pub single_transaction: bool,
}

/// Summary of a finished backup or restore.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct BackupReport {
    pub path: String,
    pub format: BackupFormat,
    /// Tables, collections or other objects the tool reported.
    #[ts(type = "number")]
    pub objects: u64,
    /// Bytes written (backup) or read (restore).
    #[ts(type = "number")]
    pub bytes: u64,
    #[ts(type = "number")]
    pub duration_ms: u64,
    /// The tool that ran, e.g. `pg_dump 18.4`; `None` for native backups.
    pub tool: Option<String>,
    /// Version mismatches and warnings the tool printed.
    pub warnings: Vec<String>,
}

/// What the backup dialog needs: formats, the tools found and warnings.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct BackupPlan {
    pub family: BackupFamily,
    pub formats: Vec<BackupFormat>,
    pub tools: Vec<ToolStatus>,
    /// E.g. a `pg_dump` older than the server.
    pub warnings: Vec<String>,
}

/// Backup inside the engine, for embedded databases (`SQLite`).
#[async_trait]
pub trait NativeBackup: Send + Sync {
    /// Copies `database` into a new file at `path`. Returns the bytes written.
    async fn backup_to(&self, database: &str, path: &Path, control: &TransferControl) -> DbResult<u64>;

    /// Replaces the contents of `database` with the database file at `path`,
    /// all or nothing. Returns the bytes read.
    async fn restore_from(&self, database: &str, path: &Path, control: &TransferControl) -> DbResult<u64>;
}

/// Tool paths the user chose in Settings, by [`Tool::id`].
pub type ToolOverrides = BTreeMap<String, String>;

/// Major and minor version of a server, from [`crate::result::ServerInfo::version`]
/// such as `PostgreSQL 17.4` or `MariaDB 11.4.2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerVersion {
    pub major: u32,
    pub minor: u32,
    pub mariadb: bool,
    /// `SingleStore`, whose version numbers are its own, not `MySQL`'s.
    pub singlestore: bool,
}

impl ServerVersion {
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = tools::version_numbers(text)?;
        Some(Self { major, minor, mariadb: text.contains("MariaDB"), singlestore: text.starts_with("SingleStore") })
    }
}

/// Why `tool` at `version` may not work with the server, if it may not.
pub fn version_warning(tool: Tool, version: &ToolVersion, server: Option<ServerVersion>) -> Option<String> {
    let server = server?;
    let name = tool.id();
    match tool {
        Tool::Mysqldump | Tool::Mysql if server.singlestore => (version.mariadb || version.major >= 9).then(|| format!(
            "SingleStore logs clients in with mysql_native_password, which {name} {} lacks. If it fails, install a MySQL 8 client (macOS: brew install mysql-client@8.4) or choose it in Settings → Engines.",
            version.text
        )),
        Tool::PgDump if version.major < server.major => Some(format!(
            "{name} {} is older than the server (PostgreSQL {}): pg_dump refuses to back up newer servers. Install the PostgreSQL {} client tools or choose a newer {name} in Settings → Engines.",
            version.text, server.major, server.major
        )),
        Tool::PgRestore | Tool::Psql if version.major < server.major => Some(format!(
            "{name} {} is older than the server (PostgreSQL {}); backups made by a newer pg_dump may not restore.",
            version.text, server.major
        )),
        Tool::Mysqldump | Tool::Mysql if version.mariadb != server.mariadb => Some(format!(
            "{name} is the {} client but the server is {}. If it fails, install the {} client tools or choose them in Settings → Engines.",
            if version.mariadb { "MariaDB" } else { "MySQL" },
            if server.mariadb { "MariaDB" } else { "MySQL" },
            if server.mariadb { "MariaDB" } else { "MySQL" },
        )),
        Tool::Mysqldump if !server.mariadb && version.major > server.major => Some(format!(
            "{name} {} is newer than the server (MySQL {}.{}) and may ask it for things it does not have. If it fails, install the MySQL {}.{} client (macOS: brew install mysql-client@{}.{}) or choose it in Settings → Engines.",
            version.text, server.major, server.minor, server.major, server.minor, server.major, server.minor
        )),
        Tool::Mysqldump if !server.mariadb && version.major < server.major => Some(format!(
            "{name} {} is older than the server (MySQL {}.{}); objects new in that version may be missing.",
            version.text, server.major, server.minor
        )),
        _ => None,
    }
}

fn family(engine: Engine) -> DbResult<BackupFamily> {
    BackupFamily::of(engine)
        .ok_or_else(|| DbError::unsupported(format!("Backup and restore are not available for {}.", engine.name())))
}

/// Formats and tools for backing up and restoring an `engine` server at `server_version`.
pub async fn plan(engine: Engine, server_version: &str, overrides: &ToolOverrides) -> DbResult<BackupPlan> {
    let family = family(engine)?;
    let server = ServerVersion::parse(server_version);
    let mut tools = Vec::new();
    let mut warnings = Vec::new();
    for &tool in family.tools() {
        let status = tools::locate(tool, engine, server, overrides.get(tool.id()).map(String::as_str)).await;
        if let Some(warning) = status.found.as_ref().and_then(|f| version_warning(tool, &f.version, server)) {
            warnings.push(warning);
        }
        tools.push(status.status);
    }
    Ok(BackupPlan { family, formats: family.formats().to_vec(), tools, warnings })
}

/// A database, schema or table name passed to a tool. Rejecting a leading `-`
/// keeps a name from being read as an option.
fn check_name(name: &str, what: &str) -> DbResult<()> {
    let ok =
        !name.trim().is_empty() && name.len() <= 512 && !name.starts_with('-') && !name.chars().any(char::is_control);
    if ok { Ok(()) } else { Err(DbError::invalid(format!("{what} `{name}` is not valid."))) }
}

impl BackupOptions {
    fn validate(&self, family: BackupFamily) -> DbResult<()> {
        if family != BackupFamily::Sqlite || !self.database.is_empty() {
            check_name(&self.database, "The database")?;
        }
        if !family.formats().contains(&self.format) {
            return Err(DbError::invalid("This format is not available for this database."));
        }
        if self.schemas.len() + self.tables.len() > 1000 {
            return Err(DbError::invalid("Choose at most 1000 schemas and tables."));
        }
        for name in &self.schemas {
            check_name(name, "The schema")?;
        }
        for name in &self.tables {
            check_name(name, "The table")?;
        }
        if family == BackupFamily::Mongodb {
            if self.tables.len() > 1 {
                return Err(DbError::invalid("mongodump backs up one collection or the whole database."));
            }
            if self.content != BackupContent::All {
                return Err(DbError::invalid("MongoDB backups always hold indexes and documents together."));
            }
        }
        Ok(())
    }
}

/// `path` with a hidden sibling name the tool writes to; renamed into place on success.
fn partial_path(path: &Path) -> DbResult<PathBuf> {
    let name = path.file_name().ok_or_else(|| DbError::invalid("Choose a file name for the backup."))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    Ok(path.with_file_name(format!(".{}.{}-{stamp:x}.partial", name.to_string_lossy(), std::process::id())))
}

fn remove_path(path: &Path) {
    let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
}

/// Total size of a file, or of the files in a directory (one level, as `pg_dump -Fd` writes).
fn disk_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path).map_or(0, |entries| {
            entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
        }),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Moves a finished backup into place. An existing file (the save dialog asked)
/// or an empty directory is replaced.
fn finish(partial: &Path, path: &Path) -> DbResult<()> {
    if path.is_dir() {
        std::fs::remove_dir(path).map_err(|e| {
            DbError::new(ErrorKind::Storage, format!("{} is a folder that is not empty: {e}", path.display()))
        })?;
    }
    std::fs::rename(partial, path)
        .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot write {}: {e}", path.display())))
}

/// Backs up a database to `path`. `native` is the engine's own backup, for
/// families that have one; the others run their dump tool.
pub async fn backup(
    params: &ConnectParams,
    server_version: &str,
    native: Option<&dyn NativeBackup>,
    options: &BackupOptions,
    path: &Path,
    overrides: &ToolOverrides,
    control: &TransferControl,
) -> DbResult<BackupReport> {
    let engine = params.config.engine;
    let family = family(engine)?;
    options.validate(family)?;
    let started = Instant::now();
    let partial = partial_path(path)?;
    let result = async {
        if family == BackupFamily::Sqlite {
            let native = native.ok_or_else(|| DbError::unsupported("This connection cannot back up itself."))?;
            let bytes = native.backup_to(&options.database, &partial, control).await?;
            return Ok(run::Outcome { objects: 0, bytes, warnings: Vec::new(), tool: None });
        }
        let tool = family.dump_tool().expect("server families have a dump tool");
        let server = ServerVersion::parse(server_version);
        let found = tools::require(tool, engine, server, overrides.get(tool.id()).map(String::as_str)).await?;
        let invocation = args::backup(family, params, &found.version, options, &partial)?;
        let mut outcome = run::run(&found, invocation, params.password(), control).await?;
        outcome.warnings.splice(0..0, version_warning(tool, &found.version, server));
        Ok(outcome)
    }
    .await;
    let outcome = match result.and_then(|o| finish(&partial, path).map(|()| o)) {
        Ok(outcome) => outcome,
        Err(e) => {
            remove_path(&partial);
            return Err(if e.kind == ErrorKind::Cancelled {
                DbError::new(ErrorKind::Cancelled, "The backup was cancelled. The partial file was removed.")
            } else {
                e
            });
        }
    };
    Ok(BackupReport {
        path: path.display().to_string(),
        format: options.format,
        objects: outcome.objects,
        bytes: disk_size(path).max(outcome.bytes),
        duration_ms: started.elapsed().as_millis() as u64,
        tool: outcome.tool,
        warnings: outcome.warnings,
    })
}

/// Restores the backup at `path` into `options.database`. Callers confirm first:
/// a restore changes or replaces existing objects.
pub async fn restore(
    params: &ConnectParams,
    server_version: &str,
    native: Option<&dyn NativeBackup>,
    options: &RestoreOptions,
    path: &Path,
    overrides: &ToolOverrides,
    control: &TransferControl,
) -> DbResult<BackupReport> {
    let engine = params.config.engine;
    let family = family(engine)?;
    if family != BackupFamily::Sqlite || !options.database.is_empty() {
        check_name(&options.database, "The database")?;
    }
    let started = Instant::now();
    let (format, gzip) = match options.format {
        Some(format) => (format, false),
        None => detect_format(path)?,
    };
    if !family.formats().contains(&format) {
        return Err(DbError::invalid(format!(
            "This file is {}; it cannot be restored into {}.",
            format.describe(),
            engine.name()
        )));
    }
    let outcome = if let Some(tool) = family.restore_tool(format) {
        let server = ServerVersion::parse(server_version);
        let found = tools::require(tool, engine, server, overrides.get(tool.id()).map(String::as_str)).await?;
        let invocation = args::restore(family, params, &found.version, options, format, gzip, path)?;
        let mut outcome = run::run(&found, invocation, params.password(), control).await;
        if let Ok(o) = &mut outcome {
            o.warnings.splice(0..0, version_warning(tool, &found.version, server));
        }
        outcome
    } else {
        let native = native.ok_or_else(|| DbError::unsupported("This connection cannot restore itself."))?;
        native.restore_from(&options.database, path, control).await.map(|bytes| run::Outcome {
            objects: 0,
            bytes,
            warnings: Vec::new(),
            tool: None,
        })
    }
    .map_err(|e| {
        if e.kind == ErrorKind::Cancelled {
            DbError::new(
                ErrorKind::Cancelled,
                "The restore was cancelled. Objects restored before that stay unless it ran in one transaction.",
            )
        } else {
            e
        }
    })?;
    Ok(BackupReport {
        path: path.display().to_string(),
        format,
        objects: outcome.objects,
        bytes: outcome.bytes.max(if path.is_file() { disk_size(path) } else { 0 }),
        duration_ms: started.elapsed().as_millis() as u64,
        tool: outcome.tool,
        warnings: outcome.warnings,
    })
}

impl BackupFormat {
    fn describe(self) -> &'static str {
        match self {
            Self::Custom => "a PostgreSQL custom-format archive",
            Self::Plain => "a SQL script",
            Self::Directory => "a PostgreSQL directory-format backup",
            Self::Tar => "a PostgreSQL tar archive",
            Self::Archive => "a MongoDB archive",
            Self::File => "an SQLite database file",
        }
    }
}

/// The format of a backup from its first bytes, and whether it is gzip-compressed.
pub fn detect_format(path: &Path) -> DbResult<(BackupFormat, bool)> {
    use std::io::Read;
    let unreadable =
        |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot read {}: {e}", path.display()));
    if path.is_dir() {
        return if path.join("toc.dat").is_file() {
            Ok((BackupFormat::Directory, false))
        } else {
            Err(DbError::invalid("This folder is not a pg_dump directory backup (it has no toc.dat)."))
        };
    }
    let mut head = Vec::with_capacity(512);
    std::fs::File::open(path).map_err(unreadable)?.take(512).read_to_end(&mut head).map_err(unreadable)?;
    Ok(if head.starts_with(b"PGDMP") {
        (BackupFormat::Custom, false)
    } else if head.starts_with(b"SQLite format 3\0") {
        (BackupFormat::File, false)
    } else if head.starts_with(&0x8199_e26d_u32.to_le_bytes()) {
        (BackupFormat::Archive, false)
    } else if head.starts_with(&[0x1f, 0x8b]) {
        // pg_dump never gzips a whole file; mongodump --gzip does.
        (BackupFormat::Archive, true)
    } else if head.get(257..262) == Some(b"ustar") {
        (BackupFormat::Tar, false)
    } else {
        (BackupFormat::Plain, false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_version(text: &str) -> ToolVersion {
        ToolVersion::parse(text).unwrap()
    }

    #[test]
    fn families_cover_supported_engines() {
        assert_eq!(BackupFamily::of(Engine::Postgres), Some(BackupFamily::Postgres));
        assert_eq!(BackupFamily::of(Engine::Mariadb), Some(BackupFamily::Mysql));
        assert_eq!(BackupFamily::of(Engine::Cockroachdb), None);
        assert_eq!(BackupFamily::of(Engine::Redis), None);
        assert_eq!(BackupFamily::Postgres.restore_tool(BackupFormat::Plain), Some(Tool::Psql));
        assert_eq!(BackupFamily::Postgres.restore_tool(BackupFormat::Custom), Some(Tool::PgRestore));
        assert_eq!(BackupFamily::Sqlite.restore_tool(BackupFormat::File), None);
    }

    #[test]
    fn version_mismatches_are_reported() {
        let pg17 = ServerVersion::parse("PostgreSQL 17.4");
        let old = tool_version("pg_dump (PostgreSQL) 16.2");
        let warning = version_warning(Tool::PgDump, &old, pg17).unwrap();
        assert!(warning.contains("16.2") && warning.contains("PostgreSQL 17"), "{warning}");
        assert!(version_warning(Tool::PgDump, &tool_version("pg_dump (PostgreSQL) 18.4 (Homebrew)"), pg17).is_none());
        assert!(version_warning(Tool::PgDump, &tool_version("pg_dump (PostgreSQL) 17.0"), pg17).is_none());
        assert!(version_warning(Tool::PgRestore, &old, pg17).is_some());
        assert!(version_warning(Tool::PgDump, &old, None).is_none(), "unknown server version");

        let maria = ServerVersion::parse("MariaDB 11.4.2");
        let mysql_tool = tool_version("mysqldump  Ver 9.2.0 for macos15.2 on arm64 (Homebrew)");
        assert!(version_warning(Tool::Mysqldump, &mysql_tool, maria).unwrap().contains("MariaDB"));
        let maria_tool = tool_version("mariadb-dump from 11.4.2-MariaDB, client 10.19 for osx10.19 (arm64)");
        assert!(version_warning(Tool::Mysqldump, &maria_tool, maria).is_none());
        let s2 = ServerVersion::parse("SingleStore 9.1.1");
        assert!(version_warning(Tool::Mysqldump, &mysql_tool, s2).unwrap().contains("mysql-client@8.4"));
        assert!(version_warning(Tool::Mysqldump, &tool_version("mysqldump  Ver 8.4.11 for macos"), s2).is_none());
        let mysql84 = ServerVersion::parse("MySQL 8.4.3");
        assert!(version_warning(Tool::Mysqldump, &mysql_tool, mysql84).unwrap().contains("mysql-client@8.4"));
        assert!(version_warning(Tool::Mysqldump, &tool_version("mysqldump  Ver 8.4.11 for macos"), mysql84).is_none());
        let old_mysql = tool_version("mysqldump  Ver 10.13 Distrib 5.7.44, for Linux (x86_64)");
        assert!(version_warning(Tool::Mysqldump, &old_mysql, mysql84).unwrap().contains("5.7"));
    }

    #[test]
    fn options_are_validated() {
        let ok = BackupOptions { database: "app".into(), ..BackupOptions::default() };
        assert!(ok.validate(BackupFamily::Postgres).is_ok());
        let bad = BackupOptions { database: "-oProxyCommand".into(), ..ok.clone() };
        assert!(bad.validate(BackupFamily::Postgres).is_err(), "option-like names are refused");
        let bad = BackupOptions { tables: vec!["a\nb".into()], ..ok.clone() };
        assert!(bad.validate(BackupFamily::Postgres).is_err());
        assert!(ok.validate(BackupFamily::Mysql).is_err(), "MySQL has no custom format");
        let mongo = BackupOptions { format: BackupFormat::Archive, tables: vec!["a".into(), "b".into()], ..ok };
        assert!(mongo.validate(BackupFamily::Mongodb).is_err());
    }

    #[test]
    fn formats_are_detected_from_contents() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        assert_eq!(detect_format(&write("a", b"PGDMP\x01\x10")).unwrap(), (BackupFormat::Custom, false));
        assert_eq!(detect_format(&write("b", b"SQLite format 3\0rest")).unwrap(), (BackupFormat::File, false));
        assert_eq!(detect_format(&write("c", &[0x6d, 0xe2, 0x99, 0x81, 1])).unwrap(), (BackupFormat::Archive, false));
        assert_eq!(detect_format(&write("d", &[0x1f, 0x8b, 8])).unwrap(), (BackupFormat::Archive, true));
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_format(&write("e", &tar)).unwrap(), (BackupFormat::Tar, false));
        assert_eq!(detect_format(&write("f.sql", b"-- dump\nCREATE TABLE")).unwrap(), (BackupFormat::Plain, false));
        let backup_dir = dir.path().join("dir");
        std::fs::create_dir(&backup_dir).unwrap();
        assert!(detect_format(&backup_dir).is_err());
        std::fs::write(backup_dir.join("toc.dat"), b"PGDMP").unwrap();
        assert_eq!(detect_format(&backup_dir).unwrap(), (BackupFormat::Directory, false));
    }

    #[test]
    fn partial_files_sit_next_to_the_target_and_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app.dump");
        let partial = partial_path(&target).unwrap();
        assert_eq!(partial.parent(), Some(dir.path()));
        assert!(partial.file_name().unwrap().to_string_lossy().starts_with(".app.dump."));
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&partial, b"new").unwrap();
        finish(&partial, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!partial.exists());
        // A directory backup replaces an empty folder chosen in the save dialog.
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let partial = partial_path(&folder).unwrap();
        std::fs::create_dir(&partial).unwrap();
        std::fs::write(partial.join("toc.dat"), b"x").unwrap();
        finish(&partial, &folder).unwrap();
        assert_eq!(disk_size(&folder), 1);
    }
}
