//! Cloudflare D1 through the Cloudflare API:
//! `POST /client/v4/accounts/{account}/d1/database/{id}/raw` with an API token.
//!
//! Each request is atomic and there are no interactive transactions, so
//! `BEGIN` is refused by the service and the manual-commit mode is off. A
//! request with several statements (`batch`) runs them as one transaction:
//! scripts, grid edits and imports use that. An edit's expected row count is
//! checked inside the batch by a guard statement that fails, undoing the
//! whole batch, when `changes()` differs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dq_core::adapter::ConnectParams;
use dq_core::config::ConnConfig;
use dq_core::edit::BoundStatement;
use dq_core::sql::{D1Dialect, Dialect};
use dq_core::{DbError, DbResult, ErrorKind};
use dq_sqlite::error::classify_message;
use dq_sqlite::values::Value;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value as Json, json};

use crate::http::{Http, Reply};
use crate::transport::{Column, Output, Transport};

/// Databases listed per page, and the most pages read.
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 20;
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Fails (with "malformed JSON") unless the statement before changed `?1` rows.
const GUARD: &str = "SELECT CASE WHEN changes() = CAST(?1 AS INTEGER) THEN 1 ELSE json('expected-row-count') END";

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct RawResult {
    results: Option<RawRows>,
    meta: Option<Meta>,
}

#[derive(Deserialize, Default)]
struct RawRows {
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<Json>>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(default)]
    changes: u64,
}

/// A database of the account, from `GET …/d1/database`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub uuid: String,
    pub name: String,
}

pub struct D1 {
    http: Http,
    /// `…/accounts/{account}/d1/database/{id}`.
    base: String,
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A JSON value of a D1 result row. BLOBs arrive as arrays of bytes.
fn value_of(json: Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(i64::from(b)),
        Json::Number(n) => n.as_i64().map_or_else(|| Value::Real(n.as_f64().unwrap_or(f64::NAN)), Value::Integer),
        Json::String(s) => Value::Text(s),
        Json::Array(items) if items.iter().all(|i| i.as_u64().is_some_and(|b| b <= 255)) => {
            Value::Blob(items.iter().filter_map(Json::as_u64).map(|b| b as u8).collect())
        }
        other => Value::Text(other.to_string()),
    }
}

fn query_body(sql: &str, params: &[Option<String>]) -> Json {
    json!({ "sql": sql, "params": params })
}

impl RawResult {
    fn into_output(self) -> Output {
        let RawRows { columns, rows } = self.results.unwrap_or_default();
        Output {
            columns: columns.into_iter().map(|name| Column { name, decltype: String::new() }).collect(),
            rows: rows.into_iter().map(|r| r.into_iter().map(value_of).collect()).collect(),
            affected: self.meta.map_or(0, |m| m.changes),
        }
    }
}

/// The token is needed for every request; check it before the first one.
fn token(params: &ConnectParams) -> DbResult<()> {
    if params.password().is_none_or(|t| t.trim().is_empty()) {
        return Err(DbError::new(ErrorKind::Auth, "Enter a Cloudflare API token.")
            .with_action(Some("Create one in the Cloudflare dashboard with the D1 Edit (or Read) permission.")));
    }
    Ok(())
}

/// `…/client/v4/accounts/{account}/d1/database`.
fn databases_url(http: &Http, params: &ConnectParams) -> DbResult<String> {
    let account = params
        .config
        .property(ConnConfig::D1_ACCOUNT)
        .filter(|a| a.len() == 32 && a.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| DbError::invalid("Enter the Cloudflare account ID (32 hexadecimal characters)."))?;
    Ok(format!("{}/client/v4/accounts/{account}/d1/database", http.origin))
}

/// Reads the API envelope, turning `errors` into a [`DbError`].
fn envelope<T: DeserializeOwned>(reply: Reply) -> DbResult<T> {
    let parsed: Option<Envelope<T>> = serde_json::from_value(reply.body).ok();
    let (errors, result) = parsed.map_or_else(|| (Vec::new(), None), |e| (e.errors, e.result));
    match (reply.status, result) {
        (200..300, Some(result)) => Ok(result),
        (status, _) => {
            let first = errors.into_iter().next();
            let code = first.as_ref().map(|e| e.code);
            let message = first.map_or_else(
                || {
                    if reply.text.trim().is_empty() {
                        format!("Cloudflare answered HTTP {status}.")
                    } else {
                        reply.text.trim().to_owned()
                    }
                },
                |e| e.message.trim_start_matches("D1_ERROR: ").to_owned(),
            );
            let (kind, action) = match (status, code) {
                (401 | 403, _) | (_, Some(10000)) => (
                    ErrorKind::Auth,
                    Some(
                        "Check the API token: it needs the D1 Edit (or Read) permission on this account. The saved token was not changed.",
                    ),
                ),
                (404, _) | (_, Some(7404)) => {
                    (ErrorKind::NotFound, Some("Check the account ID and the database ID or name."))
                }
                (429, _) => {
                    (ErrorKind::Server, Some("Cloudflare is limiting the request rate. Wait a moment and try again."))
                }
                _ => classify_message(&message),
            };
            let error = DbError::new(kind, message).with_action(action);
            Err(match code {
                Some(code) => error.with_code(code.to_string()),
                None => error,
            })
        }
    }
}

/// Every database of the account, or those named `name`.
async fn list(http: &Http, params: &ConnectParams, name: Option<&str>) -> DbResult<Vec<DatabaseInfo>> {
    token(params)?;
    let base = databases_url(http, params)?;
    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut query = vec![("per_page", PAGE_SIZE.to_string()), ("page", page.to_string())];
        if let Some(name) = name {
            query.push(("name", name.to_owned()));
        }
        let url = reqwest::Url::parse_with_params(&base, &query).map_err(|e| DbError::invalid(e.to_string()))?;
        let found: Vec<DatabaseInfo> = envelope(http.get(url.as_str(), Some(LIST_TIMEOUT)).await?)?;
        let last = found.len() < PAGE_SIZE;
        out.extend(found);
        if last {
            break;
        }
    }
    Ok(out)
}

impl D1 {
    /// Finds the database (by id, or by name in the account) behind `params`.
    pub async fn connect(params: &ConnectParams) -> DbResult<Arc<Self>> {
        token(params)?;
        let http = Http::new(params)?;
        let wanted = params.config.database.trim();
        let id = if is_uuid(wanted) {
            wanted.to_ascii_lowercase()
        } else {
            list(&http, params, Some(wanted))
                .await?
                .into_iter()
                .find(|d| d.name == wanted)
                .map(|d| d.uuid)
                .filter(|id| is_uuid(id))
                .ok_or_else(|| {
                    DbError::not_found(format!("The account has no D1 database named {wanted}."))
                        .with_action(Some("Load the list of databases, or enter the database ID."))
                })?
        };
        let base = format!("{}/{id}", databases_url(&http, params)?);
        Ok(Arc::new(Self { http, base }))
    }

    /// The account's databases, for the connection form.
    pub async fn databases(params: &ConnectParams) -> DbResult<Vec<DatabaseInfo>> {
        list(&Http::new(params)?, params, None).await
    }

    async fn raw(&self, body: &Json, timeout: Option<Duration>) -> DbResult<Vec<RawResult>> {
        envelope(self.http.post(&format!("{}/raw", self.base), body, timeout).await?)
    }
}

#[async_trait]
impl Transport for D1 {
    fn product(&self) -> &'static str {
        "Cloudflare D1"
    }

    fn http(&self) -> &Http {
        &self.http
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &D1Dialect
    }

    /// D1 keeps its own state in `_cf_*` tables, which queries may not read.
    fn internal_prefixes(&self) -> &'static [&'static str] {
        &["sqlite\\_", "\\_cf\\_"]
    }

    async fn query(&self, sql: &str, params: &[Option<String>], timeout: Option<Duration>) -> DbResult<Output> {
        let results = self.raw(&query_body(sql, params), timeout).await?;
        results.into_iter().next().map(RawResult::into_output).ok_or_else(|| DbError::internal("D1 sent no result"))
    }

    async fn transaction(&self, statements: &[BoundStatement]) -> DbResult<Vec<u64>> {
        let mut batch = Vec::with_capacity(statements.len() * 2);
        let mut positions = Vec::with_capacity(statements.len());
        for statement in statements {
            positions.push(batch.len());
            batch.push(query_body(&statement.sql, &statement.params));
            if let Some(expected) = statement.expect_rows {
                batch.push(query_body(GUARD, &[Some(expected.to_string())]));
            }
        }
        let guarded = batch.len() > statements.len();
        let results = match self.raw(&json!({ "batch": batch }), None).await {
            Err(e) if guarded && e.message.contains("malformed JSON") => {
                return Err(DbError::new(
                    ErrorKind::Constraint,
                    "A row did not change as expected. Nothing was saved; the row may have been changed or deleted by someone else.",
                ));
            }
            other => other?,
        };
        let changes: Vec<u64> = results.into_iter().map(|r| r.meta.map_or(0, |m| m.changes)).collect();
        Ok(positions.into_iter().map(|i| changes.get(i).copied().unwrap_or(0)).collect())
    }

    async fn script(&self, statements: &[&str], timeout: Option<Duration>) -> Option<DbResult<Vec<Output>>> {
        let batch: Vec<Json> = statements.iter().map(|sql| query_body(sql, &[])).collect();
        Some(
            self.raw(&json!({ "batch": batch }), timeout)
                .await
                .map(|results| results.into_iter().map(RawResult::into_output).collect()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_and_ids() {
        assert!(is_uuid("6c8a5d3e-4b1f-4f0e-9a5b-1c2d3e4f5a6b") && !is_uuid("shop") && !is_uuid("6c8a5d3e-4b1f"));
        assert_eq!(value_of(json!(9_223_372_036_854_775_807_i64)), Value::Integer(i64::MAX));
        assert_eq!(value_of(json!(1.5)), Value::Real(1.5));
        assert_eq!(value_of(json!([137, 80])), Value::Blob(vec![137, 80]));
        assert_eq!(value_of(json!({ "a": 1 })), Value::Text("{\"a\":1}".into()));
        assert_eq!(value_of(json!(true)), Value::Integer(1));
    }
}
