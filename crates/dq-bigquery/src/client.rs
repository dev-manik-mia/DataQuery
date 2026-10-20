//! The `BigQuery` REST API v2: query jobs (`jobs.insert`, `getQueryResults`
//! with page tokens, `jobs.cancel`, `jobs.get`) and the catalog resources
//! (projects, datasets, tables). Every request carries a bearer token from
//! [`Credentials`]; a refused token is renewed once.

use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::{DbError, DbResult, ErrorKind};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::auth::Credentials;

/// Google's API host; an `endpoint` property (an emulator) replaces it.
pub const API_ROOT: &str = "https://bigquery.googleapis.com";
/// How long one `getQueryResults` call waits for a running job.
const WAIT_MS: u32 = 10_000;
/// Listing pages are followed up to this many items.
pub const MAX_LISTED: usize = 10_000;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RangeType {
    #[serde(rename = "type")]
    pub kind: String,
}

/// A column of a result or table schema; `RECORD` fields nest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub mode: Option<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    pub description: Option<String>,
    pub default_value_expression: Option<String>,
    pub max_length: Option<String>,
    pub precision: Option<String>,
    pub scale: Option<String>,
    pub range_element_type: Option<RangeType>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Schema {
    #[serde(default)]
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    pub project_id: String,
    pub job_id: String,
    pub location: Option<String>,
}

/// A `getQueryResults` answer: rows of the finished job, page by page.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPage {
    #[serde(default)]
    pub job_complete: bool,
    pub schema: Option<Schema>,
    #[serde(default)]
    pub rows: Vec<Value>,
    pub page_token: Option<String>,
    pub num_dml_affected_rows: Option<String>,
}

impl QueryPage {
    pub fn affected(&self) -> Option<u64> {
        self.num_dml_affected_rows.as_deref().and_then(|n| n.parse().ok())
    }
}

/// One query job to start.
#[derive(Debug, Default)]
pub struct JobSpec<'a> {
    pub sql: &'a str,
    /// `queryParameters`, all named or all positional.
    pub params: Vec<Value>,
    pub named: bool,
    pub dry_run: bool,
    /// Server-side limit (`jobTimeoutMs`); 0 for none.
    pub timeout_secs: u32,
    /// Runs in this session (temporary tables, an open transaction).
    pub session: Option<&'a str>,
    pub create_session: bool,
    /// Resolves unqualified names in the connection's default dataset.
    pub default_dataset: bool,
    /// Where the job runs (`US`, `EU`, `europe-west1`); `None` lets `BigQuery` pick
    /// it from the datasets the query reads.
    pub location: Option<&'a str>,
}

pub struct Client {
    http: reqwest::Client,
    /// `…/bigquery/v2/`.
    api: Url,
    pub project: String,
    pub dataset: Option<String>,
    pub location: Option<String>,
    pub tls: bool,
    credentials: Credentials,
}

fn status_kind(status: u16) -> ErrorKind {
    match status {
        401 => ErrorKind::Auth,
        403 => ErrorKind::Permission,
        404 => ErrorKind::NotFound,
        409 => ErrorKind::Constraint,
        400 => ErrorKind::InvalidInput,
        _ => ErrorKind::Server,
    }
}

/// An error from `BigQuery`'s `{reason, message}` (API errors and failed jobs alike).
pub fn bigquery_error(status: Option<u16>, reason: &str, message: &str) -> DbError {
    let (kind, action) = match reason {
        "notFound" => {
            (ErrorKind::NotFound, Some("Check the project, dataset and table names; table names are case-sensitive."))
        }
        "accessDenied" | "forbidden" => (
            ErrorKind::Permission,
            Some(
                "The account needs a BigQuery role on this project or dataset (for example BigQuery Data Viewer and Job User).",
            ),
        ),
        "duplicate" => (ErrorKind::Constraint, None),
        "stopped" => (ErrorKind::Cancelled, None),
        "timeout" => (ErrorKind::Timeout, Some("Narrow the query or raise the statement timeout in Settings.")),
        "billingNotEnabled" | "billingTierLimitExceeded" => {
            (ErrorKind::Permission, Some("Enable billing for the project, or choose another billing project."))
        }
        "quotaExceeded" | "rateLimitExceeded" => {
            (ErrorKind::Server, Some("A BigQuery quota was reached; wait a moment and run it again."))
        }
        "invalidQuery" if message.starts_with("Not found") => (ErrorKind::NotFound, None),
        "invalidQuery" => (ErrorKind::Syntax, None),
        "invalid" => (ErrorKind::InvalidInput, None),
        // Emulators report every failure as an internal job error.
        _ if message.contains("Syntax error") => (ErrorKind::Syntax, None),
        _ if message.contains("not found") || message.contains("Not found") => (ErrorKind::NotFound, None),
        _ => (status.map_or(ErrorKind::Server, status_kind), None),
    };
    let mut error = DbError::new(kind, message).with_action(action);
    if !reason.is_empty() {
        error = error.with_code(reason);
    }
    error
}

/// The error of an API answer: `{"error": {"code", "message", "errors": [{"reason"}]}}`.
fn api_error(status: u16, body: &str) -> DbError {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let error = &parsed["error"];
    let reason = error["errors"][0]["reason"].as_str().or_else(|| error["status"].as_str()).unwrap_or_default();
    match error["message"].as_str() {
        Some(message) => bigquery_error(Some(status), reason, message),
        None => DbError::new(status_kind(status), format!("BigQuery answered HTTP {status}.")),
    }
}

/// The error of a finished job (`status.errorResult`), if it failed.
pub fn job_failure(job: &Value) -> Option<DbError> {
    let result = job.pointer("/status/errorResult")?;
    Some(bigquery_error(
        None,
        result["reason"].as_str().unwrap_or_default(),
        result["message"].as_str().unwrap_or("The query failed."),
    ))
}

/// A fresh job id; ours, so a job can be cancelled before `BigQuery` answers.
pub fn new_job_id() -> String {
    format!("dataquery_{}", uuid::Uuid::new_v4().simple())
}

fn network(e: &reqwest::Error) -> DbError {
    let detail = std::error::Error::source(e).map(ToString::to_string).unwrap_or_default();
    let text = format!("{e} {detail}");
    let kind = if e.is_timeout() {
        ErrorKind::Timeout
    } else if text.contains("certificate") || text.to_ascii_lowercase().contains("tls") {
        ErrorKind::Tls
    } else {
        ErrorKind::Network
    };
    DbError::new(kind, format!("BigQuery could not be reached: {}", text.trim()))
}

/// `https://bigquery.googleapis.com/bigquery/v2/`, or the endpoint's.
pub fn api_url(params: &ConnectParams) -> DbResult<Url> {
    let root = params.config.property("endpoint").unwrap_or(API_ROOT).trim_end_matches('/');
    Url::parse(&format!("{root}/bigquery/v2/")).map_err(|_| DbError::invalid("The endpoint is not a valid URL."))
}

impl Client {
    pub async fn open(params: &ConnectParams) -> DbResult<Self> {
        let c = &params.config;
        let api = api_url(params)?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(u64::from(c.connect_timeout_secs)))
            .user_agent(concat!("DataQuery/", env!("CARGO_PKG_VERSION")))
            .use_rustls_tls()
            .gzip(true);
        if matches!(c.ssl_mode, SslMode::Require | SslMode::Prefer) {
            builder = builder.danger_accept_invalid_certs(true);
        }
        Ok(Self {
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            tls: api.scheme() == "https",
            api,
            project: c.database.trim().to_owned(),
            dataset: c.property("dataset").map(str::to_owned),
            location: c.property("location").map(str::to_owned),
            credentials: Credentials::open(params).await?,
        })
    }

    pub fn account(&self) -> Option<&str> {
        self.credentials.account()
    }

    /// `…/bigquery/v2/projects/<project>/<segments…>`, each segment escaped;
    /// an empty `project` addresses the project list.
    fn url(&self, project: &str, segments: &[&str]) -> Url {
        let mut url = self.api.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().push("projects");
            if !project.is_empty() {
                path.push(project).extend(segments);
            }
        }
        url
    }

    async fn request(&self, method: Method, url: Url, query: &[(&str, &str)], body: Option<&Value>) -> DbResult<Value> {
        for attempt in 0..2 {
            let mut request = self.http.request(method.clone(), url.clone()).query(query);
            if let Some(token) = self.credentials.token(&self.http).await? {
                request = request.bearer_auth(token);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|e| network(&e))?;
            let status = response.status().as_u16();
            let text = response.text().await.map_err(|e| network(&e))?;
            if status == 401 && attempt == 0 && !self.credentials.is_anonymous() {
                self.credentials.invalidate().await;
                continue;
            }
            if !(200..300).contains(&status) {
                let error = api_error(status, &text);
                return Err(if status == 401 {
                    error.with_action(Some("Check the key file or sign in again with `gcloud auth login`."))
                } else {
                    error
                });
            }
            return if text.trim().is_empty() {
                Ok(Value::Null)
            } else {
                serde_json::from_str(&text)
                    .map_err(|_| DbError::new(ErrorKind::Server, "BigQuery sent an unreadable answer."))
            };
        }
        Err(DbError::new(ErrorKind::Auth, "BigQuery refused the access token."))
    }

    pub async fn get(&self, project: &str, segments: &[&str], query: &[(&str, &str)]) -> DbResult<Value> {
        self.request(Method::GET, self.url(project, segments), query, None).await
    }

    /// Every item of a paged listing (`datasets`, `tables`, …), up to `limit`.
    pub async fn list(&self, project: &str, segments: &[&str], key: &str, limit: usize) -> DbResult<Vec<Value>> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("maxResults", "1000")];
            if let Some(t) = token.as_deref() {
                query.push(("pageToken", t));
            }
            let mut page = self.get(project, segments, &query).await?;
            if let Value::Array(found) = page[key].take() {
                items.extend(found);
            }
            token = page["nextPageToken"].as_str().map(str::to_owned);
            if token.is_none() || items.len() >= limit {
                items.truncate(limit);
                return Ok(items);
            }
        }
    }

    /// Starts a query job with our own id, so it can be cancelled at once.
    pub async fn insert_job(&self, spec: &JobSpec<'_>, job_id: &str) -> DbResult<Value> {
        let mut query = json!({ "query": spec.sql, "useLegacySql": false });
        if !spec.params.is_empty() {
            query["parameterMode"] = json!(if spec.named { "NAMED" } else { "POSITIONAL" });
            query["queryParameters"] = Value::Array(spec.params.clone());
        }
        if spec.default_dataset
            && let Some(dataset) = &self.dataset
        {
            query["defaultDataset"] = json!({ "projectId": self.project, "datasetId": dataset });
        }
        if spec.create_session {
            query["createSession"] = json!(true);
        }
        if let Some(session) = spec.session {
            query["connectionProperties"] = json!([{ "key": "session_id", "value": session }]);
        }
        let mut reference = json!({ "projectId": self.project, "jobId": job_id });
        if let Some(location) = spec.location {
            reference["location"] = json!(location);
        }
        let mut configuration = json!({ "query": query });
        if spec.dry_run {
            configuration["dryRun"] = json!(true);
        }
        if spec.timeout_secs > 0 {
            configuration["jobTimeoutMs"] = json!((u64::from(spec.timeout_secs) * 1000).to_string());
        }
        let body = json!({ "jobReference": reference, "configuration": configuration });
        let job = self.request(Method::POST, self.url(&self.project, &["jobs"]), &[], Some(&body)).await?;
        match job_failure(&job) {
            Some(error) => Err(error),
            None => Ok(job),
        }
    }

    /// Runs a job named `job_id` and waits for its first page of at most `rows`
    /// rows. `started` learns the job, with the location `BigQuery` chose, as soon as it exists.
    pub async fn run(
        &self,
        spec: &JobSpec<'_>,
        job_id: &str,
        rows: u32,
        started: impl FnOnce(&JobRef) + Send,
    ) -> DbResult<(JobRef, QueryPage)> {
        let job = self.insert_job(spec, job_id).await?;
        let job_ref =
            serde_json::from_value(job["jobReference"].clone()).unwrap_or_else(|_| self.job_ref(job_id, spec));
        started(&job_ref);
        let page = self.wait(&job_ref, rows).await?;
        Ok((job_ref, page))
    }

    /// The reference a job named `job_id` gets before `BigQuery` confirms it.
    pub fn job_ref(&self, job_id: &str, spec: &JobSpec<'_>) -> JobRef {
        JobRef {
            project_id: self.project.clone(),
            job_id: job_id.to_owned(),
            location: spec.location.map(str::to_owned),
        }
    }

    fn page_query<'a>(job: &'a JobRef, max: &'a str, token: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut query = vec![("maxResults", max), ("formatOptions.useInt64Timestamp", "true")];
        if let Some(location) = job.location.as_deref() {
            query.push(("location", location));
        }
        if let Some(token) = token {
            query.push(("pageToken", token));
        }
        query
    }

    /// Waits for the job, then returns its first page of at most `max_results` rows.
    pub async fn wait(&self, job: &JobRef, max_results: u32) -> DbResult<QueryPage> {
        let max = max_results.to_string();
        let wait = WAIT_MS.to_string();
        loop {
            let mut query = Self::page_query(job, &max, None);
            query.push(("timeoutMs", &wait));
            let page = self.results(job, &query).await?;
            if page.job_complete {
                return Ok(page);
            }
        }
    }

    /// The page after `token`.
    pub async fn page(&self, job: &JobRef, token: &str, max_results: u32) -> DbResult<QueryPage> {
        let max = max_results.to_string();
        self.results(job, &Self::page_query(job, &max, Some(token))).await
    }

    async fn results(&self, job: &JobRef, query: &[(&str, &str)]) -> DbResult<QueryPage> {
        let value = self.get(&job.project_id, &["queries", &job.job_id], query).await?;
        serde_json::from_value(value)
            .map_err(|_| DbError::new(ErrorKind::Server, "BigQuery sent an unreadable result."))
    }

    /// The job resource, with its statistics.
    pub async fn job(&self, job: &JobRef) -> DbResult<Value> {
        let location: Vec<(&str, &str)> = job.location.as_deref().map(|l| ("location", l)).into_iter().collect();
        self.get(&job.project_id, &["jobs", &job.job_id], &location).await
    }

    /// Asks `BigQuery` to stop a job; it may still finish.
    pub async fn cancel(&self, job: &JobRef) -> DbResult<()> {
        let location: Vec<(&str, &str)> = job.location.as_deref().map(|l| ("location", l)).into_iter().collect();
        let url = self.url(&job.project_id, &["jobs", &job.job_id, "cancel"]);
        self.request(Method::POST, url, &location, None).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::config::ConnConfig;
    use dq_core::engine::Engine;

    #[test]
    fn maps_errors_and_builds_urls() {
        let body =
            r#"{"error": {"code": 404, "message": "Not found: Table p:d.t", "errors": [{"reason": "notFound"}]}}"#;
        let e = api_error(404, body);
        assert_eq!((e.kind, e.code.as_deref()), (ErrorKind::NotFound, Some("notFound")));
        assert_eq!(
            api_error(400, r#"{"error": {"message": "Syntax error: x", "errors": [{"reason": "invalidQuery"}]}}"#).kind,
            ErrorKind::Syntax
        );
        assert_eq!(
            api_error(403, r#"{"error": {"message": "Access Denied", "errors": [{"reason": "accessDenied"}]}}"#).kind,
            ErrorKind::Permission
        );
        assert_eq!(api_error(502, "<html>").kind, ErrorKind::Server);
        let failed = json!({ "status": { "errorResult": { "reason": "stopped", "message": "Job cancelled" } } });
        assert_eq!(job_failure(&failed).map(|e| e.kind), Some(ErrorKind::Cancelled));
        let mut config = ConnConfig { engine: Engine::Bigquery, database: "p".into(), ..ConnConfig::default() };
        let url = |c: &ConnConfig| api_url(&ConnectParams::new(c.clone(), None)).unwrap().to_string();
        assert_eq!(url(&config), "https://bigquery.googleapis.com/bigquery/v2/");
        config.properties.insert("endpoint".into(), "http://localhost:59050/".into());
        assert_eq!(url(&config), "http://localhost:59050/bigquery/v2/");
    }
}
