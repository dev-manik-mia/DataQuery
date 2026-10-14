//! An open Elasticsearch / `OpenSearch` connection.
//!
//! REST is stateless, so the "session" is the client, the rows of the last
//! editor result that did not fit the row limit, and the `X-Opaque-Id` of the
//! running request (to cancel its search task). Read-only profiles are
//! enforced here: Elasticsearch has no read-only session.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex as SyncMutex;
use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter};
use dq_core::document::{
    DocField, DocIndex, Document, DocumentFeatures, DocumentPage, DocumentRows, DocumentStore, FindRequest,
};
use dq_core::explain::ExplainResult;
use dq_core::monitor::Monitor;
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{ColumnInfo, Dependencies, Dependency, ModelColumn, ModelTable, SchemaModel, TableStructure};
use dq_core::sql::querydsl::{Request, parse_request};
use dq_core::sql::{Dialect, QueryDslDialect, SqlFlavor, split_statements};
use dq_core::table::RowCount;
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

use crate::catalog::{self, segment};
use crate::client::{Body, Call, Client};
use crate::explain;
use crate::monitor::Tasks;
use crate::values::{self, FieldTypes};

/// Hits per scroll page in exports.
const SCROLL_BATCH: usize = 1_000;
/// `index.max_result_window`: `from + size` beyond this needs `search_after`.
const MAX_WINDOW: u64 = 10_000;

pub const FEATURES: DocumentFeatures = DocumentFeatures {
    language: "Query DSL",
    filter_example: r#"{ "match": { "status": "paid" } }  or  status:paid AND total:>100"#,
    projection_example: r#""email", "total""#,
    sort: true,
    aggregation: false,
    validation: false,
    text_format: "json",
    new_document: "{\n}",
    filtered_count: true,
};

pub struct EsSession {
    pub(crate) client: Client,
    pub(crate) info: ServerInfo,
    pub(crate) tasks: Tasks,
    /// Rows of the last editor result beyond the row limit.
    cursor: Mutex<VecDeque<Row>>,
    /// `X-Opaque-Id` of the running editor request.
    running: SyncMutex<Option<String>>,
    cancelled: Notify,
    /// Mapped field types by index or alias, for document type labels.
    types: SyncMutex<HashMap<String, FieldTypes>>,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn no_params(params: Params<'_>) -> DbResult<()> {
    if params.is_empty() {
        Ok(())
    } else {
        Err(DbError::invalid("Elasticsearch requests take no parameters; write the values into the body."))
    }
}

/// `GET _search`, `POST orders/_doc` …: the method and the first `_endpoint` (or the path).
fn command(req: &Request) -> String {
    let segments = req.segments();
    let endpoint = segments.iter().find(|s| s.starts_with('_')).copied().unwrap_or(req.path.as_str());
    format!("{} {endpoint}", req.method)
}

fn is_search(req: &Request) -> bool {
    req.segments().last().is_some_and(|s| *s == "_search") && matches!(req.method.as_str(), "GET" | "POST")
}

fn is_ndjson(req: &Request) -> bool {
    req.segments().iter().any(|s| matches!(*s, "_bulk" | "_msearch"))
}

/// The index part of a path (`orders` in `/orders/_search`), if any.
fn target(req: &Request) -> Option<String> {
    req.segments().first().filter(|s| !s.starts_with('_')).map(|s| (*s).to_owned())
}

fn parse_json(text: &str, what: &str) -> DbResult<Value> {
    serde_json::from_str(text)
        .map_err(|e| DbError::new(ErrorKind::Syntax, format!("The {what} is not valid JSON: {e}")))
}

/// The search body of a request (`{}` when empty).
fn body_object(req: &Request) -> DbResult<Map<String, Value>> {
    if req.body.is_empty() {
        return Ok(Map::new());
    }
    match parse_json(&req.body, "request body")? {
        Value::Object(m) => Ok(m),
        _ => Err(DbError::invalid("The search body must be a JSON object.")),
    }
}

/// The Documents-view filter: empty → `match_all`, JSON → a query, else a Lucene query string.
pub fn filter_query(filter: &str) -> DbResult<Value> {
    let f = filter.trim();
    if f.is_empty() {
        Ok(json!({ "match_all": {} }))
    } else if f.starts_with('{') {
        parse_json(f, "filter")
    } else {
        Ok(json!({ "query_string": { "query": f } }))
    }
}

/// `"email", total` → `["email", "total"]`.
pub fn projection(text: &str) -> Option<Value> {
    let fields: Vec<Value> = text
        .split(',')
        .map(|f| f.trim().trim_matches('"').trim())
        .filter(|f| !f.is_empty())
        .map(|f| Value::String(f.to_owned()))
        .collect();
    (!fields.is_empty()).then_some(Value::Array(fields))
}

/// JSON sort (`{ "created_at": "desc" }` or `[…]`), or `field desc, other`.
pub fn sort(text: &str) -> DbResult<Option<Value>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.starts_with('{') || t.starts_with('[') {
        return parse_json(t, "sort").map(Some);
    }
    let parts: Vec<Value> = t
        .split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let mut words = p.split_whitespace();
            let field = words.next().unwrap_or_default();
            let order = words.next().map_or("asc", |o| if o.eq_ignore_ascii_case("desc") { "desc" } else { "asc" });
            json!({ field: order })
        })
        .collect();
    Ok(Some(Value::Array(parts)))
}

/// Where a Documents-view page starts: an offset, or the sort values of the last hit.
#[derive(Debug, Clone, PartialEq)]
pub enum Cursor {
    From(u64),
    After(Value),
}

impl Cursor {
    pub fn parse(text: Option<&str>) -> DbResult<Self> {
        let Some(text) = text.filter(|t| !t.is_empty()) else { return Ok(Self::From(0)) };
        let v: Value = serde_json::from_str(text).map_err(|_| DbError::invalid("The page cursor is not valid."))?;
        if let Some(from) = v["from"].as_u64() {
            Ok(Self::From(from))
        } else if v["after"].is_array() {
            Ok(Self::After(v["after"].clone()))
        } else {
            Err(DbError::invalid("The page cursor is not valid."))
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Self::From(n) => json!({ "from": n }).to_string(),
            Self::After(v) => json!({ "after": v }).to_string(),
        }
    }
}

/// The `_search` body for a Documents-view request; `size` asks one extra hit to learn whether more exist.
pub fn find_body(request: &FindRequest, cursor: &Cursor) -> DbResult<Map<String, Value>> {
    let mut body = Map::new();
    body.insert("query".into(), filter_query(&request.filter)?);
    if let Some(p) = projection(&request.projection) {
        body.insert("_source".into(), p);
    }
    let sorted = sort(&request.sort)?;
    if let Some(s) = &sorted {
        body.insert("sort".into(), s.clone());
    }
    match cursor {
        Cursor::From(from) => {
            if from + u64::from(request.limit()) > MAX_WINDOW {
                return Err(DbError::invalid(
                    "Pages past 10,000 documents need a sort: add one (e.g. { \"timestamp\": \"desc\" }) to page further.",
                ));
            }
            if *from > 0 {
                body.insert("from".into(), json!(from));
            }
        }
        Cursor::After(after) => {
            if sorted.is_none() {
                return Err(DbError::invalid("Paging past 10,000 documents needs a sort."));
            }
            body.insert("search_after".into(), after.clone());
        }
    }
    body.insert("size".into(), json!(request.limit() + 1));
    body.insert("track_total_hits".into(), Value::Bool(false));
    Ok(body)
}

/// The cursor after a page of `shown` hits (the last shown hit's sort values beyond the window).
pub fn next_cursor(cursor: &Cursor, shown: &[Value], has_more: bool) -> Option<Cursor> {
    if !has_more {
        return None;
    }
    let after = || shown.last().and_then(|h| h["sort"].as_array().cloned()).map(|s| Cursor::After(Value::Array(s)));
    match cursor {
        Cursor::From(from) => {
            let next = from + shown.len() as u64;
            if next + shown.len() as u64 <= MAX_WINDOW {
                Some(Cursor::From(next))
            } else {
                after().or(Some(Cursor::From(next)))
            }
        }
        Cursor::After(_) => after(),
    }
}

/// Text to an indexed JSON value by mapped type; empty type infers numbers, booleans, objects and arrays.
pub fn import_value(text: &str, data_type: &str) -> Value {
    let number = || {
        let t = text.trim();
        t.parse::<i64>()
            .map(Value::from)
            .ok()
            .or_else(|| t.parse::<u64>().map(Value::from).ok())
            .or_else(|| t.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number))
    };
    match data_type {
        t if values::is_numeric_type(t) => number().unwrap_or_else(|| Value::String(text.to_owned())),
        "boolean" => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Value::Bool(true),
            "false" | "0" | "no" => Value::Bool(false),
            _ => Value::String(text.to_owned()),
        },
        "object" | "nested" | "flattened" => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
        }
        "" => {
            let t = text.trim();
            if matches!(t, "true" | "false") {
                Value::Bool(t == "true")
            } else if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<Value>(t).is_ok() {
                serde_json::from_str(t).unwrap_or(Value::Null)
            } else {
                number().filter(|_| !t.is_empty()).unwrap_or_else(|| Value::String(text.to_owned()))
            }
        }
        _ => Value::String(text.to_owned()),
    }
}

impl EsSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let client = Client::new(params)?;
        let root = client.get("/").await?;
        let latency_ms = elapsed(started);
        let version = &root["version"];
        let number = version["number"].as_str().unwrap_or("?");
        let product = if version["distribution"] == "opensearch" { "OpenSearch" } else { "Elasticsearch" };
        // Security may be off (anonymous) or the user may not see the endpoint.
        let user = client
            .get("/_security/_authenticate")
            .await
            .ok()
            .and_then(|a| a["username"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                if params.config.username.is_empty() { "anonymous".into() } else { params.config.username.clone() }
            });
        let config = &params.config;
        let tls = config.ssl_mode != dq_core::config::SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version: format!("{product} {number}"),
            latency_ms,
            tls: tls.then(|| TlsInfo {
                version: None,
                verified: config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user,
            database: root["cluster_name"].as_str().unwrap_or("cluster").to_owned(),
            read_only: config.read_only,
        };
        Ok(Self {
            client,
            info,
            tasks: Tasks::default(),
            cursor: Mutex::new(VecDeque::new()),
            running: SyncMutex::new(None),
            cancelled: Notify::new(),
            types: SyncMutex::new(HashMap::new()),
        })
    }

    fn set_running(&self, id: Option<String>) {
        if let Ok(mut r) = self.running.lock() {
            *r = id;
        }
    }

    /// Sends `req`, stopping early on cancel. `timeout_secs` 0 means none.
    async fn send(&self, req: &Request, timeout_secs: u32) -> DbResult<Body> {
        let opaque = format!("dataquery-{}", uuid::Uuid::new_v4());
        self.set_running(Some(opaque.clone()));
        let call = Call {
            method: &req.method,
            path: &req.path,
            body: &req.body,
            ndjson: is_ndjson(req),
            opaque_id: Some(&opaque),
            timeout_secs,
        };
        let outcome = tokio::select! {
            r = self.client.call(&call) => r,
            () = self.cancelled.notified() => Err(DbError::new(ErrorKind::Cancelled, "The request was cancelled.")),
        };
        self.set_running(None);
        outcome
    }

    /// Parses one editor statement and refuses writes on read-only connections.
    fn statement(&self, sql: &str) -> DbResult<Request> {
        let req = parse_request(sql)?;
        if self.info.read_only && split_statements(sql, SqlFlavor::QueryDsl).iter().any(|s| s.writes) {
            return Err(read_only_error());
        }
        Ok(req)
    }

    async fn field_types(&self, index: &str) -> FieldTypes {
        if let Some(t) = self.types.lock().ok().and_then(|m| m.get(index).cloned()) {
            return t;
        }
        let types = catalog::field_types(&self.client, index).await.unwrap_or_default();
        if let Ok(mut m) = self.types.lock() {
            m.insert(index.to_owned(), types.clone());
        }
        types
    }

    fn document(hit: &Value, target: &str, types: &FieldTypes) -> Document {
        let id = hit["_id"].as_str().unwrap_or_default();
        let index = hit["_index"].as_str().unwrap_or_default();
        let mut label = format!("_id: {}", Value::String(id.to_owned()));
        if index != target {
            label.push_str(&format!(" · {index}"));
        }
        let source = hit["_source"].as_object().cloned().unwrap_or_default();
        Document::new(
            id.to_owned(),
            label,
            None,
            serde_json::to_string_pretty(&Value::Object(source.clone())).unwrap_or_default(),
            values::object_fields(&source, types, CellLimits::GRID),
        )
    }

    /// Every hit of a search, through the scroll API, as rows.
    async fn scroll(&self, req: &Request, on_batch: &mut BatchFn<'_>) -> DbResult<u64> {
        let mut body = body_object(req)?;
        body.insert("size".into(), json!(SCROLL_BATCH));
        body.remove("from");
        let index = target(req).map_or_else(String::new, |t| format!("/{}", segment(&t)));
        let none = FieldTypes::new();
        let mut rows = DocumentRows::new(on_batch);
        let mut page = self.client.post(&format!("{index}/_search?scroll=1m"), &Value::Object(body)).await?;
        let mut scroll_id = page["_scroll_id"].as_str().map(str::to_owned);
        let result = loop {
            let hits = page["hits"]["hits"].as_array().cloned().unwrap_or_default();
            if hits.is_empty() {
                break Ok(rows.rows());
            }
            let batch: Vec<Vec<DocField>> =
                hits.iter().map(|h| values::hit_fields(h, &none, CellLimits::FULL)).collect();
            if let Err(e) = rows.push(&batch) {
                break Err(e);
            }
            let Some(id) = scroll_id.clone() else { break Ok(rows.rows()) };
            page = match self.client.post("/_search/scroll", &json!({ "scroll": "1m", "scroll_id": id })).await {
                Ok(p) => p,
                Err(e) => break Err(e),
            };
            scroll_id = page["_scroll_id"].as_str().map(str::to_owned).or(scroll_id);
        };
        if let Some(id) = scroll_id {
            let body = json!({ "scroll_id": id }).to_string();
            let _ = self
                .client
                .call(&Call { method: "DELETE", path: "/_search/scroll", body: &body, ..Call::default() })
                .await;
        }
        result
    }
}

#[async_trait]
impl Session for EsSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &QueryDslDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let cluster = &self.info.database;
        match parent {
            None => {
                let mut node =
                    Node::container(ObjectRef::new(NodeKind::Database, cluster.clone(), None, cluster.clone()));
                node.current = true;
                Ok(vec![node])
            }
            Some(p) if p.kind == NodeKind::Database => catalog::children(&self.client, cluster).await,
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let needle = query.to_lowercase();
        let mut nodes = catalog::children(&self.client, &self.info.database).await?;
        nodes.retain(|n| n.object.name.to_lowercase().contains(&needle));
        nodes.truncate(limit as usize);
        Ok(nodes)
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        no_params(params)?;
        let started = Instant::now();
        let req = self.statement(sql)?;
        self.cursor.lock().await.clear();
        let body = self.send(&req, options.timeout_secs).await?;
        let grid = values::grid(&body, CellLimits::GRID);
        let mut rows: VecDeque<Row> = grid.rows.into();
        let first: Vec<Row> = rows.drain(..rows.len().min(options.row_limit as usize)).collect();
        let has_more = !rows.is_empty();
        *self.cursor.lock().await = rows;
        Ok(StatementResult {
            columns: grid.columns,
            rows: first,
            has_more,
            command: command(&req),
            affected: values::affected(body.json()),
            duration_ms: elapsed(started),
            notices: grid.notices,
        })
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut cursor = self.cursor.lock().await;
        let take = cursor.len().min(limit as usize);
        let rows: Vec<Row> = cursor.drain(..take).collect();
        Ok(RowsPage { rows, has_more: !cursor.is_empty(), duration_ms: elapsed(started) })
    }

    /// Cancels the running request's search task and stops waiting for it.
    async fn cancel(&self) -> DbResult<bool> {
        let Some(opaque) = self.running.lock().ok().and_then(|r| r.clone()) else { return Ok(false) };
        if let Ok(tasks) = self.client.get("/_tasks?detailed=true").await {
            for node in tasks["nodes"].as_object().into_iter().flatten().map(|(_, n)| n) {
                for (id, task) in node["tasks"].as_object().into_iter().flatten() {
                    if task["headers"]["X-Opaque-Id"] == opaque.as_str() && task["cancellable"] == true {
                        let path = format!("/_tasks/{}/_cancel", segment(id));
                        let _ = self.client.call(&Call { method: "POST", path: &path, ..Call::default() }).await;
                    }
                }
            }
        }
        self.cancelled.notify_waiters();
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        catalog::structure(&self.client, object).await
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        catalog::ddl(&self.client, object).await
    }

    async fn dependencies(&self, object: &ObjectRef) -> DbResult<Dependencies> {
        let pairs = catalog::aliases(&self.client).await?;
        let node = |kind, name: &str| ObjectRef::new(kind, self.info.database.clone(), None, name);
        let mut deps = Dependencies::default();
        for (alias, index) in pairs {
            if object.kind == NodeKind::View && alias == object.name {
                deps.uses.push(Dependency { object: node(NodeKind::Table, &index), relation: "alias of".into() });
            } else if object.kind == NodeKind::Table && index == object.name {
                deps.used_by.push(Dependency { object: node(NodeKind::View, &alias), relation: "alias".into() });
            }
        }
        Ok(deps)
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let all = self.client.get("/_mapping").await?;
        let mut model = SchemaModel::default();
        let mut names: Vec<&String> =
            all.as_object().into_iter().flatten().map(|(k, _)| k).filter(|k| !k.starts_with('.')).collect();
        names.sort();
        for name in names {
            if model.tables.len() >= SchemaModel::MAX_TABLES {
                model.truncated = true;
                break;
            }
            let columns = all[name.as_str()]["mappings"]["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(field, def)| ModelColumn {
                    name: field.clone(),
                    data_type: def["type"].as_str().unwrap_or("object").to_owned(),
                    nullable: true,
                    primary_key: false,
                })
                .collect();
            model
                .tables
                .push(ModelTable { object: ObjectRef::new(NodeKind::Table, database, None, name.clone()), columns });
        }
        Ok(model)
    }

    async fn read_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
        no_params(params)?;
        let req = self.statement(sql)?;
        let grid = values::grid(
            &self
                .client
                .call(&Call {
                    method: &req.method,
                    path: &req.path,
                    body: &req.body,
                    ndjson: is_ndjson(&req),
                    ..Call::default()
                })
                .await?,
            limits,
        );
        Ok((grid.columns, grid.rows))
    }

    /// Exports: searches page through every hit with the scroll API; other requests return one response.
    async fn stream_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        no_params(params)?;
        let req = self.statement(sql)?;
        if is_search(&req) {
            return self.scroll(&req, on_batch).await;
        }
        let body = self
            .client
            .call(&Call {
                method: &req.method,
                path: &req.path,
                body: &req.body,
                ndjson: is_ndjson(&req),
                ..Call::default()
            })
            .await?;
        let grid = values::grid(&body, CellLimits::FULL);
        let n = grid.rows.len() as u64;
        on_batch(&grid.columns, grid.rows)?;
        Ok(n)
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        let rows = catalog::indices(&self.client, &object.name).await?;
        Ok(rows.iter().map(|r| r.docs).sum())
    }

    async fn run_in_transaction(&self, _database: &str, _statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        Err(DbError::unsupported("Elasticsearch has no transactions; send the requests from the editor."))
    }

    /// Explain profiles the search (it runs); a plain explain validates the query and shows the Lucene query.
    async fn explain(
        &self,
        sql: &str,
        params: Params<'_>,
        analyze: bool,
        timeout_secs: u32,
    ) -> DbResult<ExplainResult> {
        no_params(params)?;
        let req = self.statement(sql)?;
        if !is_search(&req) {
            return Err(DbError::unsupported("Only searches (`_search`) have query plans."));
        }
        let mut body = body_object(&req)?;
        let index = target(&req).map_or_else(String::new, |t| format!("/{}", segment(&t)));
        if analyze {
            body.insert("profile".into(), Value::Bool(true));
            let text = Value::Object(body).to_string();
            let path = format!("{index}/_search");
            let response = self.send(&Request { method: "POST".into(), path, body: text }, timeout_secs).await?;
            return explain::profile_plan(response.json(), true);
        }
        let query = body.remove("query").unwrap_or_else(|| json!({ "match_all": {} }));
        let response =
            self.client.post(&format!("{index}/_validate/query?explain=true"), &json!({ "query": query })).await?;
        explain::validate_plan(&response)
    }

    fn monitor(&self) -> Option<&dyn Monitor> {
        Some(self)
    }

    fn documents(&self) -> Option<&dyn DocumentStore> {
        Some(self)
    }

    async fn close(&self) {}
}

fn read_only_documents() -> DbError {
    DbError::unsupported("Edit documents with requests in the editor (e.g. POST /index/_update/<id>).")
}

#[async_trait]
impl DocumentStore for EsSession {
    fn features(&self) -> DocumentFeatures {
        FEATURES
    }

    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let cursor = Cursor::parse(request.cursor.as_deref())?;
        let body = find_body(request, &cursor)?;
        let index = &request.object.name;
        let (path, body) = (format!("/{}/_search", segment(index)), Value::Object(body));
        let (response, types) = tokio::join!(self.client.post(&path, &body), self.field_types(index));
        let response = response?;
        let mut hits = response["hits"]["hits"].as_array().cloned().unwrap_or_default();
        let has_more = hits.len() > request.limit() as usize;
        hits.truncate(request.limit() as usize);
        Ok(DocumentPage {
            documents: hits.iter().map(|h| Self::document(h, index, &types)).collect(),
            next: next_cursor(&cursor, &hits, has_more).map(|c| c.encode()),
            duration_ms: elapsed(started),
        })
    }

    async fn count(&self, object: &ObjectRef, filter: &str) -> DbResult<Option<RowCount>> {
        let response = self
            .client
            .post(&format!("/{}/_count", segment(&object.name)), &json!({ "query": filter_query(filter)? }))
            .await?;
        Ok(response["count"].as_i64().map(|count| RowCount { count, exact: true }))
    }

    async fn indexes(&self, object: &ObjectRef) -> DbResult<Vec<DocIndex>> {
        catalog::index_summaries(&self.client, &object.name).await
    }

    async fn insert(&self, _object: &ObjectRef, _text: &str) -> DbResult<String> {
        Err(read_only_documents())
    }

    async fn replace(&self, _object: &ObjectRef, _key: &str, _text: &str) -> DbResult<()> {
        Err(read_only_documents())
    }

    async fn delete(&self, _object: &ObjectRef, _key: &str) -> DbResult<()> {
        Err(read_only_documents())
    }

    fn find_statement(&self, request: &FindRequest) -> DbResult<String> {
        let mut body = find_body(
            &FindRequest { limit: request.limit().min(100), cursor: None, ..request.clone() },
            &Cursor::From(0),
        )?;
        body.remove("track_total_hits");
        body.insert("size".into(), json!(request.limit().min(100)));
        Ok(format!(
            "POST /{}/_search\n{}",
            request.object.name,
            serde_json::to_string_pretty(&Value::Object(body)).unwrap_or_default()
        ))
    }

    /// Imports with `_bulk` (one call per batch the caller hands over).
    async fn insert_rows(
        &self,
        object: &ObjectRef,
        columns: &[ColumnInfo],
        rows: Vec<Vec<Option<String>>>,
    ) -> DbResult<u64> {
        if self.info.read_only {
            return Err(read_only_error());
        }
        let mut ndjson = String::new();
        for row in &rows {
            let mut doc = Map::new();
            let mut action = json!({ "_index": object.name });
            for (c, value) in columns.iter().zip(row) {
                let Some(text) = value else { continue };
                if c.name == "_id" {
                    action["_id"] = Value::String(text.clone());
                } else {
                    doc.insert(c.name.clone(), import_value(text, &c.data_type));
                }
            }
            ndjson.push_str(&json!({ "index": action }).to_string());
            ndjson.push('\n');
            ndjson.push_str(&Value::Object(doc).to_string());
            ndjson.push('\n');
        }
        let body = self
            .client
            .call(&Call {
                method: "POST",
                path: "/_bulk?refresh=wait_for",
                body: &ndjson,
                ndjson: true,
                timeout_secs: 300,
                ..Call::default()
            })
            .await?;
        let response = body.json();
        let written = values::affected(response).unwrap_or(0);
        if response["errors"] == true {
            let reason = response["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i.as_object().and_then(|o| o.values().next()))
                .find_map(|r| r["error"]["reason"].as_str())
                .unwrap_or("unknown error");
            return Err(DbError::new(
                ErrorKind::Constraint,
                format!("{} of {} documents were rejected: {reason}", rows.len() as u64 - written, rows.len()),
            ));
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(filter: &str, projection: &str, sort: &str) -> FindRequest {
        FindRequest {
            filter: filter.into(),
            projection: projection.into(),
            sort: sort.into(),
            ..FindRequest::new(ObjectRef::new(NodeKind::Table, "c", None, "orders"), 20)
        }
    }

    #[test]
    fn builds_find_bodies() {
        let b = find_body(&req("", "", ""), &Cursor::From(0)).unwrap();
        assert_eq!(b["query"], json!({ "match_all": {} }));
        assert_eq!(b["size"], json!(21));
        assert!(b.get("from").is_none());
        let b =
            find_body(&req("status:paid AND total:>100", "\"email\", total", "created_at desc, id"), &Cursor::From(40))
                .unwrap();
        assert_eq!(b["query"]["query_string"]["query"], "status:paid AND total:>100");
        assert_eq!(b["_source"], json!(["email", "total"]));
        assert_eq!(b["sort"], json!([{ "created_at": "desc" }, { "id": "asc" }]));
        assert_eq!(b["from"], json!(40));
        let b = find_body(&req(r#"{ "term": { "s": "a" } }"#, "", r#"{ "n": "desc" }"#), &Cursor::After(json!([5])))
            .unwrap();
        assert_eq!((b["query"].clone(), b["search_after"].clone()), (json!({ "term": { "s": "a" } }), json!([5])));
        assert!(find_body(&req("{ bad", "", ""), &Cursor::From(0)).is_err());
        assert!(find_body(&req("", "", ""), &Cursor::From(9_990)).is_err());
        assert!(find_body(&req("", "", ""), &Cursor::After(json!([1]))).is_err());
    }

    #[test]
    fn cursors_page_forward() {
        let hits = vec![json!({"sort": [3]}), json!({"sort": [4]})];
        assert_eq!(next_cursor(&Cursor::From(0), &hits, true), Some(Cursor::From(2)));
        assert_eq!(next_cursor(&Cursor::From(0), &hits, false), None);
        assert_eq!(next_cursor(&Cursor::From(9_998), &hits, true), Some(Cursor::After(json!([4]))));
        assert_eq!(
            Cursor::parse(Some(&Cursor::After(json!([4, "a"])).encode())).unwrap(),
            Cursor::After(json!([4, "a"]))
        );
        assert_eq!(Cursor::parse(None).unwrap(), Cursor::From(0));
        assert!(Cursor::parse(Some("{}")).is_err());
    }

    #[test]
    fn import_values_follow_mapping() {
        assert_eq!(import_value("12", "long"), json!(12));
        assert_eq!(import_value("1.5", "double"), json!(1.5));
        assert_eq!(import_value("x", "long"), json!("x"));
        assert_eq!(import_value("true", "boolean"), json!(true));
        assert_eq!(import_value("{\"a\":1}", "object"), json!({"a": 1}));
        assert_eq!(import_value("007", "keyword"), json!("007"));
        assert_eq!(import_value("42", ""), json!(42));
        assert_eq!(import_value("[1,2]", ""), json!([1, 2]));
        assert_eq!(import_value("hello", ""), json!("hello"));
    }

    #[test]
    fn requests() {
        let r = parse_request("GET /orders/_search\n{}").unwrap();
        assert!(is_search(&r));
        assert_eq!((command(&r), target(&r)), ("GET _search".to_owned(), Some("orders".to_owned())));
        assert!(is_ndjson(&parse_request("POST _bulk\n{}").unwrap()));
        assert!(!is_search(&parse_request("GET _cat/indices").unwrap()));
    }
}
