//! The `ClickHouse` HTTP interface: one request per statement, results as
//! `JSONCompactEachRowWithNamesAndTypes` lines read as they arrive.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::redact::redact_known;
use dq_core::{DbError, DbResult, ErrorKind};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;

/// Settings every request carries: exact numbers, and a stopped query when
/// the client goes away.
const BASE_SETTINGS: &[(&str, &str)] = &[
    ("default_format", "JSONCompactEachRowWithNamesAndTypes"),
    ("output_format_json_quote_64bit_integers", "1"),
    ("output_format_json_quote_decimals", "1"),
    ("output_format_json_quote_denormals", "1"),
    ("cancel_http_readonly_queries_on_client_close", "1"),
    ("date_time_output_format", "simple"),
];

pub struct Client {
    http: reqwest::Client,
    base: String,
    user: String,
    password: Option<String>,
}

/// A request's options beyond the SQL text.
#[derive(Default)]
pub struct Request<'a> {
    pub database: Option<&'a str>,
    /// Server-side session that keeps `SET`, `USE` and temporary tables.
    pub session_id: Option<&'a str>,
    pub query_id: Option<&'a str>,
    /// `readonly = 2`: reads and settings only, enforced by the server.
    pub read_only: bool,
    pub timeout_secs: u32,
    /// Values for `{name:Type}` parameters.
    pub params: Vec<(String, Option<String>)>,
}

impl Client {
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
        let port =
            if params.endpoint.is_some() { params.endpoint.map_or(config.port, |e| e.port()) } else { config.port };
        let host = if config.host.contains(':') { format!("[{}]", config.host) } else { config.host.clone() };
        Ok(Self {
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            base: format!("{}://{host}:{port}/", if tls { "https" } else { "http" }),
            user: config.username.clone(),
            password: params.password.clone(),
        })
    }

    fn secret(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Sends `sql`; the response body has not been read yet.
    pub async fn send(&self, sql: &str, request: &Request<'_>) -> DbResult<reqwest::Response> {
        let mut query: Vec<(String, String)> =
            BASE_SETTINGS.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        if let Some(db) = request.database.filter(|d| !d.is_empty()) {
            query.push(("database".into(), db.to_owned()));
        }
        if let Some(id) = request.session_id {
            query.push(("session_id".into(), id.to_owned()));
            query.push(("session_timeout".into(), "3600".into()));
        }
        if let Some(id) = request.query_id {
            query.push(("query_id".into(), id.to_owned()));
        }
        if request.read_only {
            query.push(("readonly".into(), "2".into()));
        }
        if request.timeout_secs > 0 {
            query.push(("max_execution_time".into(), request.timeout_secs.to_string()));
        }
        for (name, value) in &request.params {
            // `\N` is NULL in the parameter text format.
            query.push((format!("param_{name}"), value.clone().unwrap_or_else(|| "\\N".into())));
        }
        let mut builder =
            self.http.post(&self.base).query(&query).header("X-ClickHouse-User", &self.user).body(sql.to_owned());
        if let Some(password) = &self.password {
            builder = builder.header("X-ClickHouse-Key", password);
        }
        let response = builder.send().await.map_err(|e| self.network(&e))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let code = response
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(server_error(code, &body, status.as_u16(), self.secret()))
    }

    fn network(&self, e: &reqwest::Error) -> DbError {
        let message = redact_known(
            &format!("{e}: {}", std::error::Error::source(e).map(ToString::to_string).unwrap_or_default()),
            self.secret().unwrap_or("\u{0}"),
        );
        let kind = if e.is_timeout() {
            ErrorKind::Timeout
        } else if message.contains("certificate") || message.contains("tls") || message.contains("TLS") {
            ErrorKind::Tls
        } else {
            ErrorKind::Network
        };
        DbError::new(kind, message)
    }

    /// Runs a statement and returns its rows as text (catalog and monitoring queries).
    pub async fn text_rows(&self, sql: &str, request: &Request<'_>) -> DbResult<Vec<Vec<Option<String>>>> {
        let mut result = Rows::open(self.send(sql, request).await?, self.secret()).await?;
        let mut out = Vec::new();
        while let Some(row) = result.next().await? {
            out.push(
                row.into_iter()
                    .map(|v| match v {
                        Value::Null => None,
                        Value::String(s) => Some(s),
                        other => Some(other.to_string()),
                    })
                    .collect(),
            );
        }
        Ok(out)
    }
}

/// `Code: 60. DB::Exception: Unknown table … (UNKNOWN_TABLE) (version 25.8…)`
pub fn server_error(code: Option<u32>, body: &str, status: u16, secret: Option<&str>) -> DbError {
    let text = body.trim();
    let code =
        code.or_else(|| text.strip_prefix("Code: ").and_then(|r| r.split('.').next()).and_then(|c| c.parse().ok()));
    let message = text.split_once("DB::Exception: ").map_or(text, |(_, m)| m);
    // Drop the trailing `(version …)`.
    let message = message.rsplit_once(" (version ").map_or(message, |(m, _)| m).trim();
    let message = secret.map_or_else(|| message.to_owned(), |p| redact_known(message, p));
    let (kind, action) = match code {
        Some(516 | 192 | 193 | 194) => {
            (ErrorKind::Auth, Some("Check the username and password. The saved password was not changed."))
        }
        Some(81) => (ErrorKind::NotFound, Some("Check the database name.")),
        Some(60 | 46) => (ErrorKind::NotFound, Some("Check the name and its database.")),
        Some(16 | 47) => (ErrorKind::Syntax, Some("Check the column name.")),
        Some(164) => (ErrorKind::Permission, Some("This connection is read-only.")),
        Some(497) => (ErrorKind::Permission, Some("Your user lacks the privilege for this action.")),
        Some(159 | 160) => (ErrorKind::Timeout, Some("Narrow the query or raise the statement timeout in Settings.")),
        Some(394) => (ErrorKind::Cancelled, None),
        Some(62 | 6 | 27 | 38 | 41 | 53 | 70 | 72 | 386 | 43 | 44 | 36) => (ErrorKind::Syntax, None),
        Some(373) => {
            (ErrorKind::Server, Some("Another statement is still running in this session. Wait for it or cancel it."))
        }
        _ if status == 401 || status == 403 => (ErrorKind::Auth, None),
        _ => (ErrorKind::Server, None),
    };
    let mut error = DbError::new(kind, message).with_action(action);
    if let Some(code) = code {
        error = error.with_code(code.to_string());
    }
    error
}

/// A streamed result: column names and types, then one JSON array per row.
pub struct Rows {
    body: BoxStream<'static, reqwest::Result<Bytes>>,
    buffer: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    pub names: Vec<String>,
    pub types: Vec<String>,
    /// Rows the statement wrote, from the `X-ClickHouse-Summary` header.
    pub written: Option<u64>,
    done: bool,
    secret: Option<String>,
}

impl Rows {
    /// Reads the header lines. Statements without a result have no columns.
    pub async fn open(response: reqwest::Response, secret: Option<&str>) -> DbResult<Self> {
        let written = response
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
            .and_then(|v| {
                v.get("written_rows").and_then(|w| w.as_str().and_then(|s| s.parse().ok()).or_else(|| w.as_u64()))
            });
        let mut rows = Self {
            body: response.bytes_stream().boxed(),
            buffer: Vec::new(),
            lines: VecDeque::new(),
            names: Vec::new(),
            types: Vec::new(),
            written,
            done: false,
            secret: secret.map(str::to_owned),
        };
        if let Some(names) = rows.line().await? {
            rows.names = rows.parse_header(&names)?;
            let types = rows.line().await?.ok_or_else(|| DbError::internal("the server sent no column types"))?;
            rows.types = rows.parse_header(&types)?;
        }
        Ok(rows)
    }

    fn parse_header(&self, line: &[u8]) -> DbResult<Vec<String>> {
        serde_json::from_slice(line).map_err(|_| self.failure(line))
    }

    /// The server writes an exception into the body when a query fails mid-stream.
    fn failure(&self, line: &[u8]) -> DbError {
        let text = String::from_utf8_lossy(line);
        server_error(None, &text, 500, self.secret.as_deref())
    }

    async fn line(&mut self) -> DbResult<Option<Vec<u8>>> {
        loop {
            if let Some(line) = self.lines.pop_front() {
                return Ok(Some(line));
            }
            if self.done {
                return Ok(None);
            }
            if let Some(chunk) = self.body.next().await {
                let chunk =
                    chunk.map_err(|e| DbError::new(ErrorKind::Network, format!("The result stream broke: {e}")))?;
                split_lines(&mut self.buffer, &chunk, &mut self.lines);
            } else {
                self.done = true;
                if !self.buffer.iter().all(u8::is_ascii_whitespace) {
                    self.lines.push_back(std::mem::take(&mut self.buffer));
                }
            }
        }
    }

    /// The next row, or `None` at the end.
    pub async fn next(&mut self) -> DbResult<Option<Vec<Value>>> {
        loop {
            let Some(line) = self.line().await? else { return Ok(None) };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return match serde_json::from_slice::<Vec<Value>>(&line) {
                Ok(row) => Ok(Some(row)),
                Err(_) => Err(self.failure(&line)),
            };
        }
    }
}

/// Appends `chunk` to `buffer` and moves every complete line into `lines`,
/// keeping the unfinished tail. Only the new bytes are searched, and the
/// consumed prefix is removed once.
fn split_lines(buffer: &mut Vec<u8>, chunk: &[u8], lines: &mut VecDeque<Vec<u8>>) {
    // The buffered tail holds no newline, so the search starts at the chunk.
    let mut scan = buffer.len();
    buffer.extend_from_slice(chunk);
    let mut start = 0;
    while let Some(i) = buffer[scan..].iter().position(|b| *b == b'\n') {
        let end = scan + i;
        lines.push_back(buffer[start..end].to_vec());
        start = end + 1;
        scan = start;
    }
    buffer.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let (mut buffer, mut lines) = (Vec::new(), VecDeque::new());
        split_lines(&mut buffer, b"[1,\"a", &mut lines);
        assert!(lines.is_empty());
        split_lines(&mut buffer, b"b\"]\n[2", &mut lines);
        split_lines(&mut buffer, b"]\n\n[3]", &mut lines);
        assert_eq!(lines, [b"[1,\"ab\"]".to_vec(), b"[2]".to_vec(), Vec::new()]);
        assert_eq!(buffer, b"[3]");

        lines.clear();
        let many: Vec<u8> = (0..1000).flat_map(|i| format!("[{i}]\n").into_bytes()).collect();
        split_lines(&mut buffer, &many, &mut lines);
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines[0], b"[3][0]");
        assert_eq!(lines[999], b"[999]");
        assert_eq!(buffer, b"");
    }

    #[test]
    fn reads_server_errors() {
        let e = server_error(
            None,
            "Code: 60. DB::Exception: Unknown table expression identifier 'nope'. (UNKNOWN_TABLE) (version 25.8.1.1)",
            404,
            None,
        );
        assert_eq!((e.kind, e.code.as_deref()), (ErrorKind::NotFound, Some("60")));
        assert!(e.message.starts_with("Unknown table") && !e.message.contains("version"));
        let auth = server_error(
            Some(516),
            "Code: 516. DB::Exception: dataquery: Authentication failed: password is incorrect",
            401,
            Some("hunter2"),
        );
        assert_eq!(auth.kind, ErrorKind::Auth);
    }
}
