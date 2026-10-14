//! Driver options from a connection profile, and driver errors as `DbError`s.

use std::path::PathBuf;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::{DbError, DbResult, ErrorKind};
use mongodb::Client;
use mongodb::bson::{Bson, Document};
use mongodb::options::{
    ClientOptions, Credential, ReadPreference, ReadPreferenceOptions, SelectionCriteria, ServerAddress, Tls, TlsOptions,
};

/// A connected client plus a temporary combined certificate file, kept while the client lives.
pub struct Connected {
    pub client: Client,
    _cert_key: Option<tempfile_guard::TempPem>,
}

mod tempfile_guard {
    use std::path::PathBuf;

    /// Deletes the combined client certificate file when dropped.
    pub struct TempPem(pub PathBuf);

    impl Drop for TempPem {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

fn hosts(params: &ConnectParams) -> DbResult<Vec<ServerAddress>> {
    let config = &params.config;
    if let Some(endpoint) = params.endpoint {
        return Ok(vec![ServerAddress::Tcp { host: endpoint.ip().to_string(), port: Some(endpoint.port()) }]);
    }
    config
        .host
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| {
            // `host:port`, `[v6]:port`, or a host using the profile's port.
            let (host, port) = match h.rsplit_once(':') {
                Some((host, port)) if !host.ends_with(':') && port.chars().all(|c| c.is_ascii_digit()) => {
                    (host, port.parse::<u16>().map_err(|_| DbError::invalid(format!("`{h}` has an invalid port.")))?)
                }
                _ => (h, config.port),
            };
            Ok(ServerAddress::Tcp {
                host: host.trim_start_matches('[').trim_end_matches(']').to_owned(),
                port: Some(port),
            })
        })
        .collect()
}

fn read_preference(value: &str) -> DbResult<ReadPreference> {
    let options = Some(ReadPreferenceOptions::default());
    Ok(match value.to_ascii_lowercase().as_str() {
        "primary" => ReadPreference::Primary,
        "primarypreferred" => ReadPreference::PrimaryPreferred { options },
        "secondary" => ReadPreference::Secondary { options },
        "secondarypreferred" => ReadPreference::SecondaryPreferred { options },
        "nearest" => ReadPreference::Nearest { options },
        _ => return Err(DbError::invalid(format!("Unknown read preference `{value}`."))),
    })
}

/// Builds the client. No network traffic happens until the first command.
pub async fn connect(params: &ConnectParams) -> DbResult<Connected> {
    dq_core::tls::install_crypto_provider();
    let config = &params.config;
    let timeout = Duration::from_secs(u64::from(config.connect_timeout_secs.max(1)));
    let mut options = if config.property("srv") == Some("true") && params.endpoint.is_none() {
        let host = config.host.split(',').next().unwrap_or_default().trim();
        ClientOptions::parse(format!("mongodb+srv://{host}/")).await.map_err(map_error)?
    } else {
        let mut o = ClientOptions::default();
        o.hosts = hosts(params)?;
        o
    };
    if options.hosts.is_empty() {
        return Err(DbError::invalid("Host is required."));
    }
    options.app_name = Some("DataQuery".into());
    options.connect_timeout = Some(timeout);
    options.server_selection_timeout = Some(timeout);
    options.max_pool_size = Some(config.options.pool_size.max(2) + 2);
    if let Some(rs) = config.property("replica_set") {
        options.repl_set_name = Some(rs.to_owned());
    }
    if let Some(rp) = config.property("read_preference") {
        options.selection_criteria = Some(SelectionCriteria::ReadPreference(read_preference(rp)?));
    }
    // A tunnel reaches exactly one member, so discovery is turned off.
    options.direct_connection =
        if params.endpoint.is_some() { Some(true) } else { config.property("direct_connection").map(|v| v == "true") };
    if !config.username.trim().is_empty() {
        let mechanism = config
            .property("auth_mechanism")
            .map(|m| m.parse().map_err(|_| DbError::invalid(format!("Unknown authentication mechanism `{m}`."))))
            .transpose()?;
        options.credential = Some(
            Credential::builder()
                .username(config.username.clone())
                .password(params.password.clone())
                .source(config.property("auth_source").unwrap_or("admin").to_owned())
                .mechanism(mechanism)
                .build(),
        );
    }
    let mut cert_key = None;
    options.tls = Some(match config.ssl_mode {
        SslMode::Disable => Tls::Disabled,
        mode => {
            let files = &config.tls_files;
            let cert_key_file_path = match (&files.client_cert_file, &files.client_key_file) {
                (Some(cert), Some(key)) if cert != key => {
                    let pem = combined_pem(cert, key)?;
                    let path = pem.0.clone();
                    cert_key = Some(pem);
                    Some(path)
                }
                (Some(cert), _) => Some(PathBuf::from(cert)),
                _ => None,
            };
            Tls::Enabled(
                TlsOptions::builder()
                    .allow_invalid_certificates(matches!(mode, SslMode::Prefer | SslMode::Require).then_some(true))
                    .ca_file_path(files.ca_file.as_ref().map(PathBuf::from))
                    .cert_key_file_path(cert_key_file_path)
                    .build(),
            )
        }
    });
    let client = Client::with_options(options).map_err(map_error)?;
    Ok(Connected { client, _cert_key: cert_key })
}

/// The driver wants certificate and key in one PEM file.
fn combined_pem(cert: &str, key: &str) -> DbResult<tempfile_guard::TempPem> {
    let read = |p: &str| std::fs::read(p).map_err(|e| DbError::new(ErrorKind::Tls, format!("Cannot read {p}: {e}")));
    let mut pem = read(cert)?;
    pem.push(b'\n');
    pem.extend(read(key)?);
    let path = std::env::temp_dir().join(format!("dataquery-mongo-{}.pem", uuid::Uuid::new_v4()));
    write_private(&path, &pem)
        .map_err(|e| DbError::new(ErrorKind::Tls, format!("Cannot prepare the client certificate: {e}")))?;
    Ok(tempfile_guard::TempPem(path))
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Kind of a server error code.
fn kind_of_code(code: i32) -> ErrorKind {
    match code {
        18 | 11 => ErrorKind::Auth,
        13 | 8000 => ErrorKind::Permission,
        50 | 262 | 202 => ErrorKind::Timeout,
        11601 | 11602 | 237 => ErrorKind::Cancelled,
        26 => ErrorKind::NotFound,
        11000 | 11001 | 121 | 12582 => ErrorKind::Constraint,
        2 | 9 | 14 | 15 | 17 | 40 | 52 | 72 | 166 | 168 | 40324 | 15955 | 31249 | 31254 => ErrorKind::Syntax,
        59 => ErrorKind::Unsupported,
        _ => ErrorKind::Server,
    }
}

/// A server error with its code.
pub fn server_error(code: i32, code_name: Option<&str>, message: &str) -> DbError {
    let kind = kind_of_code(code);
    let mut e = DbError::new(kind, message).with_code(code_name.map_or_else(|| code.to_string(), str::to_owned));
    if code == 121 {
        e = e.with_action(Some("The document does not match the collection's validation rules."));
    }
    if code == 20 && message.contains("Transaction numbers") {
        e = DbError::unsupported("Transactions need a replica set or sharded cluster.").with_code("20");
    }
    e
}

pub fn map_error(e: mongodb::error::Error) -> DbError {
    use mongodb::error::ErrorKind as K;
    let message = e.to_string();
    match e.kind.as_ref() {
        K::Authentication { message, .. } => DbError::new(ErrorKind::Auth, format!("Authentication failed: {message}")),
        K::Command(c) => server_error(c.code, Some(&c.code_name), &c.message),
        K::Write(mongodb::error::WriteFailure::WriteError(w)) => {
            server_error(w.code, w.code_name.as_deref(), &w.message)
        }
        K::Write(mongodb::error::WriteFailure::WriteConcernError(w)) => {
            server_error(w.code, Some(&w.code_name), &w.message)
        }
        K::Io(io) if io.to_string().to_lowercase().contains("certificate") => {
            DbError::new(ErrorKind::Tls, format!("TLS failed: {io}"))
        }
        K::Io(io) => DbError::new(ErrorKind::Network, format!("Network error: {io}")),
        K::InvalidTlsConfig { message, .. } => DbError::new(ErrorKind::Tls, message),
        K::DnsResolve { message, .. } => {
            DbError::new(ErrorKind::Network, format!("Cannot resolve the host: {message}"))
        }
        K::ServerSelection { message, .. } => {
            let kind = if message.to_lowercase().contains("certificate") || message.contains("TLS") {
                ErrorKind::Tls
            } else if message.contains("Authentication") || message.contains("auth") && message.contains("fail") {
                ErrorKind::Auth
            } else {
                ErrorKind::Network
            };
            DbError::new(kind, format!("Cannot reach a server: {message}"))
        }
        K::InvalidArgument { message, .. } | K::Transaction { message, .. } => DbError::invalid(message),
        K::SessionsNotSupported => DbError::unsupported("This server does not support sessions or transactions."),
        _ => DbError::new(ErrorKind::Server, message),
    }
}

/// Checks a command reply: `ok: 0`, `writeErrors` and `writeConcernError` become errors.
pub fn check_reply(reply: &Document) -> DbResult<()> {
    let number = |d: &Document, k: &str| match d.get(k) {
        Some(Bson::Int32(v)) => Some(i64::from(*v)),
        Some(Bson::Int64(v)) => Some(*v),
        Some(Bson::Double(v)) => Some(*v as i64),
        _ => None,
    };
    if number(reply, "ok") == Some(0) {
        return Err(server_error(
            number(reply, "code").unwrap_or(0) as i32,
            reply.get_str("codeName").ok(),
            reply.get_str("errmsg").unwrap_or("The command failed."),
        ));
    }
    if let Ok(errors) = reply.get_array("writeErrors")
        && let Some(Bson::Document(first)) = errors.first()
    {
        let more = if errors.len() > 1 { format!(" ({} write errors)", errors.len()) } else { String::new() };
        let e = server_error(
            number(first, "code").unwrap_or(0) as i32,
            first.get_str("codeName").ok(),
            &format!("{}{more}", first.get_str("errmsg").unwrap_or("A write failed.")),
        );
        return Err(e);
    }
    if let Ok(w) = reply.get_document("writeConcernError") {
        return Err(server_error(
            number(w, "code").unwrap_or(0) as i32,
            w.get_str("codeName").ok(),
            w.get_str("errmsg").unwrap_or("The write concern failed."),
        ));
    }
    Ok(())
}

/// A whole number from a reply field of any numeric type.
pub fn int_field(d: &Document, key: &str) -> Option<i64> {
    match d.get(key)? {
        Bson::Int32(v) => Some(i64::from(*v)),
        Bson::Int64(v) => Some(*v),
        Bson::Double(v) => Some(*v as i64),
        _ => None,
    }
}
