//! libSQL over HTTP: Hrana 3 pipelines (`POST /v3/pipeline`, JSON) to Turso
//! or a self-hosted `sqld`.
//!
//! A pipeline runs requests on a stream (one server connection). Its reply
//! carries a baton that continues the stream; without one the request opens
//! a new stream. Editor statements share one stream, so transactions and
//! pragmas last between them; while a transaction is open the stream is kept
//! alive, because idle streams expire after a few seconds. Catalog reads and exports open a stream of their own and close
//! it in the same pipeline. Grid edits and imports run as one conditional
//! batch on a fresh stream and commit only when every count matches.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use dq_core::adapter::{ConnectParams, check_affected};
use dq_core::edit::BoundStatement;
use dq_core::sql::{Dialect, SqliteDialect};
use dq_core::{DbError, DbResult, ErrorKind};
use dq_sqlite::error::classify_message;
use dq_sqlite::values::Value;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

use crate::http::Http;
use crate::transport::{Column, Output, Transport};

/// A stream idle this long during a transaction gets a keepalive request.
const KEEPALIVE_AFTER: Duration = Duration::from_secs(4);

/// Base64 as Hrana servers write it, with or without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Deserialize)]
struct PipelineReply {
    baton: Option<String>,
    base_url: Option<String>,
    results: Vec<StreamResult>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: HranaError },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamResponse {
    Execute {
        result: StmtResult,
    },
    Batch {
        result: BatchResult,
    },
    GetAutocommit {
        is_autocommit: bool,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StmtResult {
    cols: Vec<Col>,
    rows: Vec<Vec<HranaValue>>,
    affected_row_count: u64,
}

#[derive(Deserialize)]
struct Col {
    name: Option<String>,
    decltype: Option<String>,
}

#[derive(Deserialize)]
struct BatchResult {
    step_results: Vec<Option<StmtResult>>,
    step_errors: Vec<Option<HranaError>>,
}

#[derive(Deserialize)]
struct HranaError {
    message: String,
    code: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HranaValue {
    Null,
    /// 64-bit integers travel as strings, exactly.
    Integer {
        value: String,
    },
    Float {
        value: f64,
    },
    Text {
        value: String,
    },
    Blob {
        base64: String,
    },
}

impl HranaValue {
    fn into_value(self) -> DbResult<Value> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Integer { value } => {
                Value::Integer(value.parse().map_err(|_| DbError::internal("the server sent an invalid integer"))?)
            }
            Self::Float { value } => Value::Real(value),
            Self::Text { value } => Value::Text(value),
            Self::Blob { base64 } => {
                Value::Blob(BASE64.decode(base64).map_err(|_| DbError::internal("the server sent an invalid blob"))?)
            }
        })
    }
}

impl StmtResult {
    fn into_output(self) -> DbResult<Output> {
        let columns = self
            .cols
            .into_iter()
            .map(|c| Column { name: c.name.unwrap_or_default(), decltype: c.decltype.unwrap_or_default() })
            .collect();
        let rows = self
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(HranaValue::into_value).collect::<DbResult<Vec<_>>>())
            .collect::<DbResult<Vec<_>>>()?;
        Ok(Output { columns, rows, affected: self.affected_row_count })
    }
}

/// An open stream: the baton that continues it and where to send it.
struct Stream {
    baton: String,
    base_url: Option<String>,
    in_transaction: bool,
    used: Instant,
}

pub struct Libsql {
    http: Http,
    /// The editor's stream. Taken out while a request uses it, so a cancelled
    /// request leaves none (its baton is spent).
    stream: Mutex<Option<Stream>>,
}

/// A statement request. Every parameter is text or NULL; columns convert by affinity.
fn execute(sql: &str, params: &[Option<String>]) -> Json {
    let args: Vec<Json> = params
        .iter()
        .map(|p| p.as_ref().map_or_else(|| json!({ "type": "null" }), |v| json!({ "type": "text", "value": v })))
        .collect();
    json!({ "type": "execute", "stmt": { "sql": sql, "args": args, "want_rows": true } })
}

fn error_of(e: HranaError, http: &Http) -> DbError {
    let message = http.redact(&e.message);
    let (kind, action) = match e.code.as_deref() {
        // Also statements sqld does not support, such as `CREATE TEMP TABLE`.
        Some("SQL_PARSE_ERROR") => (ErrorKind::Syntax, None),
        _ => classify_message(&message),
    };
    let error = DbError::new(kind, message).with_action(action);
    match e.code {
        Some(code) => error.with_code(code),
        None => error,
    }
}

/// The stream behind a baton is gone: it expired, or the server restarted.
fn stream_gone(e: &DbError) -> bool {
    e.code.as_deref() == Some("STREAM_GONE")
}

impl Libsql {
    pub fn connect(params: &ConnectParams) -> DbResult<Arc<Self>> {
        let session = Arc::new(Self { http: Http::new(params)?, stream: Mutex::new(None) });
        tokio::spawn(keepalive(Arc::downgrade(&session)));
        Ok(session)
    }

    /// Requests that prepare a new stream like a local connection: foreign keys
    /// enforced. (sqld refuses `PRAGMA query_only`; read-only profiles are
    /// checked before statements are sent, and Turso offers read-only tokens.)
    fn setup() -> Vec<Json> {
        vec![execute("PRAGMA foreign_keys = ON", &[])]
    }

    async fn pipeline(
        &self,
        stream: Option<&Stream>,
        requests: Vec<Json>,
        timeout: Option<Duration>,
    ) -> DbResult<PipelineReply> {
        let base = stream.and_then(|s| s.base_url.as_deref()).unwrap_or(&self.http.origin).trim_end_matches('/');
        let body = json!({ "baton": stream.map(|s| &s.baton), "requests": requests });
        let reply = self.http.post(&format!("{base}/v3/pipeline"), &body, timeout).await?;
        if reply.ok() {
            return serde_json::from_value(reply.body)
                .map_err(|e| DbError::internal(format!("unexpected reply from the libSQL server: {e}")));
        }
        // Plain text, `{"error": …}` or `{"message": …, "code": "STREAM_EXPIRED"}`.
        let field = |key: &str| reply.body.get(key).and_then(Json::as_str).map(str::trim);
        let message = field("error").or_else(|| field("message")).unwrap_or(reply.text.trim()).to_owned();
        let code = field("code").unwrap_or_default();
        Err(match reply.status {
            401 | 403 => DbError::new(
                ErrorKind::Auth,
                if message.is_empty() { "The server refused the auth token." } else { &message },
            )
            .with_action(Some(
                "Check the auth token (Turso: turso db tokens create <database>). The saved token was not changed.",
            )),
            404 => DbError::not_found("This server does not speak libSQL over HTTP (no /v3/pipeline).")
                .with_action(Some("Check the host and port of the Turso database or sqld server.")),
            400 if stream.is_some()
                && (code.starts_with("STREAM_") || message.contains("stream") || message.contains("baton")) =>
            {
                DbError::new(ErrorKind::Server, message).with_code("STREAM_GONE")
            }
            _ => {
                let (kind, action) = classify_message(&message);
                DbError::new(
                    kind,
                    if message.is_empty() { format!("The server answered HTTP {}.", reply.status) } else { message },
                )
                .with_action(action)
            }
        })
    }

    /// Runs requests on the editor stream, opening one when there is none.
    /// Returns the results of `requests` (after any setup requests).
    async fn on_stream(&self, requests: Vec<Json>, timeout: Option<Duration>) -> DbResult<Vec<StreamResult>> {
        let mut slot = self.stream.lock().await;
        let current = slot.take();
        let attempt = |current: Option<Stream>| {
            let mut all = if current.is_none() { Self::setup() } else { Vec::new() };
            let setup = all.len();
            all.extend(requests.iter().cloned());
            all.push(json!({ "type": "get_autocommit" }));
            async move {
                let reply = self.pipeline(current.as_ref(), all, timeout).await;
                (current, setup, reply)
            }
        };
        let (current, setup, reply) = attempt(current).await;
        let (current, setup, reply) = match reply {
            // An idle stream expired: outside a transaction nothing is lost, so start a new one.
            Err(e) if stream_gone(&e) && current.as_ref().is_some_and(|s| !s.in_transaction) => attempt(None).await,
            Err(e) if stream_gone(&e) => {
                return Err(DbError::new(
                    ErrorKind::Server,
                    "The server closed this session's stream, and with it the open transaction: its changes were rolled back.",
                )
                .with_action(Some("Run the transaction again.")));
            }
            other => (current, setup, other),
        };
        let mut reply = reply?;
        let autocommit = match reply.results.pop() {
            Some(StreamResult::Ok { response: StreamResponse::GetAutocommit { is_autocommit } }) => is_autocommit,
            _ => true,
        };
        let mut results = reply.results.into_iter();
        for result in results.by_ref().take(setup) {
            if let StreamResult::Error { error } = result {
                return Err(error_of(error, &self.http));
            }
        }
        *slot = reply.baton.map(|baton| Stream {
            baton,
            base_url: reply.base_url.or_else(|| current.and_then(|s| s.base_url)),
            in_transaction: !autocommit,
            used: Instant::now(),
        });
        Ok(results.collect())
    }

    /// The output of one `execute` result.
    fn output(&self, result: Option<StreamResult>) -> DbResult<Output> {
        match result {
            Some(StreamResult::Ok { response: StreamResponse::Execute { result } }) => result.into_output(),
            Some(StreamResult::Error { error }) => Err(error_of(error, &self.http)),
            _ => Err(DbError::internal("the libSQL server sent no statement result")),
        }
    }

    /// Changed-row counts of a transaction batch (`BEGIN`, then `statements`),
    /// or its first error or unmet expectation.
    fn batch_counts(&self, result: Option<StreamResult>, statements: &[BoundStatement]) -> DbResult<Vec<u64>> {
        let batch = match result {
            Some(StreamResult::Ok { response: StreamResponse::Batch { result } }) => result,
            Some(StreamResult::Error { error }) => return Err(error_of(error, &self.http)),
            _ => return Err(DbError::internal("the libSQL server sent no batch result")),
        };
        if let Some(error) = batch.step_errors.into_iter().flatten().next() {
            return Err(error_of(error, &self.http));
        }
        let mut counts = Vec::with_capacity(statements.len());
        for (statement, result) in statements.iter().zip(batch.step_results.into_iter().skip(1)) {
            let affected = result.map_or(0, |r| r.affected_row_count);
            check_affected(statement, affected)?;
            counts.push(affected);
        }
        Ok(counts)
    }

    /// Ends a stream opened for a transaction, keeping or undoing its changes.
    async fn finish(&self, stream: &Stream, commit: bool) -> DbResult<()> {
        let end = execute(if commit { "COMMIT" } else { "ROLLBACK" }, &[]);
        let mut reply = self.pipeline(Some(stream), vec![end, json!({ "type": "close" })], None).await?;
        self.output(reply.results.drain(..).next()).map(|_| ())
    }
}

/// Keeps the editor stream alive while it holds an open transaction.
async fn keepalive(session: Weak<Libsql>) {
    loop {
        tokio::time::sleep(KEEPALIVE_AFTER).await;
        let Some(session) = session.upgrade() else { return };
        // A request using the stream holds the lock, and keeps it alive itself.
        let Ok(mut slot) = session.stream.try_lock() else { continue };
        let Some(stream) = slot.take_if(|s| s.in_transaction && s.used.elapsed() >= KEEPALIVE_AFTER) else { continue };
        let ping = vec![json!({ "type": "get_autocommit" })];
        if let Ok(reply) = session.pipeline(Some(&stream), ping, Some(KEEPALIVE_AFTER)).await {
            *slot = reply.baton.map(|baton| Stream {
                baton,
                base_url: reply.base_url.or(stream.base_url),
                used: Instant::now(),
                ..stream
            });
        }
    }
}

#[async_trait]
impl Transport for Libsql {
    fn product(&self) -> &'static str {
        "libSQL"
    }

    fn http(&self) -> &Http {
        &self.http
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect
    }

    fn internal_prefixes(&self) -> &'static [&'static str] {
        &["sqlite\\_", "libsql\\_"]
    }

    async fn query(&self, sql: &str, params: &[Option<String>], timeout: Option<Duration>) -> DbResult<Output> {
        let requests = vec![execute(sql, params), json!({ "type": "close" })];
        let reply = self.pipeline(None, requests, timeout).await?;
        self.output(reply.results.into_iter().next())
    }

    async fn execute(&self, sql: &str, params: &[Option<String>], timeout: Option<Duration>) -> DbResult<Output> {
        let results = self.on_stream(vec![execute(sql, params)], timeout).await?;
        self.output(results.into_iter().next())
    }

    async fn transaction(&self, statements: &[BoundStatement]) -> DbResult<Vec<u64>> {
        // Each step runs only when the one before succeeded, so a failure stops the batch.
        let mut steps = vec![json!({ "stmt": { "sql": "BEGIN IMMEDIATE", "want_rows": false } })];
        for (i, statement) in statements.iter().enumerate() {
            let Json::Object(mut step) = execute(&statement.sql, &statement.params) else { unreachable!() };
            step.remove("type");
            step.insert("condition".into(), json!({ "type": "ok", "step": i }));
            steps.push(Json::Object(step));
        }
        let mut requests = Self::setup();
        requests.push(json!({ "type": "batch", "batch": { "steps": steps } }));
        let reply = self.pipeline(None, requests, None).await?;
        let stream = reply.baton.map(|baton| Stream {
            baton,
            base_url: reply.base_url,
            in_transaction: true,
            used: Instant::now(),
        });
        let outcome = self.batch_counts(reply.results.into_iter().nth(1), statements);
        let Some(stream) = stream else { return outcome };
        match outcome {
            Ok(counts) => self.finish(&stream, true).await.map(|()| counts),
            Err(e) => {
                let _ = self.finish(&stream, false).await;
                Err(e)
            }
        }
    }

    async fn close(&self) {
        if let Some(stream) = self.stream.lock().await.take() {
            let _ = self.pipeline(Some(&stream), vec![json!({ "type": "close" })], Some(KEEPALIVE_AFTER)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_hrana_results() {
        let reply: PipelineReply = serde_json::from_value(json!({
            "baton": "b1",
            "base_url": null,
            "results": [
                { "type": "ok", "response": { "type": "execute", "result": {
                    "cols": [{ "name": "id", "decltype": "INTEGER" }, { "name": "raw", "decltype": null }],
                    "rows": [[{ "type": "integer", "value": "9223372036854775807" }, { "type": "blob", "base64": "iVBORw" }],
                             [{ "type": "float", "value": 1.5 }, { "type": "null" }]],
                    "affected_row_count": 0, "last_insert_rowid": null, "rows_read": 1
                } } },
                { "type": "error", "error": { "message": "UNIQUE constraint failed: t.id", "code": "SQLITE_CONSTRAINT_UNIQUE" } },
                { "type": "ok", "response": { "type": "get_autocommit", "is_autocommit": false } },
                { "type": "ok", "response": { "type": "close" } }
            ]
        }))
        .unwrap();
        let mut results = reply.results.into_iter();
        let Some(StreamResult::Ok { response: StreamResponse::Execute { result } }) = results.next() else { panic!() };
        let output = result.into_output().unwrap();
        assert_eq!(output.columns[0], Column { name: "id".into(), decltype: "INTEGER".into() });
        assert_eq!(output.rows[0], [Value::Integer(i64::MAX), Value::Blob(vec![0x89, 0x50, 0x4e, 0x47])]);
        assert_eq!(output.rows[1], [Value::Real(1.5), Value::Null]);
        let Some(StreamResult::Error { error }) = results.next() else { panic!() };
        assert_eq!(error.code.as_deref(), Some("SQLITE_CONSTRAINT_UNIQUE"));
        assert!(matches!(
            results.next(),
            Some(StreamResult::Ok { response: StreamResponse::GetAutocommit { is_autocommit: false } })
        ));
        assert!(matches!(results.next(), Some(StreamResult::Ok { response: StreamResponse::Other })));
    }

    #[test]
    fn binds_text_and_null_parameters() {
        let request = execute("SELECT ?1, ?2", &[Some("it's".into()), None]);
        assert_eq!(request["stmt"]["args"], json!([{ "type": "text", "value": "it's" }, { "type": "null" }]));
    }
}
