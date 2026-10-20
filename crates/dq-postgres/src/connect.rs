//! Opens PostgreSQL connections and reads server details.

use std::time::{Duration, Instant};

use dq_core::adapter::ConnectParams;
use dq_core::config::ConnConfig;
use dq_core::result::{ServerInfo, TlsInfo};
use dq_core::{DbError, DbResult, ErrorKind, engine::Engine};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_postgres::{AsyncMessage, Client, config::SslMode as PgSslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::error::map_error;

/// What a connection is used for. Catalog and export connections are
/// read-only at the session level, so table views and exports can never write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Query,
    Catalog,
    /// One export: read-only like a catalog connection, but without the
    /// catalog's statement timeout, since streaming a large table takes longer.
    Export,
    Write,
}

/// Server family, detected from `version()`. What differs between them is
/// answered here; catalog and monitoring queries that differ live in
/// `redshift` and the flavor branches of `monitor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    Cockroach,
    /// Amazon Redshift (provisioned and Serverless): a `PostgreSQL` 8.0 fork.
    Redshift,
    /// Greenplum and Apache Cloudberry: MPP `PostgreSQL` with segments.
    Greenplum,
}

impl Flavor {
    /// `CockroachDB CCL v25.2.1 …`, `PostgreSQL 8.0.2 on i686-pc-linux-gnu, … Redshift 1.0.77467`,
    /// `PostgreSQL 12.12 (Greenplum Database 7.0.0 build …) …`, `PostgreSQL 14.4 (Apache Cloudberry 2.0.0 build …) …`.
    pub fn detect(version: &str) -> Self {
        if version.contains("CockroachDB") {
            Self::Cockroach
        } else if version.contains("Redshift") {
            Self::Redshift
        } else if version.contains("Greenplum") || version.contains("Cloudberry") {
            Self::Greenplum
        } else {
            Self::Postgres
        }
    }

    /// The flavor a profile's engine expects, for what is decided before the
    /// server's `version()` can be read.
    pub fn expected(engine: Engine) -> Self {
        match engine {
            Engine::Cockroachdb => Self::Cockroach,
            Engine::Redshift => Self::Redshift,
            Engine::Greenplum => Self::Greenplum,
            _ => Self::Postgres,
        }
    }

    /// The database to connect to when the profile names none.
    fn default_database(self) -> &'static str {
        match self {
            Self::Cockroach => "defaultdb",
            Self::Redshift => "dev",
            Self::Postgres | Self::Greenplum => "postgres",
        }
    }

    /// Takes `-c name=value` startup options and `default_transaction_read_only`.
    /// Redshift has neither: its catalog connections set the timeout after
    /// connecting, and read-only profiles rely on DataQuery's statement check.
    fn startup_options(self) -> bool {
        self != Self::Redshift
    }

    /// Product and version for display, from the `version()` banner.
    fn version_label(self, banner: &str, server_version: Option<&str>) -> String {
        match self {
            Self::Postgres => {
                let version = server_version.unwrap_or_default();
                format!("PostgreSQL {}", version.split_whitespace().next().unwrap_or(version))
            }
            // `CockroachDB CCL v25.2.1 (aarch64-…)`
            Self::Cockroach => {
                let version = banner.split_whitespace().find(|w| w.starts_with('v')).unwrap_or_default();
                format!("CockroachDB {}", version.trim_start_matches('v'))
            }
            Self::Redshift => {
                let version = banner.rsplit("Redshift").next().unwrap_or_default().trim();
                format!("Amazon Redshift {version}").trim_end().to_owned()
            }
            // The product inside the parentheses, without `Database` and the build,
            // then the PostgreSQL it is built on (which the client tools must match).
            Self::Greenplum => {
                let inner = banner.split_once('(').map_or(banner, |(_, rest)| rest);
                let product = inner.split(" build").next().unwrap_or(inner).replace(" Database", "");
                match server_version.and_then(|v| v.split_whitespace().next()) {
                    Some(base) => format!("{} (PostgreSQL {base})", product.trim()),
                    None => product.trim().to_owned(),
                }
            }
        }
    }
}

/// The server as catalog queries see it: its family, and whether its catalog
/// is `PostgreSQL` 9.x's (Greenplum 6 is built on 9.4): no identity or
/// generated columns, declarative partitions, `prokind`, `pg_sequences`,
/// `pg_stat_ssl`, `backend_type` or `pg_blocking_pids`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Server {
    pub flavor: Flavor,
    pub legacy: bool,
}

impl Server {
    pub fn detect(banner: &str) -> Self {
        let major = banner.strip_prefix("PostgreSQL ").and_then(|v| v.split('.').next()?.parse::<u32>().ok());
        Self { flavor: Flavor::detect(banner), legacy: major.is_some_and(|m| m < 10) }
    }
}

pub struct Connection {
    pub client: Client,
    pub notices: mpsc::UnboundedReceiver<String>,
}

pub async fn open(params: &ConnectParams, database: &str, role: Role, tls: &MakeRustlsConnect) -> DbResult<Connection> {
    let config = &params.config;
    let password = params.password();
    let flavor = Flavor::expected(config.engine);
    let mut pg = tokio_postgres::Config::new();
    // With an SSH tunnel the socket goes to the tunnel while TLS still checks `host`.
    pg.host(&config.host)
        .port(params.endpoint.map_or(config.port, |a| a.port()))
        .user(&config.username)
        .dbname(if database.is_empty() { flavor.default_database() } else { database })
        .connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)))
        .keepalives(config.options.keepalive_secs > 0)
        .keepalives_idle(Duration::from_secs(u64::from(config.options.keepalive_secs.max(1))))
        .application_name(match role {
            Role::Query => "DataQuery",
            Role::Catalog => "DataQuery (catalog)",
            Role::Export => "DataQuery (export)",
            Role::Write => "DataQuery (edit)",
        })
        .ssl_mode(match config.ssl_mode {
            dq_core::config::SslMode::Disable => PgSslMode::Disable,
            dq_core::config::SslMode::Prefer => PgSslMode::Prefer,
            _ => PgSslMode::Require,
        });
    if let Some(endpoint) = params.endpoint {
        pg.hostaddr(endpoint.ip());
    }
    if flavor.startup_options() {
        if role == Role::Catalog {
            pg.options("-c default_transaction_read_only=on -c statement_timeout=60000");
        } else if role == Role::Export || config.read_only {
            // Read-only profiles: every transaction on this session is read-only.
            pg.options("-c default_transaction_read_only=on");
        }
    }
    if let Some(password) = password {
        pg.password(password);
    }

    let timeout = Duration::from_secs(u64::from(config.connect_timeout_secs) + 2);
    let (client, mut connection) = tokio::time::timeout(timeout, pg.connect(tls.clone()))
        .await
        .map_err(|_| {
            DbError::new(ErrorKind::Timeout, format!("Timed out connecting to {}:{}.", config.host, config.port))
        })?
        .map_err(|e| map_error(&e, password))?;

    // Drive the connection and forward server notices (RAISE NOTICE, warnings).
    let (tx, notices) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(message) = messages.next().await {
            match message {
                Ok(AsyncMessage::Notice(n)) => {
                    let _ = tx.send(format!("{}: {}", n.severity(), n.message()));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %dq_core::redact::redact(&e.to_string()), "postgres connection closed");
                    break;
                }
            }
        }
    });
    if role == Role::Catalog && !flavor.startup_options() {
        client.batch_execute("SET statement_timeout TO 60000").await.map_err(|e| map_error(&e, None))?;
    }
    Ok(Connection { client, notices })
}

pub async fn server_info(client: &Client, config: &ConnConfig, started: Instant) -> DbResult<(ServerInfo, Server)> {
    let err = |e: tokio_postgres::Error| map_error(&e, None);
    let banner: String = client.query_one("SELECT version()", &[]).await.map_err(err)?.get(0);
    let server = Server::detect(&banner);
    let flavor = server.flavor;
    let info = match flavor {
        Flavor::Postgres | Flavor::Greenplum => {
            let tls = if server.legacy {
                "NULL::text"
            } else {
                "(SELECT version FROM pg_stat_ssl WHERE pid = pg_backend_pid())"
            };
            let row = client
                .query_one(
                    &format!(
                        "SELECT current_setting('server_version'), current_user::text, current_database()::text, \
                                pg_is_in_recovery(), {tls}"
                    ),
                    &[],
                )
                .await
                .map_err(err)?;
            let tls_version: Option<String> = row.get(4);
            ServerInfo {
                engine: config.engine,
                version: flavor.version_label(&banner, Some(row.get(0))),
                latency_ms: started.elapsed().as_millis() as u64,
                tls: tls_version.map(|v| TlsInfo {
                    version: Some(v),
                    verified: config.ssl_mode.verifies_chain(),
                    mode: config.ssl_mode,
                }),
                user: row.get(1),
                database: row.get(2),
                read_only: row.get::<_, bool>(3) || config.read_only,
            }
        }
        // No `pg_stat_ssl`: TLS is what the profile demands.
        Flavor::Cockroach | Flavor::Redshift => {
            let row =
                client.query_one("SELECT current_user::text, current_database()::text", &[]).await.map_err(err)?;
            let tls = !matches!(config.ssl_mode, dq_core::config::SslMode::Disable | dq_core::config::SslMode::Prefer);
            ServerInfo {
                engine: config.engine,
                version: flavor.version_label(&banner, None),
                latency_ms: started.elapsed().as_millis() as u64,
                tls: tls.then(|| TlsInfo {
                    version: None,
                    verified: config.ssl_mode.verifies_chain(),
                    mode: config.ssl_mode,
                }),
                user: row.get(0),
                database: row.get(1),
                read_only: config.read_only,
            }
        }
    };
    Ok((info, server))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDSHIFT: &str = "PostgreSQL 8.0.2 on i686-pc-linux-gnu, compiled by GCC gcc (GCC) 3.4.2 20041017 (Red Hat 3.4.2-6.fc3), Redshift 1.0.77467";
    const GREENPLUM: &str = "PostgreSQL 12.12 (Greenplum Database 7.0.0 build dev) on aarch64-unknown-linux-gnu, compiled by gcc 11.4.0, 64-bit";
    const CLOUDBERRY: &str = "PostgreSQL 14.4 (Apache Cloudberry 2.0.0-incubating build 1) on x86_64-pc-linux-gnu, compiled by gcc 11.5.0, 64-bit";

    #[test]
    fn detects_and_labels_flavors() {
        assert_eq!(Flavor::detect(REDSHIFT), Flavor::Redshift);
        assert_eq!(Flavor::detect(GREENPLUM), Flavor::Greenplum);
        assert_eq!(Flavor::detect(CLOUDBERRY), Flavor::Greenplum);
        assert_eq!(Flavor::detect("CockroachDB CCL v25.2.1 (aarch64-unknown-linux-gnu)"), Flavor::Cockroach);
        assert_eq!(Flavor::detect("PostgreSQL 17.2 on aarch64-unknown-linux-musl"), Flavor::Postgres);
        let gp6 =
            Server::detect("PostgreSQL 9.4.26 (Greenplum Database 6.25.3 build dev) on aarch64-unknown-linux-gnu");
        assert_eq!(gp6, Server { flavor: Flavor::Greenplum, legacy: true });
        assert!(!Server::detect(GREENPLUM).legacy && !Server::detect(CLOUDBERRY).legacy);
        assert_eq!(Flavor::Redshift.version_label(REDSHIFT, None), "Amazon Redshift 1.0.77467");
        assert_eq!(Flavor::Greenplum.version_label(GREENPLUM, Some("12.12")), "Greenplum 7.0.0 (PostgreSQL 12.12)");
        assert_eq!(
            Flavor::Greenplum.version_label(CLOUDBERRY, Some("14.4")),
            "Apache Cloudberry 2.0.0-incubating (PostgreSQL 14.4)"
        );
        assert_eq!(Flavor::Postgres.version_label("", Some("17.2 (Debian 17.2-1)")), "PostgreSQL 17.2");
    }

    #[test]
    fn redshift_connects_without_startup_options() {
        let redshift = Flavor::expected(Engine::Redshift);
        assert!(!redshift.startup_options() && Flavor::expected(Engine::Greenplum).startup_options());
        assert_eq!(
            (redshift.default_database(), Flavor::expected(Engine::Cockroachdb).default_database()),
            ("dev", "defaultdb")
        );
    }
}
