//! The Snowflake REST protocol the official drivers use: password login,
//! `query-request` with bound values, polling for long queries, result
//! chunks, `abort-request` and session renewal.

use std::sync::Mutex;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::redact::redact_known;
use dq_core::{DbError, DbResult, ErrorKind};
use serde::Deserialize;
use serde_json::{Value, json};

/// Snowflake's "query still running" code: poll `getResultUrl`.
const IN_PROGRESS: &[&str] = &["333333", "333334"];
/// Session or token expired: log in again.
const EXPIRED: &[&str] = &["390112", "390114"];

#[derive(Debug, Clone, Deserialize)]
pub struct RowType {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub scale: Option<i64>,
    pub precision: Option<i64>,
    pub length: Option<i64>,
    pub nullable: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chunk {
    pub url: String,
}

/// The `data` of a finished query.
#[derive(Debug, Default, Deserialize)]
pub struct QueryData {
    #[serde(default)]
    pub rowtype: Vec<RowType>,
    #[serde(rename = "rowsetBase64")]
    pub rowset_base64: Option<String>,
    pub rowset: Option<Vec<Vec<Value>>>,
    #[serde(rename = "queryResultFormat")]
    pub format: Option<String>,
    #[serde(default)]
    pub chunks: Vec<Chunk>,
    #[serde(rename = "chunkHeaders", default)]
    pub chunk_headers: serde_json::Map<String, Value>,
    pub qrmk: Option<String>,
    #[serde(rename = "statementTypeId")]
    pub statement_type: Option<i64>,
}

impl QueryData {
    /// Inserts, updates, deletes and merges (`0x3000`–`0x3fff`).
    pub fn is_dml(&self) -> bool {
        self.statement_type.is_some_and(|t| (0x3000..0x4000).contains(&t))
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    data: Option<Value>,
    success: Option<bool>,
    code: Option<String>,
    message: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    params: ConnectParams,
    token: Mutex<Option<String>>,
}

/// `https://<account>.snowflakecomputing.com`; a bare account identifier gets the domain added.
pub fn base_url(params: &ConnectParams) -> String {
    let c = &params.config;
    // `xy12345.eu-west-1` and `myorg-myaccount` are account identifiers; full host names and local hosts are kept.
    let full =
        c.host.contains("snowflakecomputing") || c.host == "localhost" || c.host.parse::<std::net::IpAddr>().is_ok();
    let host = if full { c.host.clone() } else { format!("{}.snowflakecomputing.com", c.host) };
    let scheme = if c.ssl_mode == SslMode::Disable { "http" } else { "https" };
    let port = params.endpoint.map_or(c.port, |e| e.port());
    let default_port = if scheme == "https" { 443 } else { 80 };
    if port == default_port || port == 0 { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{port}") }
}

fn account_name(params: &ConnectParams) -> String {
    params
        .config
        .property("account")
        .map_or_else(|| params.config.host.split('.').next().unwrap_or_default().to_ascii_uppercase(), str::to_owned)
}

impl Client {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        let c = &params.config;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(u64::from(c.connect_timeout_secs)))
            .user_agent(concat!("DataQuery/", env!("CARGO_PKG_VERSION")))
            .use_rustls_tls()
            .gzip(true);
        if c.ssl_mode == SslMode::Require || c.ssl_mode == SslMode::Prefer {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(endpoint) = params.endpoint {
            let host = base_url(params)
                .split("://")
                .nth(1)
                .unwrap_or_default()
                .split(':')
                .next()
                .unwrap_or_default()
                .to_owned();
            builder = builder.resolve(&host, endpoint);
        }
        Ok(Self {
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            base: base_url(params),
            params: params.clone(),
            token: Mutex::new(None),
        })
    }

    fn secret(&self) -> Option<&str> {
        self.params.password()
    }

    fn network(&self, e: &reqwest::Error) -> DbError {
        let detail = std::error::Error::source(e).map(ToString::to_string).unwrap_or_default();
        let text = redact_known(&format!("{e} {detail}"), self.secret().unwrap_or("\u{0}"));
        let kind = if e.is_timeout() {
            ErrorKind::Timeout
        } else if text.contains("certificate") || text.contains("TLS") || text.contains("tls") {
            ErrorKind::Tls
        } else {
            ErrorKind::Network
        };
        DbError::new(kind, text)
    }

    fn failure(&self, code: Option<&str>, message: Option<&str>) -> DbError {
        let message = message.unwrap_or("Snowflake reported an error.");
        let message = redact_known(message, self.secret().unwrap_or("\u{0}"));
        let (kind, action) = match code.unwrap_or_default() {
            "390100" | "390101" | "390102" | "390144" => {
                (ErrorKind::Auth, Some("Check the username and password. The saved password was not changed."))
            }
            "390201" | "390189" => {
                (ErrorKind::NotFound, Some("Check the database, schema, warehouse and role in the connection."))
            }
            "002003" | "002043" => (ErrorKind::NotFound, Some("Check the name; Snowflake upper-cases unquoted names.")),
            "003001" | "003011" | "002037" => {
                (ErrorKind::Permission, Some("Your role lacks the privilege for this action."))
            }
            "000604" => (ErrorKind::Cancelled, None),
            "000630" => (ErrorKind::Timeout, Some("Narrow the query or raise the statement timeout in Settings.")),
            "001003" | "000904" | "001007" | "002140" | "100038" | "100035" => (ErrorKind::Syntax, None),
            "100072" | "100090" => (ErrorKind::Constraint, None),
            "000606" => {
                (ErrorKind::Server, Some("Choose a warehouse in the connection settings, or run USE WAREHOUSE."))
            }
            _ => (ErrorKind::Server, None),
        };
        let mut error = DbError::new(kind, message).with_action(action);
        if let Some(code) = code {
            error = error.with_code(code);
        }
        error
    }

    /// Logs in with the user's password and keeps the session token.
    pub async fn login(&self) -> DbResult<Value> {
        let c = &self.params.config;
        let mut query: Vec<(&str, String)> = Vec::new();
        if !c.database.trim().is_empty() {
            query.push(("databaseName", c.database.trim().to_owned()));
        }
        for (key, param) in [("schema", "schemaName"), ("warehouse", "warehouse"), ("role", "roleName")] {
            if let Some(v) = c.property(key) {
                query.push((param, v.to_owned()));
            }
        }
        query.push(("requestId", uuid::Uuid::new_v4().to_string()));
        let body = json!({
            "data": {
                "ACCOUNT_NAME": account_name(&self.params),
                "LOGIN_NAME": c.username,
                "PASSWORD": self.params.password().unwrap_or_default(),
                "CLIENT_APP_ID": "DataQuery",
                "CLIENT_APP_VERSION": env!("CARGO_PKG_VERSION"),
                "SESSION_PARAMETERS": { "QUERY_RESULT_FORMAT": "ARROW", "CLIENT_SESSION_KEEP_ALIVE": true },
            }
        });
        let response = self
            .http
            .post(format!("{}/session/v1/login-request", self.base))
            .query(&query)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        let envelope = self.envelope(response).await?;
        let data = envelope.data.unwrap_or_default();
        let token =
            data.get("token").and_then(Value::as_str).ok_or_else(|| DbError::internal("login returned no token"))?;
        if let Ok(mut t) = self.token.lock() {
            *t = Some(token.to_owned());
        }
        Ok(data)
    }

    async fn envelope(&self, response: reqwest::Response) -> DbResult<Envelope> {
        let status = response.status();
        let text = response.text().await.map_err(|e| self.network(&e))?;
        let envelope: Envelope = serde_json::from_str(&text).map_err(|_| {
            let kind =
                if status.as_u16() == 401 || status.as_u16() == 403 { ErrorKind::Auth } else { ErrorKind::Server };
            DbError::new(kind, format!("Unexpected answer from Snowflake (HTTP {status})."))
        })?;
        if envelope.success == Some(true) || envelope.code.as_deref().is_some_and(|c| IN_PROGRESS.contains(&c)) {
            Ok(envelope)
        } else {
            Err(self.failure(envelope.code.as_deref(), envelope.message.as_deref()))
        }
    }

    fn authorization(&self) -> DbResult<String> {
        let token =
            self.token.lock().ok().and_then(|t| t.clone()).ok_or_else(|| DbError::invalid("The session is closed."))?;
        Ok(format!("Snowflake Token=\"{token}\""))
    }

    /// Runs one statement and waits for its result. `request_id` lets
    /// `abort` stop it. Text values bind `?` / `:n` parameters in order.
    pub async fn query(&self, sql: &str, binds: &[Option<String>], request_id: &str) -> DbResult<QueryData> {
        match self.query_once(sql, binds, request_id).await {
            Err(e) if e.code.as_deref().is_some_and(|c| EXPIRED.contains(&c)) => {
                self.login().await?;
                self.query_once(sql, binds, request_id).await
            }
            other => other,
        }
    }

    async fn query_once(&self, sql: &str, binds: &[Option<String>], request_id: &str) -> DbResult<QueryData> {
        let bindings: serde_json::Map<String, Value> = binds
            .iter()
            .enumerate()
            .map(|(i, v)| ((i + 1).to_string(), json!({ "type": "TEXT", "value": v })))
            .collect();
        let mut body = json!({ "sqlText": sql, "asyncExec": false, "sequenceId": 1, "querySubmissionTime": 0 });
        if !bindings.is_empty() {
            body["bindings"] = Value::Object(bindings);
        }
        let response = self
            .http
            .post(format!("{}/queries/v1/query-request", self.base))
            .query(&[("requestId", request_id)])
            .header("Authorization", self.authorization()?)
            .header("Accept", "application/snowflake")
            .json(&body)
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        let mut envelope = self.envelope(response).await?;
        // Long queries answer "in progress"; poll until the result is ready.
        let mut delay = Duration::from_millis(250);
        while envelope.code.as_deref().is_some_and(|c| IN_PROGRESS.contains(&c)) {
            let url = envelope
                .data
                .as_ref()
                .and_then(|d| d.get("getResultUrl"))
                .and_then(Value::as_str)
                .ok_or_else(|| DbError::internal("a running query returned no result URL"))?
                .to_owned();
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(5));
            let response = self
                .http
                .get(format!("{}{url}", self.base))
                .header("Authorization", self.authorization()?)
                .header("Accept", "application/snowflake")
                .send()
                .await
                .map_err(|e| self.network(&e))?;
            envelope = self.envelope(response).await?;
        }
        serde_json::from_value(envelope.data.unwrap_or_default())
            .map_err(|e| DbError::internal(format!("unreadable result: {e}")))
    }

    /// Asks Snowflake to stop the statement sent with `request_id`.
    pub async fn abort(&self, sql: &str, request_id: &str) -> DbResult<()> {
        let response = self
            .http
            .post(format!("{}/queries/v1/abort-request", self.base))
            .query(&[("requestId", uuid::Uuid::new_v4().to_string())])
            .header("Authorization", self.authorization()?)
            .json(&json!({ "sqlText": sql, "requestId": request_id }))
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        self.envelope(response).await.map(|_| ())
    }

    /// Downloads a result chunk (Arrow IPC or JSON rows, possibly gzip-compressed).
    pub async fn chunk(
        &self,
        chunk: &Chunk,
        headers: &serde_json::Map<String, Value>,
        qrmk: Option<&str>,
    ) -> DbResult<Vec<u8>> {
        let mut request = self.http.get(&chunk.url);
        for (k, v) in headers {
            if let Some(v) = v.as_str() {
                request = request.header(k, v);
            }
        }
        if headers.is_empty()
            && let Some(key) = qrmk
        {
            request = request
                .header("x-amz-server-side-encryption-customer-key", key)
                .header("x-amz-server-side-encryption-customer-algorithm", "AES256");
        }
        let bytes = request.send().await.map_err(|e| self.network(&e))?.bytes().await.map_err(|e| self.network(&e))?;
        if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out)
                .map_err(|e| DbError::internal(format!("unreadable result chunk: {e}")))?;
            return Ok(out);
        }
        Ok(bytes.to_vec())
    }

    /// Ends the session on the server.
    pub async fn logout(&self) {
        if let Ok(auth) = self.authorization() {
            let _ = self
                .http
                .post(format!("{}/session", self.base))
                .query(&[("delete", "true")])
                .header("Authorization", auth)
                .send()
                .await;
        }
        if let Ok(mut t) = self.token.lock() {
            *t = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::config::ConnConfig;

    #[test]
    fn builds_account_urls() {
        let params = |host: &str, port: u16, ssl| {
            ConnectParams::new(ConnConfig { host: host.into(), port, ssl_mode: ssl, ..ConnConfig::default() }, None)
        };
        assert_eq!(
            base_url(&params("xy12345.eu-west-1", 443, SslMode::VerifyFull)),
            "https://xy12345.eu-west-1.snowflakecomputing.com"
        );
        assert_eq!(
            base_url(&params("myorg-myaccount", 443, SslMode::VerifyFull)),
            "https://myorg-myaccount.snowflakecomputing.com"
        );
        assert_eq!(base_url(&params("localhost", 58000, SslMode::Disable)), "http://localhost:58000");
        assert_eq!(
            account_name(&params("xy12345.eu-west-1.snowflakecomputing.com", 443, SslMode::VerifyFull)),
            "XY12345"
        );
    }
}
