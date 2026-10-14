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

/// What a connection is used for. Catalog connections are read-only at the
/// session level, so table views and exports can never write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Query,
    Catalog,
    Write,
}

/// Server family, detected from `version()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    Cockroach,
}

impl Flavor {
    pub fn detect(version: &str) -> Self {
        if version.contains("CockroachDB") { Self::Cockroach } else { Self::Postgres }
    }
}

/// The database to connect to when the profile names none.
fn default_database(engine: Engine) -> &'static str {
    if engine == Engine::Cockroachdb { "defaultdb" } else { "postgres" }
}

pub struct Connection {
    pub client: Client,
    pub notices: mpsc::UnboundedReceiver<String>,
}

pub async fn open(params: &ConnectParams, database: &str, role: Role, tls: &MakeRustlsConnect) -> DbResult<Connection> {
    let config = &params.config;
    let password = params.password();
    let mut pg = tokio_postgres::Config::new();
    // With an SSH tunnel the socket goes to the tunnel while TLS still checks `host`.
    pg.host(&config.host)
        .port(params.endpoint.map_or(config.port, |a| a.port()))
        .user(&config.username)
        .dbname(if database.is_empty() { default_database(config.engine) } else { database })
        .connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)))
        .keepalives(config.options.keepalive_secs > 0)
        .keepalives_idle(Duration::from_secs(u64::from(config.options.keepalive_secs.max(1))))
        .application_name(match role {
            Role::Query => "DataQuery",
            Role::Catalog => "DataQuery (catalog)",
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
    if role == Role::Catalog {
        pg.options("-c default_transaction_read_only=on -c statement_timeout=60000");
    } else if config.read_only {
        // Read-only profiles: every transaction on this session is read-only.
        pg.options("-c default_transaction_read_only=on");
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
    Ok(Connection { client, notices })
}

pub async fn server_info(client: &Client, config: &ConnConfig, started: Instant) -> DbResult<(ServerInfo, Flavor)> {
    let err = |e: tokio_postgres::Error| map_error(&e, None);
    let banner: String = client.query_one("SELECT version()", &[]).await.map_err(err)?.get(0);
    let flavor = Flavor::detect(&banner);
    let info = match flavor {
        Flavor::Postgres => {
            let row = client
                .query_one(
                    "SELECT current_setting('server_version'), current_user::text, current_database()::text, \
                            pg_is_in_recovery(), (SELECT version FROM pg_stat_ssl WHERE pid = pg_backend_pid())",
                    &[],
                )
                .await
                .map_err(err)?;
            let version: String = row.get(0);
            let tls_version: Option<String> = row.get(4);
            ServerInfo {
                engine: config.engine,
                version: format!("PostgreSQL {}", version.split_whitespace().next().unwrap_or(&version)),
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
        Flavor::Cockroach => {
            let row =
                client.query_one("SELECT current_user::text, current_database()::text", &[]).await.map_err(err)?;
            // `CockroachDB CCL v25.2.1 (aarch64-…)`
            let version = banner.split_whitespace().find(|w| w.starts_with('v')).unwrap_or_default();
            let tls = !matches!(config.ssl_mode, dq_core::config::SslMode::Disable | dq_core::config::SslMode::Prefer);
            ServerInfo {
                engine: config.engine,
                version: format!("CockroachDB {}", version.trim_start_matches('v')),
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
    Ok((info, flavor))
}
