//! Connections: address, TLS, credentials and database from [`ConnectParams`],
//! and driver errors mapped to [`DbError`]. The password is never logged.

use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::{DbError, DbResult, ErrorKind};
use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Client, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo};

/// Catalog reads and key edits; a stuck server must not hang the explorer.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds the driver client for one server. Every connection selects `database` on connect.
pub(crate) struct Connector {
    client: Client,
    connect_timeout: Duration,
}

impl Connector {
    pub(crate) fn new(params: &ConnectParams) -> DbResult<Self> {
        dq_core::tls::install_crypto_provider();
        let config = &params.config;
        let (host, port) = params.socket_target();
        let tls = config.ssl_mode != SslMode::Disable;
        let addr = if tls {
            ConnectionAddr::TcpTls {
                host,
                port,
                // Require / Prefer: encrypted but not verified.
                insecure: matches!(config.ssl_mode, SslMode::Require | SslMode::Prefer),
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(host, port)
        };
        let mut redis = RedisConnectionInfo::default().set_lib_name("DataQuery", env!("CARGO_PKG_VERSION"));
        if !config.username.trim().is_empty() {
            redis = redis.set_username(config.username.trim());
        }
        if let Some(password) = params.password().filter(|p| !p.is_empty()) {
            redis = redis.set_password(password);
        }
        let mut info: ConnectionInfo = addr.into_connection_info().map_err(map_error)?.set_redis_settings(redis);
        // Through an SSH tunnel the socket address is local, so only the chain can be checked.
        let skip_hostname = config.ssl_mode == SslMode::VerifyCa || (tls && params.endpoint.is_some());
        let client = if tls && !config.tls_files.is_empty() {
            let read = |path: &Option<String>| -> DbResult<Option<Vec<u8>>> {
                path.as_deref()
                    .map(|p| {
                        std::fs::read(p).map_err(|e| DbError::new(ErrorKind::Tls, format!("Cannot read {p}: {e}")))
                    })
                    .transpose()
            };
            let files = &config.tls_files;
            // A combined .pem serves as both; each is parsed for its own blocks.
            let client_tls = match files.client_pair() {
                Some((cert, key)) => Some(redis::ClientTlsConfig {
                    client_cert: dq_core::tls::read_pem(cert, "client certificate")?,
                    client_key: dq_core::tls::read_pem(key, "client key")?,
                }),
                None => None,
            };
            let certs = redis::TlsCertificates { client_tls, root_cert: read(&files.ca_file)? };
            let mut client = Client::build_with_tls(info, certs).map_err(map_error)?;
            if skip_hostname {
                let mut info = client.get_connection_info().clone();
                info = with_skipped_hostname(info);
                client = Client::open(info).map_err(map_error)?;
            }
            client
        } else {
            if skip_hostname {
                info = with_skipped_hostname(info);
            }
            Client::open(info).map_err(map_error)?
        };
        Ok(Self { client, connect_timeout: Duration::from_secs(u64::from(config.connect_timeout_secs.max(1))) })
    }

    /// A connection on `database`. `timeout` bounds each reply; `None` waits as
    /// long as the command runs (the query connection, which has its own limits).
    pub(crate) async fn connect(&self, database: u32, timeout: Option<Duration>) -> DbResult<MultiplexedConnection> {
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(self.connect_timeout))
            .set_response_timeout(timeout);
        let mut conn = self.client.get_multiplexed_async_connection_with_config(&config).await.map_err(map_error)?;
        if database != 0 {
            redis::cmd("SELECT").arg(database).query_async::<()>(&mut conn).await.map_err(map_error)?;
        }
        Ok(conn)
    }

    pub(crate) async fn catalog(&self, database: u32) -> DbResult<MultiplexedConnection> {
        self.connect(database, Some(CATALOG_TIMEOUT)).await
    }
}

fn with_skipped_hostname(mut info: ConnectionInfo) -> ConnectionInfo {
    let mut addr = info.addr().clone();
    addr.set_danger_accept_invalid_hostnames(true);
    info = info.set_addr(addr);
    info
}

/// Driver and server errors as [`DbError`]s with an error kind the UI understands.
pub(crate) fn map_error(e: redis::RedisError) -> DbError {
    use redis::ErrorKind as K;
    let code = e.code().map(str::to_owned);
    let kind = match e.kind() {
        K::AuthenticationFailed => ErrorKind::Auth,
        K::Io if e.is_timeout() => ErrorKind::Timeout,
        K::Io => ErrorKind::Network,
        K::Parse | K::UnexpectedReturnType => ErrorKind::Internal,
        K::InvalidClientConfig | K::Client => ErrorKind::InvalidInput,
        _ if e.is_timeout() => ErrorKind::Timeout,
        _ if e.is_connection_refusal() || e.is_connection_dropped() => ErrorKind::Network,
        _ => match code.as_deref() {
            Some("NOAUTH" | "WRONGPASS") => ErrorKind::Auth,
            Some("NOPERM") => ErrorKind::Permission,
            Some("WRONGTYPE" | "EXECABORT") => ErrorKind::Constraint,
            Some("ERR") if e.to_string().contains("unknown command") => ErrorKind::Syntax,
            Some("ERR") if e.to_string().contains("syntax error") || e.to_string().contains("wrong number") => {
                ErrorKind::Syntax
            }
            _ => ErrorKind::Server,
        },
    };
    let message = e.detail().map_or_else(|| e.to_string(), |d| format!("{} {d}", e.code().unwrap_or("ERR")));
    let err = DbError::new(kind, message);
    let err = match kind {
        ErrorKind::Auth => err.with_action(Some("Check the username and password (or ACL user) of this connection.")),
        ErrorKind::Network => err.with_action(Some("Check that the server is running and reachable.")),
        _ => err,
    };
    match code {
        Some(c) => err.with_code(c),
        None => err,
    }
}
