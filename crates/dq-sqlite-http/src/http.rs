//! The HTTP client both services share: TLS from the connection's SSL mode,
//! SSH tunnels, a bearer token that never reaches an error message, and
//! bounded response bodies.

use std::net::SocketAddr;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::redact::redact_known;
use dq_core::result::TlsInfo;
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::Value;

/// Largest response body read. A bigger result must be narrowed with `LIMIT`.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// A reply: HTTP status and the parsed JSON body (`Null` when it was not JSON).
pub struct Reply {
    pub status: u16,
    pub body: Value,
    /// The body as text, for services that answer errors in plain text.
    pub text: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub struct Http {
    client: reqwest::Client,
    /// `https://host:port` (or `http://` without TLS), without a trailing slash.
    pub origin: String,
    token: Option<String>,
    pub tls: Option<TlsInfo>,
}

impl Http {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        let config = &params.config;
        let tls = config.ssl_mode != SslMode::Disable;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)))
            .tcp_keepalive(
                (config.options.keepalive_secs > 0)
                    .then(|| Duration::from_secs(u64::from(config.options.keepalive_secs))),
            )
            .user_agent("DataQuery")
            .use_rustls_tls();
        if let Some(endpoint) = params.endpoint {
            // Through an SSH tunnel: connect locally, verify TLS for the real host.
            builder = builder.resolve(&config.host, SocketAddr::new(endpoint.ip(), endpoint.port()));
        }
        if tls {
            match config.ssl_mode {
                SslMode::Prefer | SslMode::Require => builder = builder.danger_accept_invalid_certs(true),
                SslMode::VerifyCa => builder = builder.danger_accept_invalid_hostnames(true),
                _ => {}
            }
            if let Some(ca) = &config.tls_files.ca_file {
                let pem = std::fs::read(ca)
                    .map_err(|e| DbError::new(ErrorKind::Tls, format!("Could not read the CA file: {e}")))?;
                let cert = reqwest::Certificate::from_pem(&pem)
                    .map_err(|e| DbError::new(ErrorKind::Tls, format!("Invalid CA file: {e}")))?;
                builder = builder.add_root_certificate(cert);
            }
            if let Some(pem) = dq_core::tls::client_identity_pem(&config.tls_files)? {
                let identity = reqwest::Identity::from_pem(&pem)
                    .map_err(|e| DbError::new(ErrorKind::Tls, format!("Invalid client certificate: {e}")))?;
                builder = builder.identity(identity);
            }
        }
        let port = params.endpoint.map_or(config.port, |e| e.port());
        let host = if config.host.contains(':') { format!("[{}]", config.host) } else { config.host.clone() };
        Ok(Self {
            client: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            origin: format!("{}://{host}:{port}", if tls { "https" } else { "http" }),
            token: params.password.clone().filter(|t| !t.trim().is_empty()),
            tls: tls.then(|| TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
        })
    }

    /// Removes the token from text that may echo it.
    pub fn redact(&self, text: &str) -> String {
        self.token.as_deref().map_or_else(|| text.to_owned(), |t| redact_known(text, t))
    }

    pub async fn post(&self, url: &str, body: &Value, timeout: Option<Duration>) -> DbResult<Reply> {
        self.send(self.client.post(url).json(body), timeout).await
    }

    pub async fn get(&self, url: &str, timeout: Option<Duration>) -> DbResult<Reply> {
        self.send(self.client.get(url), timeout).await
    }

    async fn send(&self, mut request: reqwest::RequestBuilder, timeout: Option<Duration>) -> DbResult<Reply> {
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let mut response = request.send().await.map_err(|e| self.network(&e))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| self.network(&e))? {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(DbError::invalid(format!(
                    "The result is larger than {} MB. Narrow the query with LIMIT or WHERE.",
                    MAX_BODY_BYTES >> 20
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = self.redact(&String::from_utf8_lossy(&bytes));
        Ok(Reply { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), text })
    }

    fn network(&self, e: &reqwest::Error) -> DbError {
        let message =
            self.redact(&format!("{e}: {}", std::error::Error::source(e).map(ToString::to_string).unwrap_or_default()));
        let kind = if e.is_timeout() {
            ErrorKind::Timeout
        } else if message.contains("certificate") || message.contains("tls") || message.contains("TLS") {
            ErrorKind::Tls
        } else {
            ErrorKind::Network
        };
        DbError::new(kind, message)
    }
}

/// An error for a statement that ran longer than `secs`.
pub fn timed_out(secs: u64) -> DbError {
    DbError::new(ErrorKind::Timeout, format!("The statement ran longer than {secs} s and was stopped."))
        .with_action(Some("Narrow the query or raise the statement timeout in Settings."))
}
