//! Opens MySQL / MariaDB connections and reads server details.

use std::time::{Duration, Instant};

use dq_core::config::{ConnConfig, SslMode};
use dq_core::result::{ServerInfo, TlsInfo};
use dq_core::{DbError, DbResult, ErrorKind};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Opts, OptsBuilder, PoolConstraints, PoolOpts, SslOpts};

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

/// Server family, detected from `VERSION()`. They differ in a few session variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Mysql,
    Mariadb,
}

impl Flavor {
    fn detect(version: &str) -> Self {
        if version.to_ascii_lowercase().contains("mariadb") { Self::Mariadb } else { Self::Mysql }
    }

    /// Statement that sets the per-statement time limit; `0` disables it.
    /// MySQL's limit applies to `SELECT` only; MariaDB's to every statement.
    pub fn timeout_statement(self, secs: u32) -> String {
        match self {
            Self::Mysql => format!("SET SESSION max_execution_time = {}", u64::from(secs) * 1000),
            Self::Mariadb => format!("SET SESSION max_statement_time = {secs}"),
        }
    }
}

/// TLS options for an SSL mode, or `None` for a plain connection.
fn ssl_opts(mode: SslMode) -> Option<SslOpts> {
    let opts = SslOpts::default();
    match mode {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => {
            Some(opts.with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true))
        }
        SslMode::VerifyCa => Some(opts.with_danger_skip_domain_validation(true)),
        SslMode::VerifyFull => Some(opts),
    }
}

fn opts(
    config: &ConnConfig,
    password: Option<&str>,
    role: Role,
    tls: Option<SslOpts>,
    setup: Vec<String>,
) -> OptsBuilder {
    OptsBuilder::default()
        .ip_or_hostname(config.host.clone())
        .tcp_port(config.port)
        .user(Some(config.username.clone()))
        .pass(password.map(str::to_owned))
        .db_name(Some(config.database.clone()).filter(|d| !d.is_empty()))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        // Loopback servers are often in containers whose socket path is not ours.
        .prefer_socket(false)
        // Grid edits check affected rows; count matched rows, not only changed ones.
        .client_found_rows(role == Role::Write)
        .ssl_opts(tls)
        .setup(setup)
}

/// Opens one connection, honouring the connect timeout and `prefer` fallback.
pub async fn open(config: &ConnConfig, password: Option<&str>, role: Role, setup: Vec<String>) -> DbResult<Conn> {
    let attempt = |tls| connect(opts(config, password, role, tls, setup.clone()).into(), config, password);
    match attempt(ssl_opts(config.ssl_mode)).await {
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

/// Session settings for read-only catalog connections.
pub fn catalog_setup(flavor: Flavor) -> Vec<String> {
    vec!["SET SESSION TRANSACTION READ ONLY".to_owned(), flavor.timeout_statement(60)]
}

/// A small pool of read-only connections for the explorer, table pages and exports.
/// `config` must be [`resolved`].
pub fn catalog_pool(config: &ConnConfig, password: Option<&str>, flavor: Flavor) -> mysql_async::Pool {
    let pool = PoolOpts::default().with_constraints(PoolConstraints::new(0, 4).expect("valid pool constraints"));
    let opts = opts(config, password, Role::Catalog, ssl_opts(config.ssl_mode), catalog_setup(flavor)).pool_opts(pool);
    mysql_async::Pool::new(opts)
}

/// Settles `prefer` to what the first connection negotiated, so later
/// connections neither retry nor silently downgrade.
pub fn resolved(config: &ConnConfig, info: &ServerInfo) -> ConnConfig {
    let ssl_mode = match config.ssl_mode {
        SslMode::Prefer if info.tls.is_some() => SslMode::Require,
        SslMode::Prefer => SslMode::Disable,
        mode => mode,
    };
    ConnConfig { ssl_mode, ..config.clone() }
}

pub struct Opened {
    pub conn: Conn,
    pub info: ServerInfo,
    pub flavor: Flavor,
}

/// Opens the query connection and reads what the UI shows about the server.
pub async fn open_query(config: &ConnConfig, password: Option<&str>) -> DbResult<Opened> {
    let started = Instant::now();
    let mut conn = open(config, password, Role::Query, Vec::new()).await?;
    let err = |e: mysql_async::Error| map_error(&e, password);
    let row: mysql_async::Row = conn
        .query_first("SELECT VERSION(), CURRENT_USER(), DATABASE(), @@global.read_only")
        .await
        .map_err(err)?
        .ok_or_else(|| DbError::internal("the server returned no version"))?;
    let tls: Option<mysql_async::Row> =
        conn.query_first("SHOW SESSION STATUS LIKE 'Ssl_version'").await.map_err(err)?;
    let tls_version = tls.and_then(|r| text(&r, 1)).filter(|v| !v.is_empty());

    let version = text(&row, 0).unwrap_or_default();
    let flavor = Flavor::detect(&version);
    let number = version.split('-').next().unwrap_or(&version);
    let product = match flavor {
        Flavor::Mysql => "MySQL",
        Flavor::Mariadb => "MariaDB",
    };
    let info = ServerInfo {
        engine: config.engine,
        version: format!("{product} {number}"),
        latency_ms: started.elapsed().as_millis() as u64,
        tls: tls_version.map(|v| TlsInfo {
            version: Some(v),
            verified: config.ssl_mode.verifies_chain(),
            mode: config.ssl_mode,
        }),
        user: text(&row, 1).unwrap_or_default(),
        database: text(&row, 2).unwrap_or_default(),
        read_only: text(&row, 3).is_some_and(|v| v == "1"),
    };
    Ok(Opened { conn, info, flavor })
}
