//! Opens MySQL / MariaDB connections and reads server details.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use dq_core::adapter::ConnectParams;
use dq_core::config::{ConnConfig, SslMode};
use dq_core::result::{ServerInfo, TlsInfo};
use dq_core::{DbError, DbResult, ErrorKind};
use mysql_async::prelude::Queryable;
use mysql_async::{ClientIdentity, Conn, DriverError, Opts, OptsBuilder, PoolConstraints, PoolOpts, SslOpts};

use crate::error::map_error;
use crate::values::text;

/// What a connection is used for. Catalog connections are read-only at the
/// session level, so table views and exports can never write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Query,
    Catalog,
    Write,
}

/// Server family, detected from `VERSION()` and `@@version_comment`. Their
/// differences are answered here rather than by branches across the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Mysql,
    Mariadb,
    /// SingleStore (formerly MemSQL) reports a MySQL `VERSION()`; its own
    /// version is `@@memsql_version`.
    Singlestore,
}

impl Flavor {
    fn detect(version: &str, comment: &str) -> Self {
        let comment = comment.to_ascii_lowercase();
        if comment.contains("singlestore") || comment.contains("memsql") {
            Self::Singlestore
        } else if version.to_ascii_lowercase().contains("mariadb") {
            Self::Mariadb
        } else {
            Self::Mysql
        }
    }

    /// Product name shown before the version number.
    pub fn product(self) -> &'static str {
        match self {
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::Singlestore => "SingleStore",
        }
    }

    /// Statement that sets the per-statement time limit; `0` disables it.
    /// MySQL's limit applies to `SELECT` only; MariaDB's to every statement.
    /// SingleStore accepts `max_execution_time` but ignores it, so its
    /// sessions time statements out themselves with `KILL QUERY`.
    pub fn timeout_statement(self, secs: u32) -> Option<String> {
        match self {
            Self::Mysql => Some(format!("SET SESSION max_execution_time = {}", u64::from(secs) * 1000)),
            Self::Mariadb => Some(format!("SET SESSION max_statement_time = {secs}")),
            Self::Singlestore => None,
        }
    }

    /// The server enforces `SET SESSION TRANSACTION READ ONLY`. SingleStore
    /// accepts it and still writes, so its sessions refuse writing statements.
    pub fn enforces_read_only(self) -> bool {
        self != Self::Singlestore
    }

    /// Databases that belong to the server, as an SQL list; hidden unless current.
    pub fn system_databases(self) -> &'static str {
        match self {
            Self::Mysql | Self::Mariadb => "('information_schema','mysql','performance_schema','sys')",
            Self::Singlestore => "('information_schema','memsql','cluster')",
        }
    }

    /// SingleStore parses `FOREIGN KEY` clauses but neither stores nor enforces
    /// them, and has no triggers or sequences: those catalog reads are skipped.
    pub fn has_foreign_keys(self) -> bool {
        self != Self::Singlestore
    }

    pub fn has_triggers(self) -> bool {
        self != Self::Singlestore
    }
}

/// TLS options for an SSL mode, or `None` for a plain connection. CA and
/// client certificate files come from the connection's advanced TLS settings.
fn ssl_opts(params: &ConnectParams, mode: SslMode) -> Option<SslOpts> {
    let files = &params.config.tls_files;
    let mut opts = SslOpts::default();
    if let Some(ca) = &files.ca_file {
        opts = opts.with_root_certs(vec![PathBuf::from(ca).into()]).with_disable_built_in_roots(true);
    }
    if let Some((cert, key)) = files.client_pair() {
        opts =
            opts.with_client_identity(Some(ClientIdentity::new(PathBuf::from(cert).into(), PathBuf::from(key).into())));
    }
    if params.endpoint.is_some() {
        // Through an SSH tunnel the socket is local; verify the database's own name.
        opts = opts.with_danger_tls_hostname_override(Some(params.config.host.clone()));
    }
    match mode {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => {
            Some(opts.with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true))
        }
        SslMode::VerifyCa => Some(opts.with_danger_skip_domain_validation(true)),
        SslMode::VerifyFull => Some(opts),
    }
}

fn opts(params: &ConnectParams, role: Role, tls: Option<SslOpts>, setup: Vec<String>) -> OptsBuilder {
    let config = &params.config;
    let (host, port) = params.socket_target();
    let keepalive = config.options.keepalive_secs;
    OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user(Some(config.username.clone()))
        .pass(params.password.clone())
        .db_name(Some(config.database.clone()).filter(|d| !d.is_empty()))
        .tcp_keepalive((keepalive > 0).then(|| Duration::from_secs(u64::from(keepalive))))
        // Loopback servers are often in containers whose socket path is not ours.
        .prefer_socket(false)
        // Grid edits check affected rows; count matched rows, not only changed ones.
        .client_found_rows(role == Role::Write)
        .ssl_opts(tls)
        .setup(setup)
}

/// `setup` for a connection of `role`: query and write connections of
/// read-only profiles only allow read-only transactions.
fn role_setup(config: &ConnConfig, role: Role, mut setup: Vec<String>) -> Vec<String> {
    if config.read_only && role != Role::Catalog {
        setup.insert(0, READ_ONLY.to_owned());
    }
    setup
}

/// Opens one connection, honouring the connect timeout and `prefer` fallback.
pub async fn open(params: &ConnectParams, role: Role, setup: Vec<String>) -> DbResult<Conn> {
    dq_core::tls::install_crypto_provider();
    let config = &params.config;
    let setup = role_setup(config, role, setup);
    let attempt = |tls| connect(opts(params, role, tls, setup.clone()).into(), config, params.password());
    match attempt(ssl_opts(params, config.ssl_mode)).await {
        // `prefer` falls back to plain text only when the server has no TLS at all.
        Err(Fallback::NoTls) if config.ssl_mode == SslMode::Prefer => attempt(None).await.map_err(Fallback::into_error),
        result => result.map_err(Fallback::into_error),
    }
}

enum Fallback {
    NoTls,
    Error(DbError),
}

impl Fallback {
    fn into_error(self) -> DbError {
        match self {
            Self::NoTls => DbError::new(ErrorKind::Tls, "The server does not support TLS.").with_action(Some(
                "The server does not accept TLS. Choose SSL mode `disable` only if this network is trusted.",
            )),
            Self::Error(e) => e,
        }
    }
}

async fn connect(opts: Opts, config: &ConnConfig, password: Option<&str>) -> Result<Conn, Fallback> {
    let timeout = Duration::from_secs(u64::from(config.connect_timeout_secs));
    match tokio::time::timeout(timeout, Conn::new(opts)).await {
        Err(_) => Err(Fallback::Error(DbError::new(
            ErrorKind::Timeout,
            format!("Timed out connecting to {}:{}.", config.host, config.port),
        ))),
        Ok(Err(mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer))) => Err(Fallback::NoTls),
        Ok(Err(e)) => Err(Fallback::Error(map_error(&e, password))),
        Ok(Ok(conn)) => Ok(conn),
    }
}

const READ_ONLY: &str = "SET SESSION TRANSACTION READ ONLY";

/// Session settings for read-only catalog connections.
pub fn catalog_setup(flavor: Flavor) -> Vec<String> {
    std::iter::once(READ_ONLY.to_owned()).chain(flavor.timeout_statement(60)).collect()
}

/// How long a pooled connection may idle before it is closed. With the
/// driver's default of zero, a pool whose minimum is 0 keeps no idle
/// connection, and every checkout would connect anew.
const POOL_IDLE: Duration = Duration::from_secs(60);

/// Up to `pool_size` connections for `role`. The driver rolls back and resets
/// each connection returned to the pool. `params` must be [`resolved`].
fn pool(params: &ConnectParams, role: Role, setup: Vec<String>) -> mysql_async::Pool {
    dq_core::tls::install_crypto_provider();
    let max = params.config.options.pool_size.max(1) as usize;
    let pool = PoolOpts::default()
        .with_constraints(PoolConstraints::new(0, max).expect("valid pool constraints"))
        .with_inactive_connection_ttl(POOL_IDLE);
    let tls = ssl_opts(params, params.config.ssl_mode);
    mysql_async::Pool::new(opts(params, role, tls, role_setup(&params.config, role, setup)).pool_opts(pool))
}

/// Read-only connections for the explorer, table pages and exports.
pub fn catalog_pool(params: &ConnectParams, flavor: Flavor) -> mysql_async::Pool {
    pool(params, Role::Catalog, catalog_setup(flavor))
}

/// Connections for grid edits, imports and schema changes.
pub fn write_pool(params: &ConnectParams) -> mysql_async::Pool {
    pool(params, Role::Write, Vec::new())
}

/// Settles `prefer` to what the first connection negotiated, so later
/// connections neither retry nor silently downgrade.
pub fn resolved(params: &ConnectParams, info: &ServerInfo) -> ConnectParams {
    let ssl_mode = match params.config.ssl_mode {
        SslMode::Prefer if info.tls.is_some() => SslMode::Require,
        SslMode::Prefer => SslMode::Disable,
        mode => mode,
    };
    ConnectParams { config: ConnConfig { ssl_mode, ..params.config.clone() }, ..params.clone() }
}

pub struct Opened {
    pub conn: Conn,
    pub info: ServerInfo,
    pub flavor: Flavor,
}

/// Opens the query connection and reads what the UI shows about the server.
pub async fn open_query(params: &ConnectParams) -> DbResult<Opened> {
    let started = Instant::now();
    let config = &params.config;
    let mut conn = open(params, Role::Query, Vec::new()).await?;
    let err = |e: mysql_async::Error| map_error(&e, params.password());
    let row: mysql_async::Row = conn
        .query_first("SELECT VERSION(), CURRENT_USER(), DATABASE(), @@global.read_only, @@version_comment")
        .await
        .map_err(err)?
        .ok_or_else(|| DbError::internal("the server returned no version"))?;
    let tls: Option<mysql_async::Row> =
        conn.query_first("SHOW SESSION STATUS LIKE 'Ssl_version'").await.map_err(err)?;
    let tls_version = tls.and_then(|r| text(&r, 1)).filter(|v| !v.is_empty());

    let mut version = text(&row, 0).unwrap_or_default();
    let flavor = Flavor::detect(&version, &text(&row, 4).unwrap_or_default());
    if flavor == Flavor::Singlestore {
        let own: Option<String> = conn.query_first("SELECT @@memsql_version").await.map_err(err)?;
        version = own.unwrap_or(version);
    }
    let number = version.split('-').next().unwrap_or(&version);
    let info = ServerInfo {
        engine: config.engine,
        version: format!("{} {number}", flavor.product()),
        latency_ms: started.elapsed().as_millis() as u64,
        tls: tls_version.map(|v| TlsInfo {
            version: Some(v),
            verified: config.ssl_mode.verifies_chain(),
            mode: config.ssl_mode,
        }),
        user: text(&row, 1).unwrap_or_default(),
        database: text(&row, 2).unwrap_or_default(),
        read_only: config.read_only || text(&row, 3).is_some_and(|v| v == "1"),
    };
    Ok(Opened { conn, info, flavor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_from_version_and_comment() {
        assert_eq!(Flavor::detect("8.4.3", "MySQL Community Server - GPL"), Flavor::Mysql);
        assert_eq!(Flavor::detect("11.4.2-MariaDB-ubu2404", "mariadb.org binary distribution"), Flavor::Mariadb);
        let s2 = "SingleStoreDB source distribution (compatible; MySQL Enterprise & MySQL Commercial)";
        assert_eq!(Flavor::detect("5.7.32", s2), Flavor::Singlestore);
        assert_eq!(Flavor::Singlestore.timeout_statement(5), None);
        assert_eq!(
            Flavor::detect("5.5.58", "MemSQL source distribution (compatible; MySQL Enterprise)"),
            Flavor::Singlestore
        );
        assert!(Flavor::Singlestore.system_databases().contains("'memsql'"));
        assert!(!Flavor::Singlestore.has_foreign_keys() && Flavor::Mysql.has_foreign_keys());
    }
}
