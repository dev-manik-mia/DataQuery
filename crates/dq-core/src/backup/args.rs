//! Command lines for the dump and restore tools.
//!
//! Secrets never appear in arguments: `PostgreSQL` tools read `PGPASSWORD`,
//! `MySQL`/`MariaDB` tools a private option file (`--defaults-extra-file`) and
//! the `MongoDB` tools a private YAML config (`--config`) holding the URI.
//! Through an SSH tunnel the tools connect to the tunnel's local end; TLS
//! still verifies the server's name where the tool allows it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::tools::ToolVersion;
use super::{BackupContent, BackupFamily, BackupFormat, BackupOptions, RestoreOptions};
use crate::adapter::ConnectParams;
use crate::config::SslMode;
use crate::engine::Engine;
use crate::error::{DbError, DbResult};

/// One run of a tool.
#[derive(Debug)]
pub(crate) struct Invocation {
    pub family: BackupFamily,
    pub args: Vec<OsString>,
    /// Environment for the tool. Holds the password for `PostgreSQL`.
    pub env: Vec<(&'static str, String)>,
    /// A private option file: the flag that names it (passed first) and its contents.
    pub option_file: Option<(&'static str, String)>,
    /// A file streamed to the tool's standard input.
    pub stdin: Option<PathBuf>,
    /// Where the tool writes, polled for progress.
    pub output: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl Invocation {
    fn new(family: BackupFamily) -> Self {
        Self {
            family,
            args: Vec::new(),
            env: Vec::new(),
            option_file: None,
            stdin: None,
            output: None,
            warnings: Vec::new(),
        }
    }

    fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    fn flag(&mut self, on: bool, flag: &str) -> &mut Self {
        if on {
            self.args.push(flag.into());
        }
        self
    }

    /// `--name=value` with a path value kept as an `OsString`.
    fn path_arg(&mut self, name: &str, path: &Path) -> &mut Self {
        let mut arg = OsString::from(name);
        arg.push(path);
        self.args.push(arg);
        self
    }
}

const TUNNEL_TLS: &str = "The SSH tunnel ends on this computer, so the tool cannot check the server's certificate name; the certificate chain is still verified and the tunnel itself is encrypted.";

pub(crate) fn backup(
    family: BackupFamily,
    params: &ConnectParams,
    version: &ToolVersion,
    options: &BackupOptions,
    out: &Path,
) -> DbResult<Invocation> {
    let mut inv = Invocation::new(family);
    inv.output = Some(out.to_path_buf());
    match family {
        BackupFamily::Postgres => {
            pg_connection(&mut inv, params, version, &options.database);
            let format = match options.format {
                BackupFormat::Plain => "p",
                BackupFormat::Directory => "d",
                BackupFormat::Tar => "t",
                _ => "c",
            };
            inv.arg("--verbose").arg("--no-password").arg(format!("--format={format}")).path_arg("--file=", out);
            content_flags(&mut inv, options.content);
            for schema in &options.schemas {
                inv.arg(format!("--schema={schema}"));
            }
            for table in &options.tables {
                inv.arg(format!("--table={table}"));
            }
            // pg_dump ignores --clean for archives; pg_restore applies it instead.
            inv.flag(options.no_owner, "--no-owner")
                .flag(options.clean && options.format == BackupFormat::Plain, "--clean")
                .flag(options.clean && options.format == BackupFormat::Plain, "--if-exists");
        }
        BackupFamily::Mysql => {
            mysql_option_file(&mut inv, params, version)?;
            // SingleStore has no `WITH CONSISTENT SNAPSHOT`; its dumps are consistent per table only.
            let singlestore = params.config.engine == Engine::Singlestore;
            if singlestore && options.single_transaction {
                inv.warnings
                    .push("SingleStore has no consistent snapshot: each table is dumped as of its own moment.".into());
            }
            inv.arg("--verbose")
                .path_arg("--result-file=", out)
                .arg("--default-character-set=utf8mb4")
                .arg("--hex-blob")
                // Tablespaces need the PROCESS privilege and rarely belong in a database backup.
                .arg("--no-tablespaces")
                .flag(options.single_transaction && !singlestore, "--single-transaction")
                .flag(options.routines, "--routines")
                .arg(if options.triggers { "--triggers" } else { "--skip-triggers" })
                .flag(options.events, "--events")
                .flag(!options.clean, "--skip-add-drop-table");
            match options.content {
                BackupContent::All => {}
                BackupContent::SchemaOnly => {
                    inv.arg("--no-data");
                }
                BackupContent::DataOnly => {
                    inv.arg("--no-create-info");
                }
            }
            if !version.mariadb {
                // GTID statements need SUPER on restore; column statistics fail on older servers.
                inv.arg("--set-gtid-purged=OFF");
                inv.flag(version.major >= 8, "--skip-column-statistics");
            }
            // Without --databases the file has no CREATE DATABASE / USE, so it restores into any database.
            inv.arg(&options.database);
            for table in &options.tables {
                inv.arg(table);
            }
        }
        BackupFamily::Mongodb => {
            mongo_config(&mut inv, params);
            inv.path_arg("--archive=", out).flag(options.compress, "--gzip").arg(format!("--db={}", options.database));
            if let Some(collection) = options.tables.first() {
                inv.arg(format!("--collection={collection}"));
            }
        }
        BackupFamily::Sqlite => return Err(DbError::internal("SQLite backups run inside the engine.")),
    }
    Ok(inv)
}

pub(crate) fn restore(
    family: BackupFamily,
    params: &ConnectParams,
    version: &ToolVersion,
    options: &RestoreOptions,
    format: BackupFormat,
    gzip: bool,
    input: &Path,
) -> DbResult<Invocation> {
    let mut inv = Invocation::new(family);
    match (family, format) {
        (BackupFamily::Postgres, BackupFormat::Plain) => {
            pg_connection(&mut inv, params, version, &options.database);
            inv.arg("--no-psqlrc")
                .arg("--quiet")
                .arg("--no-password")
                .arg("--set=ON_ERROR_STOP=1")
                .flag(options.single_transaction, "--single-transaction");
            inv.stdin = Some(input.to_path_buf());
        }
        (BackupFamily::Postgres, _) => {
            pg_connection(&mut inv, params, version, &options.database);
            inv.arg("--verbose")
                .arg("--no-password")
                .flag(options.clean, "--clean")
                .flag(options.clean, "--if-exists")
                .flag(options.no_owner, "--no-owner")
                .flag(options.single_transaction, "--single-transaction")
                // Without one transaction pg_restore would go on after errors and report them at the end.
                .flag(!options.single_transaction, "--exit-on-error");
            content_flags(&mut inv, options.content);
            inv.args.push(input.as_os_str().to_owned());
        }
        (BackupFamily::Mysql, _) => {
            mysql_option_file(&mut inv, params, version)?;
            inv.arg("--default-character-set=utf8mb4").arg(format!("--database={}", options.database));
            inv.stdin = Some(input.to_path_buf());
        }
        (BackupFamily::Mongodb, _) => {
            mongo_config(&mut inv, params);
            // `--archive` without a value reads standard input, which gives byte progress.
            inv.arg("--archive")
                .flag(gzip, "--gzip")
                .flag(options.clean, "--drop")
                .arg("--nsFrom=$db$.$coll$")
                .arg(format!("--nsTo={}.$coll$", options.database));
            inv.stdin = Some(input.to_path_buf());
        }
        (BackupFamily::Sqlite, _) => return Err(DbError::internal("SQLite restores run inside the engine.")),
    }
    Ok(inv)
}

fn content_flags(inv: &mut Invocation, content: BackupContent) {
    match content {
        BackupContent::All => {}
        BackupContent::SchemaOnly => {
            inv.arg("--schema-only");
        }
        BackupContent::DataOnly => {
            inv.arg("--data-only");
        }
    }
}

/// A libpq connection-string value: single-quoted with `\` and `'` escaped.
fn conninfo_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `--dbname=<connection string>` and the environment for a `PostgreSQL` tool.
/// A connection string keeps any database name literal (a bare `--dbname`
/// containing `=` would be parsed as one).
fn pg_connection(inv: &mut Invocation, params: &ConnectParams, version: &ToolVersion, database: &str) {
    let config = &params.config;
    let mut parts = vec![("host", config.host.clone())];
    let port = match params.endpoint {
        // libpq connects to `hostaddr` and verifies the certificate against `host`.
        Some(endpoint) => {
            parts.push(("hostaddr", endpoint.ip().to_string()));
            endpoint.port()
        }
        None => config.port,
    };
    parts.push(("port", port.to_string()));
    parts.push(("user", config.username.clone()));
    parts.push(("dbname", database.to_owned()));
    parts.push(("sslmode", config.ssl_mode.as_str().to_owned()));
    if config.ssl_mode != SslMode::Disable {
        match &config.tls_files.ca_file {
            Some(ca) => parts.push(("sslrootcert", ca.clone())),
            // Like the app, trust the system's certificate store (libpq 16+).
            None if config.ssl_mode.verifies_chain() && version.major >= 16 => {
                parts.push(("sslrootcert", "system".into()));
            }
            None => {}
        }
        if let Some((cert, key)) = config.tls_files.client_pair() {
            parts.push(("sslcert", cert.to_owned()));
            parts.push(("sslkey", key.to_owned()));
        }
    }
    parts.push(("connect_timeout", config.connect_timeout_secs.to_string()));
    parts.push(("application_name", "DataQuery".into()));
    let conninfo = parts.iter().map(|(k, v)| format!("{k}={}", conninfo_value(v))).collect::<Vec<_>>().join(" ");
    inv.arg(format!("--dbname={conninfo}"));
    if let Some(password) = params.password() {
        inv.env.push(("PGPASSWORD", password.to_owned()));
    }
}

/// A value in a `MySQL` option file: quoted, with backslashes and line breaks escaped.
fn option_value(value: &str) -> DbResult<String> {
    let quote = if !value.contains('"') {
        '"'
    } else if !value.contains('\'') {
        '\''
    } else {
        return Err(DbError::invalid(
            "The password contains both kinds of quotes, which MySQL option files cannot hold. Change the password or back up with another tool.",
        ));
    };
    let escaped = value.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t");
    Ok(format!("{quote}{escaped}{quote}"))
}

/// Connection settings for `mysqldump` / `mysql` in a private option file.
/// The `MariaDB` clients name their TLS options differently.
fn mysql_option_file(inv: &mut Invocation, params: &ConnectParams, version: &ToolVersion) -> DbResult<()> {
    let config = &params.config;
    let (host, port) = params.socket_target();
    let mut lines = vec![
        "[client]".to_owned(),
        format!("host={}", option_value(&host)?),
        format!("port={port}"),
        "protocol=TCP".to_owned(),
        format!("user={}", option_value(&config.username)?),
    ];
    if let Some(password) = params.password() {
        lines.push(format!("password={}", option_value(password)?));
    }
    let mut mode = config.ssl_mode;
    if mode == SslMode::VerifyFull && params.endpoint.is_some() {
        mode = SslMode::VerifyCa;
        inv.warnings.push(TUNNEL_TLS.into());
    }
    let tls: &[&str] = match (version.mariadb, mode) {
        (false, SslMode::Disable) => &["ssl-mode=DISABLED"],
        (false, SslMode::Prefer) => &["ssl-mode=PREFERRED"],
        (false, SslMode::Require) => &["ssl-mode=REQUIRED"],
        (false, SslMode::VerifyCa) => &["ssl-mode=VERIFY_CA"],
        (false, SslMode::VerifyFull) => &["ssl-mode=VERIFY_IDENTITY"],
        (true, SslMode::Disable) => &["skip-ssl"],
        (true, SslMode::Prefer) => &["skip-ssl-verify-server-cert"],
        (true, SslMode::Require) => &["ssl", "skip-ssl-verify-server-cert"],
        // MariaDB checks the chain and the name together.
        (true, SslMode::VerifyCa) => {
            inv.warnings.push("MariaDB clients cannot check the certificate chain without its name; the connection is encrypted but not verified.".into());
            &["ssl", "skip-ssl-verify-server-cert"]
        }
        (true, SslMode::VerifyFull) => &["ssl", "ssl-verify-server-cert"],
    };
    lines.extend(tls.iter().map(|s| (*s).to_owned()));
    if mode == SslMode::Disable && !version.mariadb && version.major >= 8 {
        // Without TLS, caching_sha2_password logins need the server's RSA key.
        lines.push("get-server-public-key".into());
    }
    if mode != SslMode::Disable {
        if let Some(ca) = &config.tls_files.ca_file {
            lines.push(format!("ssl-ca={}", option_value(ca)?));
        }
        if let Some((cert, key)) = config.tls_files.client_pair() {
            lines.push(format!("ssl-cert={}", option_value(cert)?));
            lines.push(format!("ssl-key={}", option_value(key)?));
        }
    }
    inv.option_file = Some(("--defaults-extra-file=", lines.join("\n") + "\n"));
    Ok(())
}

/// Percent-encodes everything but unreserved URI characters.
pub(crate) fn uri_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The connection URI for the `MongoDB` tools, in a private `--config` file.
fn mongo_config(inv: &mut Invocation, params: &ConnectParams) {
    let config = &params.config;
    let srv = config.property("srv") == Some("true") && params.endpoint.is_none();
    let hosts = match params.endpoint {
        Some(endpoint) => endpoint.to_string(),
        None if srv => config.host.split(',').next().unwrap_or_default().trim().to_owned(),
        None => config
            .host
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| if h.contains(':') { h.to_owned() } else { format!("{h}:{}", config.port) })
            .collect::<Vec<_>>()
            .join(","),
    };
    let user = config.username.trim();
    let credentials = match (user.is_empty(), params.password()) {
        (true, _) => String::new(),
        (false, Some(password)) => format!("{}:{}@", uri_encode(user), uri_encode(password)),
        (false, None) => format!("{}@", uri_encode(user)),
    };
    let mut query: Vec<(&str, String)> = vec![("appName", "DataQuery".into())];
    if !user.is_empty() {
        query.push(("authSource", config.property("auth_source").unwrap_or("admin").to_owned()));
        if let Some(mechanism) = config.property("auth_mechanism") {
            query.push(("authMechanism", mechanism.to_owned()));
        }
    }
    if params.endpoint.is_some() {
        // A tunnel reaches exactly one member.
        query.push(("directConnection", "true".into()));
    } else {
        if let Some(rs) = config.property("replica_set") {
            query.push(("replicaSet", rs.to_owned()));
        }
        if let Some(direct) = config.property("direct_connection") {
            query.push(("directConnection", direct.to_owned()));
        }
    }
    if let Some(rp) = config.property("read_preference") {
        query.push(("readPreference", rp.to_owned()));
    }
    let timeout_ms = (u64::from(config.connect_timeout_secs) * 1000).to_string();
    query.push(("connectTimeoutMS", timeout_ms.clone()));
    query.push(("serverSelectionTimeoutMS", timeout_ms));
    match config.ssl_mode {
        SslMode::Disable => query.push(("tls", "false".into())),
        mode => {
            query.push(("tls", "true".into()));
            if matches!(mode, SslMode::Prefer | SslMode::Require) {
                query.push(("tlsInsecure", "true".into()));
            } else if params.endpoint.is_some() {
                query.push(("tlsAllowInvalidHostnames", "true".into()));
                inv.warnings.push(TUNNEL_TLS.into());
            }
            if let Some(ca) = &config.tls_files.ca_file {
                query.push(("tlsCAFile", ca.clone()));
            }
            if let Some((cert, key)) = config.tls_files.client_pair() {
                if cert != key {
                    inv.warnings.push(
                        "mongodump needs the client certificate and key in one .pem file; the key file was not used."
                            .into(),
                    );
                }
                query.push(("tlsCertificateKeyFile", cert.to_owned()));
            }
        }
    }
    let scheme = if srv { "mongodb+srv" } else { "mongodb" };
    let query = query.iter().map(|(k, v)| format!("{k}={}", uri_encode(v))).collect::<Vec<_>>().join("&");
    // Only unreserved characters and %XX escapes: safe inside a YAML double-quoted string.
    inv.option_file = Some(("--config=", format!("uri: \"{scheme}://{credentials}{hosts}/?{query}\"\n")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConnConfig;
    use crate::engine::Engine;

    fn params(engine: Engine, password: Option<&str>) -> ConnectParams {
        ConnectParams::new(
            ConnConfig {
                engine,
                host: "db.example.com".into(),
                port: 5432,
                database: "app".into(),
                username: "admin".into(),
                ssl_mode: SslMode::VerifyFull,
                ..ConnConfig::default()
            },
            password.map(str::to_owned),
        )
    }

    fn version(text: &str) -> ToolVersion {
        ToolVersion::parse(text).unwrap()
    }

    fn args(inv: &Invocation) -> Vec<String> {
        inv.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn options(format: BackupFormat) -> BackupOptions {
        BackupOptions { database: "app".into(), format, ..BackupOptions::default() }
    }

    const SECRET: &str = "s3cr\"et'\\pw";

    #[test]
    fn pg_dump_arguments() {
        let p = params(Engine::Postgres, Some(SECRET));
        let pg18 = version("pg_dump (PostgreSQL) 18.4");
        let opts = BackupOptions {
            content: BackupContent::SchemaOnly,
            schemas: vec!["sales".into()],
            tables: vec!["sales.orders".into()],
            no_owner: true,
            clean: true,
            ..options(BackupFormat::Plain)
        };
        let inv = backup(BackupFamily::Postgres, &p, &pg18, &opts, Path::new("/tmp/out.sql")).unwrap();
        let a = args(&inv);
        assert_eq!(
            a[1..],
            [
                "--verbose",
                "--no-password",
                "--format=p",
                "--file=/tmp/out.sql",
                "--schema-only",
                "--schema=sales",
                "--table=sales.orders",
                "--no-owner",
                "--clean",
                "--if-exists",
            ]
        );
        assert!(a[0].starts_with("--dbname=host='db.example.com' port='5432' user='admin' dbname='app' sslmode='verify-full' sslrootcert='system'"), "{}", a[0]);
        assert_eq!(inv.env, vec![("PGPASSWORD", SECRET.to_owned())]);
        assert!(a.iter().all(|x| !x.contains("s3cr")), "the password is never an argument");
        assert_eq!(inv.output.as_deref(), Some(Path::new("/tmp/out.sql")));

        // Archives leave --clean to pg_restore; old libpq has no system root store.
        let pg15 = version("pg_dump (PostgreSQL) 15.1");
        let inv = backup(
            BackupFamily::Postgres,
            &p,
            &pg15,
            &BackupOptions { clean: true, ..options(BackupFormat::Directory) },
            Path::new("/tmp/d"),
        )
        .unwrap();
        let a = args(&inv);
        assert!(a.contains(&"--format=d".to_owned()) && !a.contains(&"--clean".to_owned()));
        assert!(!a[0].contains("sslrootcert"));
    }

    #[test]
    fn pg_connection_strings_escape_names_and_use_the_tunnel() {
        let mut p = params(Engine::Postgres, None);
        p.endpoint = Some("127.0.0.1:40123".parse().unwrap());
        p.config.tls_files.ca_file = Some("/certs/ca.pem".into());
        p.config.tls_files.client_cert_file = Some("/certs/client.pem".into());
        let inv = backup(
            BackupFamily::Postgres,
            &p,
            &version("pg_dump 17.2"),
            &BackupOptions { database: "it's a=b\\c".into(), ..options(BackupFormat::Custom) },
            Path::new("/o"),
        )
        .unwrap();
        let dbname = args(&inv).remove(0);
        assert!(dbname.contains("host='db.example.com' hostaddr='127.0.0.1' port='40123'"), "{dbname}");
        assert!(dbname.contains(r"dbname='it\'s a=b\\c'"), "{dbname}");
        assert!(dbname.contains("sslrootcert='/certs/ca.pem' sslcert='/certs/client.pem' sslkey='/certs/client.pem'"));
        assert_eq!(inv.env, [], "no password, no PGPASSWORD");
    }

    #[test]
    fn pg_restore_and_psql_arguments() {
        let p = params(Engine::Postgres, Some("pw"));
        let v = version("pg_restore (PostgreSQL) 18.4");
        let opts =
            RestoreOptions { database: "target".into(), clean: true, no_owner: true, ..RestoreOptions::default() };
        let inv = restore(BackupFamily::Postgres, &p, &v, &opts, BackupFormat::Custom, false, Path::new("/b/app.dump"))
            .unwrap();
        assert_eq!(
            args(&inv)[1..],
            ["--verbose", "--no-password", "--clean", "--if-exists", "--no-owner", "--exit-on-error", "/b/app.dump"]
        );
        assert!(args(&inv)[0].contains("dbname='target'"));
        assert!(inv.stdin.is_none());

        let opts = RestoreOptions { single_transaction: true, ..opts };
        let inv = restore(BackupFamily::Postgres, &p, &v, &opts, BackupFormat::Plain, false, Path::new("/b/app.sql"))
            .unwrap();
        assert_eq!(
            args(&inv)[1..],
            ["--no-psqlrc", "--quiet", "--no-password", "--set=ON_ERROR_STOP=1", "--single-transaction"]
        );
        assert_eq!(inv.stdin.as_deref(), Some(Path::new("/b/app.sql")));
    }

    #[test]
    fn mysql_arguments_and_option_file() {
        let mut p = params(Engine::Mysql, Some("s3cr\"et\\pw"));
        p.config.port = 3306;
        let v = version("mysqldump  Ver 9.2.0 for macos15.2 on arm64 (Homebrew)");
        let opts = BackupOptions { tables: vec!["bk_a".into(), "bk_b".into()], ..options(BackupFormat::Plain) };
        let inv = backup(BackupFamily::Mysql, &p, &v, &opts, Path::new("/tmp/app.sql")).unwrap();
        assert_eq!(
            args(&inv),
            [
                "--verbose",
                "--result-file=/tmp/app.sql",
                "--default-character-set=utf8mb4",
                "--hex-blob",
                "--no-tablespaces",
                "--single-transaction",
                "--routines",
                "--triggers",
                "--skip-add-drop-table",
                "--set-gtid-purged=OFF",
                "--skip-column-statistics",
                "app",
                "bk_a",
                "bk_b",
            ]
        );
        let (flag, file) = inv.option_file.as_ref().unwrap();
        assert_eq!(*flag, "--defaults-extra-file=");
        assert_eq!(
            file,
            "[client]\nhost=\"db.example.com\"\nport=3306\nprotocol=TCP\nuser=\"admin\"\npassword='s3cr\"et\\\\pw'\nssl-mode=VERIFY_IDENTITY\n"
        );
        assert_eq!(inv.env, []);

        // MariaDB clients, through a tunnel, restoring.
        p.endpoint = Some("127.0.0.1:40001".parse().unwrap());
        p.password = Some("plain".into());
        let maria = version("mariadb-dump from 11.4.2-MariaDB, client 10.19 for osx10.19 (arm64)");
        let opts = RestoreOptions { database: "copy".into(), ..RestoreOptions::default() };
        let inv =
            restore(BackupFamily::Mysql, &p, &maria, &opts, BackupFormat::Plain, false, Path::new("/b.sql")).unwrap();
        assert_eq!(args(&inv), ["--default-character-set=utf8mb4", "--database=copy"]);
        let file = &inv.option_file.as_ref().unwrap().1;
        assert!(file.contains("host=\"127.0.0.1\"\nport=40001\n") && file.contains("password=\"plain\""), "{file}");
        assert!(
            file.contains("\nssl\nskip-ssl-verify-server-cert\n"),
            "verify-full degrades to verify-ca behind a tunnel: {file}"
        );
        assert_eq!(inv.warnings.len(), 2);
        assert_eq!(inv.stdin.as_deref(), Some(Path::new("/b.sql")));

        assert!(option_value("a\"b'c").is_err(), "both quote kinds cannot be written");
        assert_eq!(option_value("a\nb\\").unwrap(), "\"a\\nb\\\\\"");
    }

    #[test]
    fn mongo_arguments_and_config() {
        let mut p = params(Engine::Mongodb, Some("p@ss:w/rd"));
        p.config.host = "m1, m2:27018".into();
        p.config.port = 27017;
        p.config.ssl_mode = SslMode::Disable;
        p.config.properties.insert("replica_set".into(), "rs0".into());
        let v = version("mongodump version: 100.16.1");
        let opts = BackupOptions { tables: vec!["users".into()], ..options(BackupFormat::Archive) };
        let inv = backup(BackupFamily::Mongodb, &p, &v, &opts, Path::new("/tmp/app.archive")).unwrap();
        assert_eq!(args(&inv), ["--archive=/tmp/app.archive", "--gzip", "--db=app", "--collection=users"]);
        let (flag, config) = inv.option_file.as_ref().unwrap();
        assert_eq!(*flag, "--config=");
        assert_eq!(
            config,
            "uri: \"mongodb://admin:p%40ss%3Aw%2Frd@m1:27017,m2:27018/?appName=DataQuery&authSource=admin&replicaSet=rs0&connectTimeoutMS=10000&serverSelectionTimeoutMS=10000&tls=false\"\n"
        );

        p.endpoint = Some("127.0.0.1:40002".parse().unwrap());
        p.config.ssl_mode = SslMode::VerifyFull;
        let opts = RestoreOptions { database: "copy".into(), clean: true, ..RestoreOptions::default() };
        let inv = restore(BackupFamily::Mongodb, &p, &v, &opts, BackupFormat::Archive, true, Path::new("/a")).unwrap();
        assert_eq!(args(&inv), ["--archive", "--gzip", "--drop", "--nsFrom=$db$.$coll$", "--nsTo=copy.$coll$"]);
        let config = &inv.option_file.as_ref().unwrap().1;
        assert!(config.contains("@127.0.0.1:40002/?") && config.contains("directConnection=true"), "{config}");
        assert!(!config.contains("replicaSet") && config.contains("tls=true&tlsAllowInvalidHostnames=true"));
    }
}
