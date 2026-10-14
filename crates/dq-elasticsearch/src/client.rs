//! The REST client: one request per statement, JSON in and out.
//!
//! Basic authentication when a username is set, an `ApiKey` header when only
//! a password (the encoded API key) is set. Throttled requests (429) are
//! retried with backoff. Every request carries an `X-Opaque-Id` so a running
//! search can be found in the task list and cancelled.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::redact::redact_known;
use dq_core::{DbError, DbResult, ErrorKind};
use reqwest::Method;
use serde_json::Value;

const RETRIES: u32 = 4;

pub struct Client {
    http: reqwest::Client,
    base: String,
    auth: Option<String>,
    secret: Option<String>,
}

/// A response body: JSON when it parses, else text (`_cat` without `format=json`).
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Json(Value),
    Text(String),
}

impl Body {
    pub fn json(&self) -> &Value {
        match self {
            Self::Json(v) => v,
            Self::Text(_) => &Value::Null,
        }
    }
}

/// One request.
#[derive(Debug, Clone, Default)]
pub struct Call<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a str,
    /// Sent as `application/x-ndjson` (`_bulk`, `_msearch`).
    pub ndjson: bool,
    pub opaque_id: Option<&'a str>,
    /// Client-side limit; 0 means none.
    pub timeout_secs: u32,
}

impl Client {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        dq_core::tls::install_crypto_provider();
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
        let password = params.password.clone().filter(|p| !p.is_empty());
        let auth = match (config.username.trim(), &password) {
            ("", Some(key)) => Some(format!("ApiKey {key}")),
            ("", None) => None,
            (user, pw) => Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{}", pw.as_deref().unwrap_or("")))
            )),
        };
        Ok(Self {
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            base: format!("{}://{host}:{port}", if tls { "https" } else { "http" }),
            auth,
            secret: password,
        })
    }

    /// Sends a request; a non-2xx status becomes a [`DbError`].
    pub async fn call(&self, call: &Call<'_>) -> DbResult<Body> {
        let method = Method::from_bytes(call.method.as_bytes())
            .map_err(|_| DbError::invalid(format!("`{}` is not an HTTP method.", call.method)))?;
        let path = if call.path.starts_with('/') { call.path.to_owned() } else { format!("/{}", call.path) };
        let url = format!("{}{path}", self.base);
        let mut attempt = 0;
        loop {
            let mut builder = self.http.request(method.clone(), &url);
            if let Some(auth) = &self.auth {
                builder = builder.header("Authorization", auth);
            }
            if let Some(id) = call.opaque_id {
                builder = builder.header("X-Opaque-Id", id);
            }
            if call.timeout_secs > 0 {
                builder = builder.timeout(Duration::from_secs(u64::from(call.timeout_secs)));
            }
            if !call.body.is_empty() {
                let mut body = call.body.to_owned();
                let content_type = if call.ndjson {
                    if !body.ends_with('\n') {
                        body.push('\n');
                    }
                    "application/x-ndjson"
                } else {
                    "application/json"
                };
                builder = builder.header("Content-Type", content_type).body(body);
            }
            let response = builder.send().await.map_err(|e| self.network(&e, call.timeout_secs))?;
            let status = response.status().as_u16();
            let text = response.text().await.map_err(|e| self.network(&e, call.timeout_secs))?;
            if status == 429 && attempt < RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
                continue;
            }
            let body = parse_body(&text);
            if (200..300).contains(&status) || (call.method == "HEAD" && status == 404) {
                return Ok(body);
            }
            return Err(server_error(status, &body, self.secret.as_deref()));
        }
    }

    /// `GET` returning JSON.
    pub async fn get(&self, path: &str) -> DbResult<Value> {
        Ok(self.call(&Call { method: "GET", path, ..Call::default() }).await?.json().clone())
    }

    /// `POST` with a JSON body returning JSON.
    pub async fn post(&self, path: &str, body: &Value) -> DbResult<Value> {
        let body = body.to_string();
        Ok(self.call(&Call { method: "POST", path, body: &body, ..Call::default() }).await?.json().clone())
    }

    fn network(&self, e: &reqwest::Error, timeout_secs: u32) -> DbError {
        if e.is_timeout() {
            return DbError::new(
                ErrorKind::Timeout,
                format!("The request ran longer than {timeout_secs} s and was stopped waiting for."),
            )
            .with_action(Some("Narrow the query or raise the statement timeout in Settings."));
        }
        let message = redact_known(
            &format!("{e}: {}", std::error::Error::source(e).map(ToString::to_string).unwrap_or_default()),
            self.secret.as_deref().unwrap_or("\u{0}"),
        );
        let kind = if message.contains("certificate") || message.contains("tls") || message.contains("TLS") {
            ErrorKind::Tls
        } else {
            ErrorKind::Network
        };
        DbError::new(kind, message).with_action(Some("Check the host, port and that the server is reachable."))
    }
}

pub fn parse_body(text: &str) -> Body {
    serde_json::from_str(text).map_or_else(|_| Body::Text(text.to_owned()), Body::Json)
}

/// `{"error": {"type": …, "reason": …, "root_cause": […]}, "status": 400}`.
pub fn server_error(status: u16, body: &Body, secret: Option<&str>) -> DbError {
    let (kind_name, reason) = match body {
        Body::Json(v) => {
            let error = &v["error"];
            let root = &error["root_cause"][0];
            let kind = error["type"].as_str().or_else(|| root["type"].as_str()).unwrap_or("");
            // The root cause usually says more than the wrapper (`search_phase_execution_exception`).
            let reason = root["reason"]
                .as_str()
                .filter(|_| kind.ends_with("phase_execution_exception"))
                .or_else(|| error["reason"].as_str())
                .or_else(|| error.as_str())
                .or_else(|| v["message"].as_str())
                .map_or_else(|| format!("The server answered {status}."), str::to_owned);
            (kind.to_owned(), reason)
        }
        Body::Text(t) if t.trim().is_empty() => (String::new(), format!("The server answered {status}.")),
        Body::Text(t) => (String::new(), t.trim().chars().take(500).collect()),
    };
    let reason = secret.map_or_else(|| reason.clone(), |s| redact_known(&reason, s));
    let (kind, action) = match (status, kind_name.as_str()) {
        (401, _) => (ErrorKind::Auth, Some("Check the username and password or API key.")),
        (403, _) => (ErrorKind::Permission, Some("The user lacks the privilege for this request.")),
        (404, _) => (ErrorKind::NotFound, None),
        (409, _) => (ErrorKind::Constraint, None),
        (408 | 504, _) => (ErrorKind::Timeout, None),
        (429, _) => (ErrorKind::Server, Some("The cluster is overloaded; try again in a moment.")),
        (_, t) if t.contains("parsing") || t.contains("x_content") || t.contains("json_parse") => {
            (ErrorKind::Syntax, None)
        }
        (400, _) => (ErrorKind::InvalidInput, None),
        _ => (ErrorKind::Server, None),
    };
    let mut error = DbError::new(kind, reason).with_action(action);
    if !kind_name.is_empty() {
        error = error.with_code(kind_name);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_errors() {
        let body = parse_body(
            r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [x]"}],"type":"index_not_found_exception","reason":"no such index [x]"},"status":404}"#,
        );
        let e = server_error(404, &body, None);
        assert_eq!((e.kind, e.code.as_deref()), (ErrorKind::NotFound, Some("index_not_found_exception")));
        assert_eq!(e.message, "no such index [x]");
        let body = parse_body(
            r#"{"error":{"root_cause":[{"type":"query_shard_exception","reason":"failed to create query: bad"}],"type":"search_phase_execution_exception","reason":"all shards failed"},"status":400}"#,
        );
        let e = server_error(400, &body, None);
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::InvalidInput, "failed to create query: bad"));
        let e =
            server_error(400, &parse_body(r#"{"error":{"type":"parsing_exception","reason":"Unknown key"}}"#), None);
        assert_eq!(e.kind, ErrorKind::Syntax);
        assert_eq!(server_error(401, &parse_body(""), None).kind, ErrorKind::Auth);
        assert_eq!(server_error(409, &parse_body("{}"), None).kind, ErrorKind::Constraint);
    }
}
