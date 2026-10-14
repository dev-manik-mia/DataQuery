//! Opens SQL Server connections and reads server details.

use std::sync::Once;
use std::time::{Duration, Instant};

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::result::{ServerInfo, TlsInfo};
use dq_core::{DbError, DbResult, ErrorKind};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::error::map_error;
use crate::rows::first_row;

pub type Conn = Client<Compat<TcpStream>>;

/// Settings every connection starts with: full-length values, and the
/// behaviour SSMS and most drivers default to.
const SESSION_SETUP: &str = "SET TEXTSIZE 2147483647; SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; \
SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET CONCAT_NULL_YIELDS_NULL ON; SET ARITHABORT ON";

/// Uses the same TLS implementation (ring) as the other adapters.
fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn config(params: &ConnectParams, database: Option<&str>) -> Config {
    let c = &params.config;
    let mut config = Config::new();
    config.host(&c.host);
    config.port(c.port);
    config.application_name("DataQuery");
    config.authentication(AuthMethod::sql_server(&c.username, params.password().unwrap_or_default()));
    if let Some(db) = database.or(Some(c.database.as_str())).filter(|d| !d.is_empty()) {
        config.database(db);
    }
    let files = &c.tls_files;
    if let Some(ca) = &files.ca_file {
        config.trust_cert_ca(ca);
    }
    if let (Some(cert), Some(key)) = (&files.client_cert_file, &files.client_key_file) {
        config.client_certificate(cert, key);
    }
    // SQL Server encrypts the login even without TLS for the session; `disable`
    // turns encryption off entirely.
    match c.ssl_mode {
        SslMode::Disable => config.encryption(EncryptionLevel::NotSupported),
        SslMode::Prefer => {
            config.encryption(EncryptionLevel::On);
            config.trust_cert();
        }
        SslMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        // The certificate chain and the host name are both checked.
        SslMode::VerifyCa | SslMode::VerifyFull => config.encryption(EncryptionLevel::Required),
    }
    config
}

/// Opens one connection to `database` (the profile's database when `None`).
/// The TDS handshake future is large, so it lives on the heap.
pub async fn open(params: &ConnectParams, database: Option<&str>) -> DbResult<Conn> {
    Box::pin(open_inner(params, database)).await
}

async fn open_inner(params: &ConnectParams, database: Option<&str>) -> DbResult<Conn> {
    install_crypto_provider();
    let c = &params.config;
    let timeout = Duration::from_secs(u64::from(c.connect_timeout_secs));
    let err = |e: tiberius::error::Error| map_error(&e, params.password());
    let timed_out = || DbError::new(ErrorKind::Timeout, format!("Timed out connecting to {}:{}.", c.host, c.port));
    let attempt = async {
        let (host, port) = params.socket_target();
        let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(|e| {
            let kind = if e.kind() == std::io::ErrorKind::TimedOut { ErrorKind::Timeout } else { ErrorKind::Network };
            DbError::new(kind, format!("Could not reach {}:{}: {e}", c.host, c.port))
        })?;
        tcp.set_nodelay(true).map_err(|e| DbError::new(ErrorKind::Network, e.to_string()))?;
        if c.options.keepalive_secs > 0 {
            let idle = Duration::from_secs(u64::from(c.options.keepalive_secs));
            let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(idle));
        }
        let mut client = Box::pin(Client::connect(config(params, database), tcp.compat_write())).await.map_err(err)?;
        client.simple_query(SESSION_SETUP).await.map_err(err)?.into_results().await.map_err(err)?;
        Ok::<_, DbError>(client)
    };
    tokio::time::timeout(timeout, attempt).await.map_err(|_| timed_out())?
}

pub struct Opened {
    pub conn: Conn,
    pub info: ServerInfo,
    pub spid: i64,
}

/// Opens the query connection and reads what the UI shows about the server.
pub async fn open_query(params: &ConnectParams) -> DbResult<Opened> {
    let started = Instant::now();
    let config = &params.config;
    let mut conn = open(params, None).await?;
    let row = first_row(
        &mut conn,
        "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
                CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), SUSER_SNAME(), DB_NAME(), @@SPID, \
                CAST(DATABASEPROPERTYEX(DB_NAME(), 'Updateability') AS nvarchar(32)), \
                (SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID)",
        &[],
        params.password(),
    )
    .await?
    .ok_or_else(|| DbError::internal("the server returned no version"))?;
    let version = row.text(0).unwrap_or_default();
    let major = version.split('.').next().and_then(|m| m.parse::<u32>().ok()).unwrap_or(0);
    let release = match major {
        17 => "2025",
        16 => "2022",
        15 => "2019",
        14 => "2017",
        13 => "2016",
        _ => "",
    };
    let edition = row.text(1).unwrap_or_default();
    let product = if edition.contains("Azure") { "Azure SQL".to_owned() } else { format!("SQL Server {release}") };
    let encrypted = row.text(6).is_some_and(|e| e.eq_ignore_ascii_case("TRUE"));
    let info = ServerInfo {
        engine: config.engine,
        version: format!("{} {version}", product.trim_end()),
        latency_ms: started.elapsed().as_millis() as u64,
        tls: encrypted.then(|| TlsInfo {
            version: None,
            verified: config.ssl_mode.verifies_chain(),
            mode: config.ssl_mode,
        }),
        user: row.text(2).unwrap_or_default(),
        database: row.text(3).unwrap_or_default(),
        read_only: config.read_only || row.text(5).is_some_and(|u| u == "READ_ONLY"),
    };
    let spid = row.number(4).unwrap_or(0);
    Ok(Opened { conn, info, spid })
}
