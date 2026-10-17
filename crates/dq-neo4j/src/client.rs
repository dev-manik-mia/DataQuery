//! Neo4j's Query API (`POST /db/{db}/query/v2`, HTTP or HTTPS): typed values,
//! counters, notifications, query plans and explicit transactions
//! (`…/query/v2/tx`, `…/tx/{id}`, `…/tx/{id}/commit`, `DELETE …/tx/{id}`).

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::redact::redact_known;
use dq_core::{DbError, DbResult, ErrorKind};
use futures_util::StreamExt;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};

/// The typed JSON format: every value is `{ "$type": …, "_value": … }`.
const MEDIA_TYPE: &str = "application/vnd.neo4j.query";
/// Largest response body read; bigger results must be narrowed with `LIMIT`.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// One statement to run.
pub struct Query<'a> {
    pub database: &'a str,
    pub statement: &'a str,
    /// Typed parameters (`{ name: { "$type": …, "_value": … } }`).
    pub parameters: Map<String, Value>,
    pub access: Access,
    /// Run inside this open transaction.
    pub transaction: Option<&'a Transaction>,
}

impl<'a> Query<'a> {
    pub fn read(database: &'a str, statement: &'a str) -> Self {
        Self { database, statement, parameters: Map::new(), access: Access::Read, transaction: None }
    }

    pub fn write(database: &'a str, statement: &'a str) -> Self {
        Self { access: Access::Write, ..Self::read(database, statement) }
    }

    #[must_use]
    pub fn with(mut self, name: &str, value: Value) -> Self {
        self.parameters.insert(name.to_owned(), value);
        self
    }
}

/// An open explicit transaction: its id, database and the cluster routing header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub id: String,
    pub database: String,
    pub affinity: Option<String>,
}

/// A query's reply.
#[derive(Debug, Default, Clone)]
pub struct Reply {
    pub fields: Vec<String>,
    /// Rows of typed values.
    pub values: Vec<Vec<Value>>,
    pub counters: Option<Map<String, Value>>,
    pub notifications: Vec<String>,
    /// `queryPlan` (EXPLAIN) or `profiledQueryPlan` (PROFILE).
    pub plan: Option<Value>,
    pub profiled: bool,
    /// Set when the query opened a transaction.
    pub transaction: Option<Transaction>,
    /// The result had more rows than were kept (see [`Client::run_limited`]).
    pub more: bool,
}

impl Reply {
    /// Nodes and relationships created or deleted plus properties set, when the query changed data.
    pub fn affected(&self) -> Option<u64> {
        let c = self.counters.as_ref()?;
        if !c.get("containsUpdates").and_then(Value::as_bool).unwrap_or(false)
            && !c.get("containsSystemUpdates").and_then(Value::as_bool).unwrap_or(false)
        {
            return None;
        }
        let n = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(
            n("nodesCreated")
                + n("nodesDeleted")
                + n("relationshipsCreated")
                + n("relationshipsDeleted")
                + n("propertiesSet")
                + n("systemUpdates"),
        )
    }

    /// `2 nodes created, 4 properties set` from the counters.
    pub fn counters_summary(&self) -> Option<String> {
        let c = self.counters.as_ref()?;
        let parts: Vec<String> = [
            ("nodesCreated", "nodes created"),
            ("nodesDeleted", "nodes deleted"),
            ("relationshipsCreated", "relationships created"),
            ("relationshipsDeleted", "relationships deleted"),
            ("propertiesSet", "properties set"),
            ("labelsAdded", "labels added"),
            ("labelsRemoved", "labels removed"),
            ("indexesAdded", "indexes added"),
            ("indexesRemoved", "indexes removed"),
            ("constraintsAdded", "constraints added"),
            ("constraintsRemoved", "constraints removed"),
            ("systemUpdates", "system updates"),
        ]
        .iter()
        .filter_map(|(k, label)| c.get(*k).and_then(Value::as_u64).filter(|n| *n > 0).map(|n| format!("{n} {label}")))
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    user: String,
    password: Option<String>,
}

impl Client {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        dq_core::tls::install_crypto_provider();
        let config = &params.config;
        let tls = config.ssl_mode != SslMode::Disable;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs.max(1))))
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
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            base: format!("{}://{host}:{port}", if tls { "https" } else { "http" }),
            user: config.username.clone(),
            password: params.password.clone(),
        })
    }

    fn url(&self, database: &str, tail: &str) -> String {
        let db: String = url_segment(database);
        format!("{}/db/{db}/query/v2{tail}", self.base)
    }

    fn request(&self, method: reqwest::Method, url: &str, affinity: Option<&str>) -> reqwest::RequestBuilder {
        let mut b = self
            .http
            .request(method, url)
            .header(reqwest::header::ACCEPT, MEDIA_TYPE)
            .header(reqwest::header::CONTENT_TYPE, MEDIA_TYPE);
        if !self.user.is_empty() || self.password.is_some() {
            b = b.basic_auth(&self.user, self.password.as_deref());
        }
        if let Some(a) = affinity {
            b = b.header("neo4j-cluster-affinity", a);
        }
        b
    }

    /// Server root: version and edition (`GET /`).
    pub async fn discovery(&self) -> DbResult<Value> {
        let response = self
            .http
            .get(format!("{}/", self.base))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        let (_, body) = self.read(response).await?;
        Ok(body)
    }

    /// Runs one statement: auto-commit, or in `query.transaction`. `open_transaction`
    /// starts a new explicit transaction with this statement.
    pub async fn run(&self, query: &Query<'_>, open_transaction: bool) -> DbResult<Reply> {
        self.run_limited(query, open_transaction, usize::MAX).await
    }

    /// [`Client::run`] keeping only the first `max_rows` rows: the rest are
    /// skipped while parsing, and [`Reply::more`] is set when there were any.
    pub async fn run_limited(&self, query: &Query<'_>, open_transaction: bool, max_rows: usize) -> DbResult<Reply> {
        let mut body = json!({ "statement": query.statement, "includeCounters": true });
        if !query.parameters.is_empty() {
            body["parameters"] = Value::Object(query.parameters.clone());
        }
        if query.access == Access::Read {
            body["accessMode"] = json!("READ");
        }
        let (url, affinity) = match (query.transaction, open_transaction) {
            (Some(tx), _) => (self.url(&tx.database, &format!("/tx/{}", url_segment(&tx.id))), tx.affinity.as_deref()),
            (None, true) => (self.url(query.database, "/tx"), None),
            (None, false) => (self.url(query.database, ""), None),
        };
        let response = self
            .request(reqwest::Method::POST, &url, affinity)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        let new_affinity = response
            .headers()
            .get("neo4j-cluster-affinity")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| affinity.map(str::to_owned));
        let (_, body, more) = self.read_rows(response, max_rows).await?;
        let mut reply = parse_reply(body);
        reply.more = more;
        if let Some(tx) = &mut reply.transaction {
            tx.database = query.transaction.map_or_else(|| query.database.to_owned(), |t| t.database.clone());
            tx.affinity = new_affinity;
        }
        Ok(reply)
    }

    pub async fn commit(&self, tx: &Transaction) -> DbResult<Reply> {
        let url = self.url(&tx.database, &format!("/tx/{}/commit", url_segment(&tx.id)));
        let response = self
            .request(reqwest::Method::POST, &url, tx.affinity.as_deref())
            .json(&json!({}))
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        let (_, body) = self.read(response).await?;
        Ok(parse_reply(body))
    }

    pub async fn rollback(&self, tx: &Transaction) -> DbResult<()> {
        let url = self.url(&tx.database, &format!("/tx/{}", url_segment(&tx.id)));
        let response = self
            .request(reqwest::Method::DELETE, &url, tx.affinity.as_deref())
            .send()
            .await
            .map_err(|e| self.network(&e))?;
        self.read(response).await.map(|_| ())
    }

    /// Reads a response with a size limit and turns `errors` into a [`DbError`].
    async fn read(&self, response: reqwest::Response) -> DbResult<(reqwest::StatusCode, Value)> {
        self.read_rows(response, usize::MAX).await.map(|(status, body, _)| (status, body))
    }

    /// [`Client::read`] keeping at most `max_rows` rows of `data.values`; true when there were more.
    async fn read_rows(
        &self,
        response: reqwest::Response,
        max_rows: usize,
    ) -> DbResult<(reqwest::StatusCode, Value, bool)> {
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(DbError::new(ErrorKind::Auth, "The user name or password was not accepted.")
                .with_action(Some("Check the credentials of this connection.")));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| self.network(&e))?;
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(DbError::invalid(
                    "The result is larger than 256 MB. Narrow it with LIMIT or a more selective MATCH.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let (body, more) = if bytes.is_empty() {
            (Value::Null, false)
        } else {
            parse_body(&bytes, max_rows).map_err(|_| {
                DbError::new(
                    ErrorKind::Server,
                    format!("Neo4j answered HTTP {status} with a body that is not JSON. Is this the HTTP port?"),
                )
            })?
        };
        if let Some(error) = body.get("errors").and_then(Value::as_array).and_then(|e| e.first()) {
            let code = error.get("code").and_then(Value::as_str).unwrap_or_default();
            let message = error.get("message").and_then(Value::as_str).unwrap_or("Neo4j reported an error.");
            return Err(self.server_error(code, message));
        }
        if !status.is_success() {
            return Err(DbError::new(ErrorKind::Server, format!("Neo4j answered HTTP {status}.")));
        }
        Ok((status, body, more))
    }

    fn secret(&self, text: &str) -> String {
        self.password.as_deref().map_or_else(|| text.to_owned(), |p| redact_known(text, p))
    }

    fn server_error(&self, code: &str, message: &str) -> DbError {
        let kind = match code {
            c if c.contains("Security.Unauthorized") || c.contains("Security.AuthenticationRateLimit") => {
                ErrorKind::Auth
            }
            c if c.contains("Security.Forbidden") || c.contains("AccessMode") => ErrorKind::Permission,
            c if c.contains("SyntaxError") || c.contains("ArgumentError") || c.contains("TypeError") => {
                ErrorKind::Syntax
            }
            c if c.contains("ConstraintValidationFailed") || c.contains("Schema.Constraint") => ErrorKind::Constraint,
            c if c.contains("DatabaseNotFound") || c.contains("EntityNotFound") || c.contains("Request.Invalid") => {
                ErrorKind::NotFound
            }
            c if c.contains("Terminated") => ErrorKind::Cancelled,
            c if c.contains("TransactionTimedOut") => ErrorKind::Timeout,
            c if c.contains("ParameterMissing") => ErrorKind::InvalidInput,
            _ => ErrorKind::Server,
        };
        let message = self.secret(message);
        let (first, detail) = message.split_once('\n').map_or((message.as_str(), None), |(a, b)| (a, Some(b)));
        let position = first
            .rsplit_once("(offset: ")
            .and_then(|(_, rest)| rest.split(')').next())
            .and_then(|n| n.trim().parse::<u32>().ok())
            .map(|offset| offset + 1);
        DbError::new(kind, first).with_code(code).with_detail(detail).with_position(position)
    }

    fn network(&self, e: &reqwest::Error) -> DbError {
        let text = self.secret(&e.to_string());
        if e.is_timeout() {
            return DbError::new(ErrorKind::Timeout, format!("Neo4j did not answer in time: {text}"));
        }
        let tls = text.contains("certificate") || text.contains("tls") || text.contains("TLS");
        DbError::new(if tls { ErrorKind::Tls } else { ErrorKind::Network }, format!("Network error: {text}"))
            .with_action(Some("Check the host, the HTTP port (7474) and that Neo4j is reachable from this machine."))
    }
}

/// A database name or transaction id as one URL path segment.
fn url_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A reply body keeping the first `max_rows` rows of `data.values`, and whether
/// there were more. Later rows are skipped by the parser without building values.
fn parse_body(bytes: &[u8], max_rows: usize) -> serde_json::Result<(Value, bool)> {
    let mut more = false;
    let mut de = serde_json::Deserializer::from_slice(bytes);
    match (Body { max_rows, more: &mut more, data: false }).deserialize(&mut de).and_then(|v| de.end().map(|()| v)) {
        Ok(body) => Ok((body, more)),
        // Not an object: keep it whole, as any JSON body was before.
        Err(_) => serde_json::from_slice(bytes).map(|body| (body, false)),
    }
}

/// The reply object (or its `data` member when `data` is set); `data.values` goes to [`Rows`].
struct Body<'a> {
    max_rows: usize,
    more: &'a mut bool,
    data: bool,
}

impl<'de> DeserializeSeed<'de> for Body<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Body<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a Query API reply object")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = match (self.data, key.as_str()) {
                (false, "data") => {
                    map.next_value_seed(Body { max_rows: self.max_rows, more: &mut *self.more, data: true })?
                }
                (true, "values") => map.next_value_seed(Rows { max_rows: self.max_rows, more: &mut *self.more })?,
                _ => map.next_value()?,
            };
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}

/// `data.values`: the first `max_rows` rows; the rest are read past.
struct Rows<'a> {
    max_rows: usize,
    more: &'a mut bool,
}

impl<'de> DeserializeSeed<'de> for Rows<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Rows<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of rows")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut rows = Vec::new();
        while rows.len() < self.max_rows {
            match seq.next_element::<Value>()? {
                Some(row) => rows.push(row),
                None => return Ok(Value::Array(rows)),
            }
        }
        while seq.next_element::<IgnoredAny>()?.is_some() {
            *self.more = true;
        }
        Ok(Value::Array(rows))
    }
}

pub fn parse_reply(mut body: Value) -> Reply {
    let data = body.get("data");
    let fields = data
        .and_then(|d| d.get("fields"))
        .and_then(Value::as_array)
        .map(|f| f.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let values = match body.get_mut("data").and_then(|d| d.get_mut("values")).map(Value::take) {
        Some(Value::Array(rows)) => rows
            .into_iter()
            .map(|r| match r {
                Value::Array(cells) => cells,
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let notifications = body
        .get("notifications")
        .and_then(Value::as_array)
        .map(|ns| {
            ns.iter()
                .map(|n| {
                    let title = n.get("title").and_then(Value::as_str).unwrap_or_default();
                    let severity = n.get("severity").and_then(Value::as_str).unwrap_or("INFORMATION");
                    let description = n.get("description").and_then(Value::as_str).unwrap_or_default();
                    format!("{severity}: {title} {description}").trim().to_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    let (plan, profiled) = match (body.get("profiledQueryPlan"), body.get("queryPlan")) {
        (Some(p), _) => (Some(p.clone()), true),
        (None, Some(p)) => (Some(p.clone()), false),
        _ => (None, false),
    };
    let transaction = body.get("transaction").and_then(|t| t.get("id")).and_then(Value::as_str).map(|id| Transaction {
        id: id.to_owned(),
        database: String::new(),
        affinity: None,
    });
    Reply {
        fields,
        values,
        counters: body.get("counters").and_then(Value::as_object).cloned(),
        notifications,
        plan,
        profiled,
        transaction,
        more: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_counters() {
        let body = json!({
            "data": { "fields": ["a"], "values": [[{"$type": "Integer", "_value": "1"}]] },
            "counters": { "containsUpdates": true, "nodesCreated": 2, "propertiesSet": 4 },
            "notifications": [{ "severity": "WARNING", "title": "Cartesian product", "description": "Slow." }],
            "transaction": { "id": "abc", "expires": "x" }
        });
        let r = parse_reply(body);
        assert_eq!(r.fields, ["a"]);
        assert_eq!(r.affected(), Some(6));
        assert_eq!(r.counters_summary().as_deref(), Some("2 nodes created, 4 properties set"));
        assert_eq!(r.notifications, ["WARNING: Cartesian product Slow."]);
        assert_eq!(r.transaction.unwrap().id, "abc");
        let read = parse_reply(json!({ "counters": { "containsUpdates": false } }));
        assert_eq!(read.affected(), None);
        assert_eq!(url_segment("my db/x"), "my%20db%2Fx");
    }

    #[test]
    fn bodies_keep_the_first_rows() {
        let text = br#"{"data":{"fields":["n"],"values":[[1],[{"a":[2]}],[3]]},"bookmarks":["b"]}"#;
        let (body, more) = parse_body(text, 2).unwrap();
        assert!(more);
        let r = parse_reply(body);
        assert_eq!(r.values, [vec![json!(1)], vec![json!({"a": [2]})]]);
        assert_eq!(r.fields, ["n"]);
        let (body, more) = parse_body(text, 3).unwrap();
        assert!(!more && body["bookmarks"] == json!(["b"]));
        assert_eq!(parse_body(b"[1]", 1).unwrap(), (json!([1]), false));
        assert!(parse_body(br#"{"data":{"values":[[1],[2"#, 1).is_err());
    }
}
