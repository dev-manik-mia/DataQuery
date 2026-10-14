//! Loads the Oracle client library and opens connections.
//!
//! The driver (ODPI-C) talks to the server through Oracle's own client
//! library, loaded at run time. It is found through `DATAQUERY_ORACLE_CLIENT`,
//! the usual install folders of Oracle Instant Client, or the system library
//! path.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::result::{ServerInfo, TlsInfo};
use dq_core::{DbError, DbResult, ErrorKind};
use oracle::{Connection, Connector, InitParams};

use crate::error::map_error;
use crate::rows::first_row;

const CLIENT_HELP: &str = "Install Oracle Instant Client (Basic or Basic Light) and restart DataQuery, \
or set DATAQUERY_ORACLE_CLIENT to the folder that contains it.";

/// Folders that hold `libclntsh` in a typical Instant Client install.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var_os("DATAQUERY_ORACLE_CLIENT").map(PathBuf::from).into_iter().collect();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots: Vec<PathBuf> = home
        .iter()
        .flat_map(|h| [h.join("Downloads"), h.join("lib"), h.clone()])
        .chain(["/opt/oracle", "/usr/local/lib", "/usr/local/oracle", "/opt/homebrew/lib"].map(PathBuf::from))
        .collect();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("instantclient")))
            .collect();
        // Newest version first.
        found.sort();
        found.reverse();
        out.extend(found);
        out.push(root);
    }
    out
}

fn has_client(dir: &Path) -> bool {
    ["libclntsh.dylib", "libclntsh.so", "oci.dll"].iter().any(|f| dir.join(f).exists())
}

/// Loads the client library once per process.
fn init_client() -> DbResult<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let mut params = InitParams::new();
        if let Some(dir) = candidate_dirs().into_iter().find(|d| has_client(d)) {
            params.oracle_client_lib_dir(dir.as_os_str()).map_err(|e| e.to_string())?;
        }
        params.init().map(|_| ()).map_err(|e| e.to_string())
    })
    .clone()
    .map_err(|message| {
        DbError::new(ErrorKind::Unsupported, format!("The Oracle client library could not be loaded: {message}"))
            .with_action(Some(CLIENT_HELP))
    })
}

/// EZConnect Plus string: `tcp[s]://host:port/service?…`.
pub fn connect_string(params: &ConnectParams) -> String {
    let c = &params.config;
    let (host, port) = params.socket_target();
    let tls = !matches!(c.ssl_mode, SslMode::Disable | SslMode::Prefer);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    let mut options = vec![format!("connect_timeout={}", c.connect_timeout_secs), "retry_count=0".to_owned()];
    if c.options.keepalive_secs > 0 {
        options.push(format!("expire_time={}", (c.options.keepalive_secs / 60).max(1)));
    }
    if tls {
        // `require` encrypts; `verify-full` also matches the certificate to the host name.
        options.push(format!("ssl_server_dn_match={}", c.ssl_mode == SslMode::VerifyFull));
        if params.endpoint.is_some() {
            options.push(format!("ssl_server_cert_dn=\"CN={}\"", c.host));
        }
        if let Some(dir) = c.tls_files.ca_file.as_deref().and_then(|f| Path::new(f).parent()) {
            options.push(format!("wallet_location={}", dir.display()));
        }
    }
    format!("{}://{host}:{port}/{}?{}", if tls { "tcps" } else { "tcp" }, c.database.trim(), options.join("&"))
}

/// Session settings: text conversions that round-trip, whatever the server's defaults.
pub const SESSION_SETUP: &str = "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' \
NLS_NUMERIC_CHARACTERS = '.,'";

/// Opens one connection (blocking; call from a blocking thread).
pub fn open_blocking(params: &ConnectParams) -> DbResult<Connection> {
    init_client()?;
    let c = &params.config;
    if c.database.trim().is_empty() {
        return Err(DbError::invalid("Enter the service name (for example FREEPDB1 or ORCLPDB1)."));
    }
    let password = params.password().unwrap_or_default();
    let conn = Connector::new(c.username.as_str(), password, connect_string(params))
        .connect()
        .map_err(|e| map_error(&e, params.password()))?;
    conn.execute(SESSION_SETUP, &[]).map_err(|e| map_error(&e, params.password()))?;
    Ok(conn)
}

pub async fn open(params: &ConnectParams) -> DbResult<Connection> {
    let params = params.clone();
    tokio::task::spawn_blocking(move || open_blocking(&params))
        .await
        .map_err(|e| DbError::internal(format!("connect task failed: {e}")))?
}

pub struct Opened {
    pub conn: Connection,
    pub info: ServerInfo,
    pub sid: i64,
}

/// Opens the query connection and reads what the UI shows about the server.
pub async fn open_query(params: &ConnectParams) -> DbResult<Opened> {
    let started = std::time::Instant::now();
    let params = params.clone();
    tokio::task::spawn_blocking(move || {
        let conn = open_blocking(&params)?;
        let config = &params.config;
        let row = first_row(
            &conn,
            "SELECT SYS_CONTEXT('USERENV', 'CON_NAME'), USER, SYS_CONTEXT('USERENV', 'SID'), \
                    SYS_CONTEXT('USERENV', 'DATABASE_ROLE') FROM DUAL",
            &[],
        )?
        .ok_or_else(|| DbError::internal("the server returned no session details"))?;
        let (version, banner) = conn.server_version().map_err(|e| map_error(&e, None))?;
        // `Oracle AI Database 26ai Free Release 23.26.0.0.0 - …` → `AI Database 26ai Free`
        let product = banner.split(" Release").next().unwrap_or(&banner).trim_start_matches("Oracle ").to_owned();
        let tls = !matches!(config.ssl_mode, SslMode::Disable | SslMode::Prefer);
        let info = ServerInfo {
            engine: config.engine,
            version: format!("Oracle {version} ({product})"),
            latency_ms: started.elapsed().as_millis() as u64,
            tls: tls.then(|| TlsInfo { version: None, verified: config.ssl_mode.verifies_chain(), mode: config.ssl_mode }),
            user: row.string(1),
            database: row.string(0),
            read_only: config.read_only || row.text(3).is_some_and(|r| r.contains("STANDBY")),
        };
        Ok(Opened { conn, info, sid: row.number(2).unwrap_or(0) })
    })
    .await
    .map_err(|e| DbError::internal(format!("connect task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::config::ConnConfig;

    #[test]
    fn builds_ezconnect_strings() {
        let config = ConnConfig {
            host: "db.example.com".into(),
            port: 1521,
            database: "ORCLPDB1".into(),
            username: "app".into(),
            ssl_mode: SslMode::Disable,
            ..ConnConfig::default()
        };
        let plain = ConnectParams::new(config.clone(), None);
        assert_eq!(connect_string(&plain), "tcp://db.example.com:1521/ORCLPDB1?connect_timeout=10&retry_count=0&expire_time=1");
        let tls = ConnectParams::new(ConnConfig { ssl_mode: SslMode::VerifyFull, port: 2484, ..config }, None);
        assert!(connect_string(&tls).starts_with("tcps://db.example.com:2484/ORCLPDB1?"));
        assert!(connect_string(&tls).contains("ssl_server_dn_match=true"));
    }
}
