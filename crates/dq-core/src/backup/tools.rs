//! Finding the dump and restore tools and reading their versions.
//!
//! A path chosen in Settings wins. Otherwise every copy on `PATH` and in the
//! usual install directories is asked for its version, and the best one is
//! used: the client of the server's own product (`MySQL` vs `MariaDB`), then
//! the newest (`pg_dump` refuses servers newer than itself).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::engine::Engine;
use crate::error::{DbError, DbResult, ErrorKind};
use crate::programs;

use super::ServerVersion;

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Copies asked for their version; more are ignored.
const MAX_CANDIDATES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Tool {
    PgDump,
    PgRestore,
    Psql,
    /// `mysqldump`, or `mariadb-dump`.
    Mysqldump,
    /// The `mysql` or `mariadb` command-line client.
    Mysql,
    Mongodump,
    Mongorestore,
}

impl Tool {
    pub const ALL: [Self; 7] =
        [Self::PgDump, Self::PgRestore, Self::Psql, Self::Mysqldump, Self::Mysql, Self::Mongodump, Self::Mongorestore];

    /// The name used in Settings and messages.
    pub fn id(self) -> &'static str {
        match self {
            Self::PgDump => "pg_dump",
            Self::PgRestore => "pg_restore",
            Self::Psql => "psql",
            Self::Mysqldump => "mysqldump",
            Self::Mysql => "mysql",
            Self::Mongodump => "mongodump",
            Self::Mongorestore => "mongorestore",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    /// Program names to look for. `MariaDB` installs `mariadb-dump` and `mariadb`
    /// (older versions only the `mysql*` names).
    fn programs(self) -> &'static [&'static str] {
        match self {
            Self::PgDump => &["pg_dump"],
            Self::PgRestore => &["pg_restore"],
            Self::Psql => &["psql"],
            Self::Mysqldump => &["mysqldump", "mariadb-dump"],
            Self::Mysql => &["mysql", "mariadb"],
            Self::Mongodump => &["mongodump"],
            Self::Mongorestore => &["mongorestore"],
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Self::PgDump | Self::PgRestore | Self::Psql => {
                "Install the PostgreSQL client tools (macOS: brew install libpq; Windows: the PostgreSQL installer; Linux: postgresql-client), or choose the program in Settings → Engines."
            }
            Self::Mysqldump | Self::Mysql => {
                "Install the MySQL or MariaDB client (macOS: brew install mysql-client or mariadb; Linux: mysql-client or mariadb-client), or choose the program in Settings → Engines."
            }
            Self::Mongodump | Self::Mongorestore => {
                "Install the MongoDB Database Tools (macOS: brew install mongodb/brew/mongodb-database-tools), or choose the program in Settings → Engines."
            }
        }
    }
}

/// A tool's version, from its `--version` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    /// As printed, e.g. `18.4` or `11.4.2-MariaDB`.
    pub text: String,
    pub major: u32,
    pub minor: u32,
    /// The `MariaDB` build of `mysqldump` / `mysql`.
    pub mariadb: bool,
}

/// Major and minor of the first `N.N` in `text`.
pub(crate) fn version_numbers(text: &str) -> Option<(u32, u32)> {
    let token = version_token(text)?;
    let mut parts = token.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty());
    Some((parts.next()?.parse().ok()?, parts.next().and_then(|m| m.parse().ok()).unwrap_or(0)))
}

/// The version word of `--version` output. Old `MySQL` clients print their own
/// protocol version first (`Ver 10.13 Distrib 5.7.44`); `mariadb-dump` prints
/// `from 11.4.2-MariaDB, client 10.19`.
fn version_token(text: &str) -> Option<&str> {
    let after = |marker: &str| text.split_once(marker).and_then(|(_, rest)| rest.split_whitespace().next());
    after("Distrib ")
        .or_else(|| after(" from "))
        .or_else(|| {
            text.split_whitespace()
                .find(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains('.'))
                .or_else(|| text.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())))
        })
        .map(|w| w.trim_end_matches([',', ';', ')']))
}

impl ToolVersion {
    pub fn parse(output: &str) -> Option<Self> {
        let line = output.lines().find(|l| !l.trim().is_empty())?;
        let (major, minor) = version_numbers(line)?;
        Some(Self { text: version_token(line)?.to_owned(), major, minor, mariadb: line.contains("MariaDB") })
    }
}

/// A tool as shown in Settings and the backup dialog.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ToolStatus {
    pub tool: Tool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// The path was chosen in Settings.
    pub custom: bool,
    /// Why the tool cannot be used.
    pub error: Option<String>,
}

/// A usable tool.
#[derive(Debug, Clone)]
pub struct Found {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: ToolVersion,
}

impl Found {
    /// `pg_dump 18.4`, for reports.
    pub fn label(&self) -> String {
        format!("{} {}", self.tool.id(), self.version.text)
    }
}

pub struct Located {
    pub status: ToolStatus,
    pub found: Option<Found>,
}

/// Directories where the tools are usually installed, besides `PATH`.
fn install_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/opt/homebrew/opt/libpq/bin",
        "/usr/local/opt/libpq/bin",
        "/opt/homebrew/opt/mysql-client/bin",
        "/usr/local/opt/mysql-client/bin",
        "/opt/homebrew/opt/mariadb/bin",
        "/Applications/Postgres.app/Contents/Versions/latest/bin",
        "/usr/local/mysql/bin",
    ]
    .map(PathBuf::from)
    .into();
    for brew in ["/opt/homebrew/opt", "/usr/local/opt"] {
        for prefix in ["postgresql@", "libpq@", "mysql-client@", "mysql@", "mariadb@"] {
            dirs.extend(programs::versioned_dirs(brew, prefix, "bin"));
        }
    }
    dirs.extend(programs::versioned_dirs("/Applications/Postgres.app/Contents/Versions", "", "bin"));
    dirs.extend(programs::versioned_dirs("/usr/lib/postgresql", "", "bin"));
    if cfg!(windows) {
        let program_files =
            std::env::var_os("ProgramFiles").map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
        dirs.extend(programs::versioned_dirs(program_files.join("PostgreSQL"), "", "bin"));
        dirs.extend(programs::versioned_dirs(program_files.join("MySQL"), "MySQL", "bin"));
        dirs.extend(programs::versioned_dirs(&program_files, "MariaDB", "bin"));
        dirs.extend(programs::versioned_dirs(program_files.join("MongoDB").join("Tools"), "", "bin"));
    }
    dirs
}

/// Runs `path --version`.
async fn version_of(path: &Path) -> DbResult<ToolVersion> {
    let output = tokio::time::timeout(VERSION_TIMEOUT, programs::command(path).arg("--version").output())
        .await
        .map_err(|_| DbError::new(ErrorKind::Timeout, "It did not print its version within 10 seconds."))?
        .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot run it: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    ToolVersion::parse(&text).ok_or_else(|| DbError::invalid("It printed no version; is it the right program?"))
}

/// How well a copy at `v` suits the server, higher is better: the server's own
/// product (`MySQL` or `MariaDB`) first, then a version at least as new as the
/// server and closest to it (`pg_dump` refuses newer servers; a much newer
/// `mysqldump` asks older servers for things they lack), else the newest.
fn rank(tool: Tool, v: &ToolVersion, engine: Engine, server: Option<ServerVersion>) -> (bool, bool, i64) {
    let number = i64::from(v.major) * 1000 + i64::from(v.minor);
    let mariadb = server.map_or(engine == Engine::Mariadb, |s| s.mariadb);
    let Some(s) = server else { return (v.mariadb == mariadb, true, number) };
    let fits = match tool {
        // MySQL 9 clients dropped mysql_native_password, which MariaDB servers use.
        Tool::Mysqldump | Tool::Mysql if v.mariadb != s.mariadb => !s.mariadb || v.major < 9,
        Tool::Mysqldump | Tool::Mysql => (v.major, v.minor) >= (s.major, s.minor),
        Tool::PgDump | Tool::PgRestore | Tool::Psql => v.major >= s.major,
        Tool::Mongodump | Tool::Mongorestore => return (true, true, number),
    };
    (v.mariadb == mariadb, fits, if fits { -number } else { number })
}

/// Finds `tool` for `engine` at `server`: the Settings path when given, else the best copy installed.
pub async fn locate(tool: Tool, engine: Engine, server: Option<ServerVersion>, custom: Option<&str>) -> Located {
    let custom = custom.map(str::trim).filter(|p| !p.is_empty());
    let candidates: Vec<PathBuf> = if let Some(path) = custom {
        vec![PathBuf::from(path)]
    } else {
        let dirs = programs::search_path(install_dirs());
        let mut found: Vec<PathBuf> = Vec::new();
        for program in tool.programs() {
            for path in programs::find_all(&dirs, program) {
                // Homebrew links the same binary from several directories.
                let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !found.iter().any(|f| std::fs::canonicalize(f).unwrap_or_else(|_| f.clone()) == real) {
                    found.push(path);
                }
            }
        }
        found.truncate(MAX_CANDIDATES);
        found
    };
    let mut best: Option<Found> = None;
    let mut last_error = None;
    for path in candidates {
        if custom.is_some() && (!path.is_absolute() || !path.is_file()) {
            last_error = Some(format!("{} is not a file.", path.display()));
            continue;
        }
        match version_of(&path).await {
            Ok(version) => {
                let rank = |v: &ToolVersion| rank(tool, v, engine, server);
                if best.as_ref().is_none_or(|b| rank(&version) > rank(&b.version)) {
                    best = Some(Found { tool, path, version });
                }
            }
            Err(e) => last_error = Some(format!("{}: {}", path.display(), e.message)),
        }
    }
    let status = ToolStatus {
        tool,
        path: best.as_ref().map(|f| f.path.display().to_string()),
        version: best.as_ref().map(|f| f.version.text.clone()),
        custom: custom.is_some(),
        error: match &best {
            Some(_) => None,
            None => Some(last_error.unwrap_or_else(|| format!("{} was not found.", tool.id()))),
        },
    };
    Located { status, found: best }
}

/// Like [`locate`], failing with an actionable error when the tool is missing.
pub async fn require(
    tool: Tool,
    engine: Engine,
    server: Option<ServerVersion>,
    custom: Option<&str>,
) -> DbResult<Found> {
    let located = locate(tool, engine, server, custom).await;
    located.found.ok_or_else(|| {
        DbError::new(ErrorKind::NotFound, located.status.error.unwrap_or_default())
            .with_action(Some(tool.install_hint()))
    })
}

/// Status of every tool, for Settings.
pub async fn all(overrides: &super::ToolOverrides) -> Vec<ToolStatus> {
    let mut out = Vec::with_capacity(Tool::ALL.len());
    for tool in Tool::ALL {
        out.push(locate(tool, Engine::Postgres, None, overrides.get(tool.id()).map(String::as_str)).await.status);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_each_tool() {
        let v = |s: &str| ToolVersion::parse(s).unwrap();
        assert_eq!(v("pg_dump (PostgreSQL) 18.4 (Homebrew)").text, "18.4");
        assert_eq!((v("pg_restore (PostgreSQL) 16.2").major, v("psql (PostgreSQL) 9.6.24").minor), (16, 6));
        let mysql = v("mysqldump  Ver 9.2.0 for macos15.2 on arm64 (Homebrew)");
        assert_eq!((mysql.text.as_str(), mysql.major, mysql.mariadb), ("9.2.0", 9, false));
        assert_eq!(v("mysql  Ver 8.0.36 for Linux on x86_64 (MySQL Community Server - GPL)").major, 8);
        let old = v("mysqldump  Ver 10.13 Distrib 5.7.44, for Linux (x86_64)");
        assert_eq!((old.text.as_str(), old.major, old.minor), ("5.7.44", 5, 7));
        let maria = v("mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for debian-linux-gnu (x86_64)");
        assert_eq!((maria.major, maria.minor, maria.mariadb), (10, 11, true));
        let maria = v("mariadb-dump from 11.4.2-MariaDB, client 10.19 for osx10.19 (arm64)");
        assert_eq!((maria.text.as_str(), maria.major, maria.mariadb), ("11.4.2-MariaDB", 11, true));
        assert_eq!(v("mongodump version: 100.16.1\ngit version: bbe4099").text, "100.16.1");
        assert!(ToolVersion::parse("usage: foo [options]").is_none());
    }

    #[test]
    fn the_copy_closest_to_the_server_wins() {
        let v = |s: &str| ToolVersion::parse(s).unwrap();
        let best = |tool, engine, server: &str, versions: &[&str]| -> String {
            let server = ServerVersion::parse(server);
            (*versions.iter().max_by_key(|text| rank(tool, &v(text), engine, server)).unwrap()).to_owned()
        };
        let (my92, my84, my57) = (
            "mysqldump  Ver 9.2.0 for macos",
            "mysqldump  Ver 8.4.11 for macos",
            "mysqldump  Ver 10.13 Distrib 5.7.44,",
        );
        let maria = "mariadb-dump from 11.4.2-MariaDB, client 10.19";
        assert_eq!(best(Tool::Mysqldump, Engine::Mysql, "MySQL 8.4.3", &[my92, my84, my57, maria]), my84);
        assert_eq!(best(Tool::Mysqldump, Engine::Mysql, "MySQL 9.1.0", &[my92, my84]), my92);
        assert_eq!(best(Tool::Mysqldump, Engine::Mysql, "", &[my84, my92]), my92, "unknown server: newest");
        assert_eq!(best(Tool::Mysqldump, Engine::Mariadb, "MariaDB 11.4.2", &[my92, my84, maria]), maria);
        assert_eq!(
            best(Tool::Mysqldump, Engine::Mariadb, "MariaDB 11.4.2", &[my92, my84]),
            my84,
            "MySQL 9 cannot log in"
        );
        let pg = |n: &str| format!("pg_dump (PostgreSQL) {n}");
        let (pg14, pg17, pg18) = (pg("14.9"), pg("17.4"), pg("18.4"));
        let all = [pg14.as_str(), pg17.as_str(), pg18.as_str()];
        assert_eq!(best(Tool::PgDump, Engine::Postgres, "PostgreSQL 16.2", &all), pg17);
        assert_eq!(best(Tool::PgDump, Engine::Postgres, "PostgreSQL 19.0", &all), pg18, "none fits: the newest");
        let mongo = ["mongodump version: 100.9.0", "mongodump version: 100.16.1"];
        assert_eq!(best(Tool::Mongodump, Engine::Mongodb, "MongoDB 8.0.1", &mongo), mongo[1]);
    }

    #[test]
    fn tools_round_trip_their_ids() {
        for tool in Tool::ALL {
            assert_eq!(Tool::from_id(tool.id()), Some(tool));
            assert_eq!(serde_json::to_value(tool).unwrap(), tool.id(), "Settings keys match the wire names");
        }
        assert_eq!(Tool::from_id("rm"), None);
    }

    #[cfg(unix)]
    fn fake_tool(dir: &Path, name: &str, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_paths_win_and_bad_ones_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_tool(dir.path(), "pg_dump", "pg_dump (PostgreSQL) 15.1");
        let located = locate(Tool::PgDump, Engine::Postgres, None, path.to_str()).await;
        assert!(located.status.custom);
        assert_eq!(located.status.version.as_deref(), Some("15.1"));
        assert_eq!(located.found.unwrap().path, path);

        let missing = dir.path().join("nope");
        let located = locate(Tool::PgDump, Engine::Postgres, None, missing.to_str()).await;
        assert!(located.found.is_none() && located.status.error.unwrap().contains("not a file"));

        let silent = fake_tool(dir.path(), "silent", "");
        let err = require(Tool::PgDump, Engine::Postgres, None, silent.to_str()).await.unwrap_err();
        assert!(err.message.contains("no version") && err.action.unwrap().contains("Settings"), "{}", err.message);
    }
}
