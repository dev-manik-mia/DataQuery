//! An open `DynamoDB` connection.
//!
//! `DynamoDB` is stateless HTTP, so the "session" is a signed client plus the
//! editor's open result (a `NextToken`) and its transaction buffer: `BEGIN`
//! queues writes until `COMMIT` sends them as one `ExecuteTransaction`.
//! There is no read-only mode on the server, so read-only profiles are
//! enforced here. `DynamoDB` cannot cancel a request; cancel and timeouts stop
//! waiting for it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::adapter::{BatchFn, ConnectParams, Params, Session, StatementIter, check_affected};
use dq_core::document::{
    DocField, DocIndex, Document, DocumentFeatures, DocumentPage, DocumentRows, DocumentStore, FindRequest, GridColumns,
};
use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::{ColumnMeta, ExecOptions, Row, RowsPage, ServerInfo, StatementResult, TlsInfo};
use dq_core::schema::{ColumnInfo, Dependencies, ModelTable, SchemaModel, TableSize, TableStructure};
use dq_core::sql::builder::validate_filter;
use dq_core::sql::lexer::{TokenKind, tokenize};
use dq_core::sql::{Dialect, PartiqlDialect, SqlFlavor, StatementKind, analyze, command_tag};
use dq_core::table::RowCount;
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult, ErrorKind};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

use crate::catalog;
use crate::client::{Client, backoff};
use crate::values::{self, Item};

/// `ExecuteTransaction` and `BatchWriteItem` limits.
const MAX_TX_STATEMENTS: usize = 100;
const BATCH_WRITE: usize = 25;
/// Requests one editor statement or page may make while looking for rows.
const MAX_REQUESTS: usize = 100;
/// Requests one Documents-view page may make (filters can skip many items).
const MAX_FIND_REQUESTS: usize = 20;
const SAMPLE_ITEMS: u32 = 50;

pub struct DdbSession {
    client: Arc<Client>,
    info: ServerInfo,
    cursor: Mutex<Option<Cursor>>,
    /// Writes queued by `BEGIN`; `None` outside a transaction.
    tx: Mutex<Option<Vec<Value>>>,
    running: AtomicUsize,
    cancelled: Notify,
    /// Key attribute names by table.
    keys: SyncMutex<HashMap<String, Vec<String>>>,
}

/// The rest of the editor's last result.
struct Cursor {
    statement: String,
    parameters: Vec<Value>,
    next: Option<String>,
    columns: GridColumns,
}

fn read_only_error() -> DbError {
    DbError::new(ErrorKind::Permission, "This connection is read-only.")
        .with_action(Some("Edit the connection and turn off Read-only to change data."))
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn items(page: &Value) -> Vec<Item> {
    page["Items"].as_array().into_iter().flatten().filter_map(|i| i.as_object().cloned()).collect()
}

fn next_token(page: &Value) -> Option<String> {
    page["NextToken"].as_str().filter(|t| !t.is_empty()).map(str::to_owned)
}

impl DdbSession {
    pub(crate) async fn open(params: &ConnectParams) -> DbResult<Self> {
        let started = Instant::now();
        let client = Arc::new(Client::new(params)?);
        client.call("ListTables", &json!({ "Limit": 1 })).await?;
        let config = &params.config;
        let tls = config.host.trim().is_empty() || config.ssl_mode != dq_core::config::SslMode::Disable;
        let info = ServerInfo {
            engine: config.engine,
            version: if client.local { "DynamoDB Local".into() } else { "DynamoDB".into() },
            latency_ms: elapsed(started),
            tls: tls.then(|| TlsInfo {
                version: None,
                verified: config.host.trim().is_empty() || config.ssl_mode.verifies_chain(),
                mode: config.ssl_mode,
            }),
            user: client.access_key_id().to_owned(),
            database: client.region().to_owned(),
            read_only: config.read_only,
        };
        Ok(Self {
            client,
            info,
            cursor: Mutex::new(None),
            tx: Mutex::new(None),
            running: AtomicUsize::new(0),
            cancelled: Notify::new(),
            keys: SyncMutex::new(HashMap::new()),
        })
    }

    /// Runs `call`, stopping early on cancel or after `timeout_secs` (0 = none).
    async fn guarded<T>(&self, call: impl Future<Output = DbResult<T>>, timeout_secs: u32) -> DbResult<T> {
        self.running.fetch_add(1, Ordering::SeqCst);
        let limit = if timeout_secs == 0 { Duration::MAX } else { Duration::from_secs(u64::from(timeout_secs)) };
        let outcome = tokio::select! {
            result = call => result,
            () = self.cancelled.notified() => Err(DbError::new(ErrorKind::Cancelled, "The statement was cancelled.")),
            () = tokio::time::sleep(limit) => Err(DbError::new(
                ErrorKind::Timeout,
                format!("The statement ran longer than {timeout_secs} s and was stopped waiting for."),
            )
            .with_action(Some("Narrow the query or raise the statement timeout in Settings."))),
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        outcome
    }

    /// One `ExecuteStatement` page.
    async fn page(
        &self,
        statement: &str,
        parameters: &[Value],
        next: Option<&str>,
        limit: Option<u32>,
    ) -> DbResult<Value> {
        let mut body = json!({ "Statement": statement });
        if !parameters.is_empty() {
            body["Parameters"] = Value::Array(parameters.to_vec());
        }
        if let Some(token) = next {
            body["NextToken"] = Value::String(token.to_owned());
        }
        if let Some(limit) = limit {
            body["Limit"] = json!(limit);
        }
        self.client.call("ExecuteStatement", &body).await
    }

    /// Up to `limit` items, following `NextToken` for at most `max_requests` requests.
    async fn collect(
        &self,
        statement: &str,
        parameters: &[Value],
        mut next: Option<String>,
        limit: usize,
        max_requests: usize,
    ) -> DbResult<(Vec<Item>, Option<String>)> {
        let mut out = Vec::new();
        for _ in 0..max_requests {
            let want = (limit - out.len()).clamp(1, 1_000) as u32;
            let page = self.page(statement, parameters, next.as_deref(), Some(want)).await?;
            out.extend(items(&page));
            next = next_token(&page);
            if next.is_none() || out.len() >= limit {
                break;
            }
        }
        Ok((out, next))
    }

    /// Key attribute names of a table, cached.
    async fn key_names(&self, table: &str) -> DbResult<Vec<String>> {
        if let Some(keys) = self.keys.lock().ok().and_then(|k| k.get(table).cloned()) {
            return Ok(keys);
        }
        let described = catalog::describe(&self.client, table).await?;
        let keys = catalog::key_names(&described);
        if let Ok(mut cache) = self.keys.lock() {
            cache.insert(table.to_owned(), keys.clone());
        }
        Ok(keys)
    }

    fn check_writable(&self) -> DbResult<()> {
        if self.info.read_only { Err(read_only_error()) } else { Ok(()) }
    }

    async fn transaction_control(&self, sql: &str, started: Instant) -> DbResult<StatementResult> {
        let word = sql.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
        let mut tx = self.tx.lock().await;
        let (affected, notice) = match word.as_str() {
            "BEGIN" | "START" => {
                if tx.is_some() {
                    return Err(DbError::invalid("A transaction is already open."));
                }
                *tx = Some(Vec::new());
                (None, "Transaction open: writes are queued and run together at COMMIT. Reads run at once.".to_owned())
            }
            "COMMIT" => {
                let queued = tx.take().unwrap_or_default();
                if queued.is_empty() {
                    (None, "Nothing to commit.".to_owned())
                } else {
                    let count = queued.len();
                    self.client.call("ExecuteTransaction", &json!({ "TransactStatements": queued })).await?;
                    (Some(count as u64), format!("{count} statements committed in one transaction."))
                }
            }
            "ROLLBACK" => {
                let dropped = tx.take().map_or(0, |q| q.len());
                (None, format!("{dropped} queued statements discarded."))
            }
            _ => return Err(DbError::unsupported(format!("DynamoDB does not support `{word}`."))),
        };
        Ok(StatementResult {
            columns: Vec::new(),
            rows: Vec::new(),
            has_more: false,
            command: word,
            affected,
            duration_ms: elapsed(started),
            notices: vec![notice],
        })
    }

    /// Streams every item of a statement as rows.
    async fn stream(&self, sql: &str, params: Params<'_>, on_batch: &mut BatchFn<'_>) -> DbResult<u64> {
        let parameters: Vec<Value> = params.iter().map(|p| values::param(p.as_deref())).collect();
        let mut rows = DocumentRows::new(on_batch);
        let mut next = None;
        loop {
            let page = self.page(sql.trim().trim_end_matches(';'), &parameters, next.as_deref(), None).await?;
            let batch: Vec<Vec<DocField>> =
                items(&page).iter().map(|i| values::fields(i, &[], CellLimits::FULL)).collect();
            rows.push(&batch)?;
            next = next_token(&page);
            if next.is_none() {
                return Ok(rows.rows());
            }
        }
    }

    fn document(item: &Item, keys: &[String]) -> Document {
        Document::new(
            values::key_json(item, keys).unwrap_or_default(),
            values::key_label(item, keys),
            Some(values::item_size(item)),
            values::item_text(item, keys),
            values::fields(item, keys, CellLimits::GRID),
        )
    }

    /// The table's key attributes from `key`, which must name each of them.
    async fn key_item(&self, table: &str, key: &str) -> DbResult<(Item, Vec<String>)> {
        let names = self.key_names(table).await?;
        let key = values::parse_item(key)?;
        if names.iter().any(|n| !key.contains_key(n)) || key.len() != names.len() {
            return Err(DbError::invalid("The key does not match the table's key attributes."));
        }
        Ok((key, names))
    }

    async fn put(&self, table: &str, item: &Item, partition: &str, exists: bool) -> DbResult<()> {
        let condition = if exists { "attribute_exists(#k)" } else { "attribute_not_exists(#k)" };
        let body = json!({
            "TableName": table,
            "Item": item,
            "ConditionExpression": condition,
            "ExpressionAttributeNames": { "#k": partition },
        });
        match self.client.call("PutItem", &body).await {
            Err(e) if e.code.as_deref() == Some("ConditionalCheckFailedException") && !exists => {
                Err(DbError::new(ErrorKind::Constraint, "An item with this key already exists."))
            }
            other => other.map(|_| ()),
        }
    }
}

#[async_trait]
impl Session for DdbSession {
    fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &PartiqlDialect
    }

    async fn list_children(&self, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
        let region = &self.info.database;
        match parent {
            None => {
                let mut node =
                    Node::container(ObjectRef::new(NodeKind::Database, region.clone(), None, region.clone()));
                node.current = true;
                Ok(vec![node])
            }
            Some(p) if p.kind == NodeKind::Database => {
                let (names, _) = catalog::table_names(&self.client, SchemaModel::MAX_TABLES).await?;
                let described = catalog::describe_many(&self.client, &names).await;
                Ok(described
                    .into_iter()
                    .map(|(name, table)| {
                        let mut node = Node::leaf(ObjectRef::new(NodeKind::Table, region.clone(), None, name));
                        node.row_estimate = table.as_ref().and_then(catalog::item_count);
                        node
                    })
                    .collect())
            }
            Some(_) => Ok(Vec::new()),
        }
    }

    async fn search(&self, query: &str, limit: u32) -> DbResult<Vec<Node>> {
        let needle = query.to_lowercase();
        let (names, _) = catalog::table_names(&self.client, SchemaModel::MAX_TABLES).await?;
        Ok(names
            .into_iter()
            .filter(|n| n.to_lowercase().contains(&needle))
            .take(limit as usize)
            .map(|n| Node::leaf(ObjectRef::new(NodeKind::Table, self.info.database.clone(), None, n)))
            .collect())
    }

    async fn execute(&self, sql: &str, params: Params<'_>, options: ExecOptions) -> DbResult<StatementResult> {
        let options = options.clamped();
        let started = Instant::now();
        let sql = sql.trim().trim_end_matches(';').trim();
        let statement =
            analyze(sql, SqlFlavor::Partiql).ok_or_else(|| DbError::invalid("There is no statement to run."))?;
        let mut cursor = self.cursor.lock().await;
        *cursor = None;
        if statement.kind == StatementKind::Transaction {
            return self.transaction_control(sql, started).await;
        }
        if statement.writes {
            self.check_writable()?;
        }
        let parameters: Vec<Value> = params.iter().map(|p| values::param(p.as_deref())).collect();
        let command = command_tag(sql, SqlFlavor::Partiql);
        {
            let mut tx = self.tx.lock().await;
            if let Some(queued) = tx.as_mut().filter(|_| statement.writes) {
                if queued.len() >= MAX_TX_STATEMENTS {
                    return Err(DbError::invalid("DynamoDB transactions hold at most 100 statements."));
                }
                let mut entry = json!({ "Statement": sql });
                if !parameters.is_empty() {
                    entry["Parameters"] = Value::Array(parameters);
                }
                queued.push(entry);
                return Ok(StatementResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    has_more: false,
                    command,
                    affected: None,
                    duration_ms: elapsed(started),
                    notices: vec!["Queued; runs at COMMIT.".into()],
                });
            }
        }
        let in_tx = self.tx.lock().await.is_some();
        let limit = options.row_limit as usize;
        let (found, next) =
            self.guarded(self.collect(sql, &parameters, None, limit, MAX_REQUESTS), options.timeout_secs).await?;
        let mut notices = Vec::new();
        if in_tx {
            notices.push("This read ran at once; queued writes are not visible until COMMIT.".to_owned());
        }
        if statement.writes && found.is_empty() {
            return Ok(StatementResult {
                columns: Vec::new(),
                rows: Vec::new(),
                has_more: false,
                command,
                affected: Some(1),
                duration_ms: elapsed(started),
                notices,
            });
        }
        let docs: Vec<Vec<DocField>> = found.iter().map(|i| values::fields(i, &[], CellLimits::GRID)).collect();
        let mut columns = GridColumns::default();
        for fields in &docs {
            columns.observe(fields);
        }
        let rows: Vec<Row> = docs.iter().map(|f| columns.row(f)).collect();
        let has_more = next.is_some();
        if has_more {
            notices.push(format!(
                "Columns come from the first {} items; open the table in the Documents view to see every attribute.",
                rows.len()
            ));
        }
        let meta = columns.meta();
        if has_more {
            *cursor = Some(Cursor { statement: sql.to_owned(), parameters, next, columns });
        }
        Ok(StatementResult {
            columns: meta,
            rows,
            has_more,
            command,
            affected: None,
            duration_ms: elapsed(started),
            notices,
        })
    }

    async fn fetch_more(&self, limit: u32) -> DbResult<RowsPage> {
        let started = Instant::now();
        let mut slot = self.cursor.lock().await;
        let mut cursor =
            slot.take().ok_or_else(|| DbError::invalid("No more rows: the result was closed by a newer query."))?;
        let limit = limit.clamp(1, ExecOptions::MAX_ROW_LIMIT) as usize;
        let (found, next) = self
            .guarded(self.collect(&cursor.statement, &cursor.parameters, cursor.next.take(), limit, MAX_REQUESTS), 0)
            .await?;
        let rows = found.iter().map(|i| cursor.columns.row(&values::fields(i, &[], CellLimits::GRID))).collect();
        let has_more = next.is_some();
        if has_more {
            cursor.next = next;
            *slot = Some(cursor);
        }
        Ok(RowsPage { rows, has_more, duration_ms: elapsed(started) })
    }

    async fn cancel(&self) -> DbResult<bool> {
        if self.running.load(Ordering::SeqCst) == 0 {
            return Ok(false);
        }
        self.cancelled.notify_waiters();
        Ok(true)
    }

    async fn structure(&self, object: &ObjectRef) -> DbResult<TableStructure> {
        let table = catalog::describe(&self.client, &object.name).await?;
        let statement = format!("SELECT * FROM {}", PartiqlDialect.quote_ident(&object.name));
        let sample = items(&self.page(&statement, &[], None, Some(SAMPLE_ITEMS)).await?);
        let keys = catalog::key_names(&table);
        let total = table["TableSizeBytes"].as_i64().unwrap_or(0);
        let index_bytes: i64 = catalog::index_infos(&table).iter().filter_map(|i| i.size_bytes).sum();
        Ok(TableStructure {
            object: object.clone(),
            columns: catalog::columns(&table, &sample),
            constraints: catalog::constraints(&table),
            indexes: catalog::index_infos(&table),
            row_identity: keys,
            full_row_key: Vec::new(),
            comment: None,
            row_estimate: catalog::item_count(&table),
            foreign_keys: Vec::new(),
            size: Some(TableSize { total_bytes: total + index_bytes, table_bytes: total, index_bytes }),
        })
    }

    async fn ddl(&self, object: &ObjectRef) -> DbResult<String> {
        let table = catalog::describe(&self.client, &object.name).await?;
        let ttl = self.client.call("DescribeTimeToLive", &json!({ "TableName": object.name })).await.ok();
        catalog::ddl(&table, ttl.as_ref().map(|t| &t["TimeToLiveDescription"]))
    }

    async fn dependencies(&self, _object: &ObjectRef) -> DbResult<Dependencies> {
        Ok(Dependencies::default())
    }

    async fn schema_model(&self, database: &str, _schema: Option<&str>) -> DbResult<SchemaModel> {
        let (names, truncated) = catalog::table_names(&self.client, SchemaModel::MAX_TABLES).await?;
        let database = if database.is_empty() { self.info.database.as_str() } else { database };
        let tables = catalog::describe_many(&self.client, &names)
            .await
            .into_iter()
            .map(|(name, table)| ModelTable {
                object: ObjectRef::new(NodeKind::Table, database, None, name),
                columns: table.as_ref().map(catalog::model_columns).unwrap_or_default(),
            })
            .collect();
        Ok(SchemaModel { tables, foreign_keys: Vec::new(), truncated })
    }

    async fn read_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        limits: CellLimits,
    ) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
        let _ = limits;
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        self.stream(sql, params, &mut |meta, batch| {
            columns = meta.to_vec();
            rows.extend(batch);
            Ok(())
        })
        .await?;
        Ok((columns, rows))
    }

    async fn stream_query(
        &self,
        _database: &str,
        sql: &str,
        params: Params<'_>,
        on_batch: &mut BatchFn<'_>,
    ) -> DbResult<u64> {
        self.stream(sql, params, on_batch).await
    }

    async fn row_estimate(&self, object: &ObjectRef) -> DbResult<Option<i64>> {
        if object.kind != NodeKind::Table {
            return Ok(None);
        }
        Ok(catalog::item_count(&catalog::describe(&self.client, &object.name).await?))
    }

    async fn run_in_transaction(&self, _database: &str, statements: &mut StatementIter<'_>) -> DbResult<Vec<u64>> {
        self.check_writable()?;
        let mut bound = Vec::new();
        for statement in statements {
            bound.push(statement?);
            if bound.len() > MAX_TX_STATEMENTS {
                return Err(DbError::invalid("DynamoDB transactions hold at most 100 statements."));
            }
        }
        if bound.is_empty() {
            return Ok(Vec::new());
        }
        for statement in &bound {
            check_affected(statement, 1)?;
        }
        let entries: Vec<Value> = bound
            .iter()
            .map(|s| {
                let mut entry = json!({ "Statement": s.sql });
                if !s.params.is_empty() {
                    entry["Parameters"] = Value::Array(s.params.iter().map(|p| values::param(p.as_deref())).collect());
                }
                entry
            })
            .collect();
        self.client.call("ExecuteTransaction", &json!({ "TransactStatements": entries })).await?;
        Ok(vec![1; bound.len()])
    }

    fn documents(&self) -> Option<&dyn DocumentStore> {
        Some(self)
    }

    async fn close(&self) {
        let _ = self.cancel().await;
    }
}

/// `"a", b, c.d` → `"a", "b", c.d`: bare names are quoted; paths stay as typed.
fn projection(text: &str) -> DbResult<String> {
    let Some(text) = validate_filter(&PartiqlDialect, Some(text))? else { return Ok("*".into()) };
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    for token in tokenize(&text, SqlFlavor::Partiql) {
        match (token.kind, token.text(&text)) {
            (TokenKind::OpenParen, _) | (TokenKind::Punct, "[") => depth += 1,
            (TokenKind::CloseParen, _) | (TokenKind::Punct, "]") => depth -= 1,
            (TokenKind::Punct, ",") if depth == 0 => {
                parts.push(&text[start..token.start]);
                start = token.end;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return Err(DbError::invalid("The projection has an empty name. Separate names with commas."));
            }
            let bare = p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            Ok(if bare && p != "*" { PartiqlDialect.quote_ident(p) } else { p.to_owned() })
        })
        .collect::<DbResult<Vec<_>>>()
        .map(|p| p.join(", "))
}

#[async_trait]
impl DocumentStore for DdbSession {
    fn features(&self) -> DocumentFeatures {
        DocumentFeatures {
            language: "PartiQL",
            filter_example: "\"status\" = 'paid' AND \"total\" >= 100",
            projection_example: "\"pk\", \"email\", \"total\"",
            sort: false,
            aggregation: false,
            validation: false,
            text_format: "dynamodb_json",
            new_document: "{\n  \"pk\": { \"S\": \"\" }\n}",
            filtered_count: false,
        }
    }

    async fn find(&self, request: &FindRequest) -> DbResult<DocumentPage> {
        let started = Instant::now();
        let statement = self.find_statement(request)?;
        let keys = self.key_names(&request.object.name).await?;
        let (found, next) =
            self.collect(&statement, &[], request.cursor.clone(), request.limit() as usize, MAX_FIND_REQUESTS).await?;
        Ok(DocumentPage {
            documents: found.iter().map(|i| Self::document(i, &keys)).collect(),
            next,
            duration_ms: elapsed(started),
        })
    }

    async fn count(&self, object: &ObjectRef, filter: &str) -> DbResult<Option<RowCount>> {
        if !filter.trim().is_empty() {
            return Ok(None);
        }
        let table = catalog::describe(&self.client, &object.name).await?;
        Ok(catalog::item_count(&table).map(|count| RowCount { count, exact: false }))
    }

    async fn indexes(&self, object: &ObjectRef) -> DbResult<Vec<DocIndex>> {
        Ok(catalog::doc_indexes(&catalog::describe(&self.client, &object.name).await?))
    }

    async fn insert(&self, object: &ObjectRef, text: &str) -> DbResult<String> {
        self.check_writable()?;
        let keys = self.key_names(&object.name).await?;
        let item = values::parse_item(text)?;
        let key = values::key_json(&item, &keys)
            .ok_or_else(|| DbError::invalid(format!("The item needs its key attributes: {}.", keys.join(", "))))?;
        self.put(&object.name, &item, &keys[0], false).await?;
        Ok(key)
    }

    async fn replace(&self, object: &ObjectRef, key: &str, text: &str) -> DbResult<()> {
        self.check_writable()?;
        let (key, names) = self.key_item(&object.name, key).await?;
        let item = values::parse_item(text)?;
        if names.iter().any(|n| item.get(n) != key.get(n)) {
            return Err(DbError::invalid("To change the key, insert a new item and delete this one."));
        }
        self.put(&object.name, &item, &names[0], true).await
    }

    async fn delete(&self, object: &ObjectRef, key: &str) -> DbResult<()> {
        self.check_writable()?;
        let (key, names) = self.key_item(&object.name, key).await?;
        let body = json!({
            "TableName": object.name,
            "Key": key,
            "ConditionExpression": "attribute_exists(#k)",
            "ExpressionAttributeNames": { "#k": names[0] },
            "ReturnValues": "NONE",
        });
        self.client.call("DeleteItem", &body).await.map(|_| ())
    }

    fn find_statement(&self, request: &FindRequest) -> DbResult<String> {
        let mut sql = format!(
            "SELECT {} FROM {}",
            projection(&request.projection)?,
            PartiqlDialect.quote_ident(&request.object.name)
        );
        if let Some(filter) = validate_filter(&PartiqlDialect, Some(&request.filter))? {
            sql.push_str(" WHERE ");
            sql.push_str(&filter);
        }
        Ok(sql)
    }

    async fn insert_rows(
        &self,
        object: &ObjectRef,
        columns: &[ColumnInfo],
        rows: Vec<Vec<Option<String>>>,
    ) -> DbResult<u64> {
        self.check_writable()?;
        let keys = self.key_names(&object.name).await?;
        let mut requests = Vec::with_capacity(rows.len());
        for row in rows {
            let mut item = Map::new();
            for (column, value) in columns.iter().zip(row) {
                let Some(value) = value else { continue };
                let av = values::from_text(&column.data_type, &value)
                    .map_err(|m| DbError::invalid(format!("{}: {m}", column.name)))?;
                item.insert(column.name.clone(), av);
            }
            if let Some(missing) = keys.iter().find(|k| !item.contains_key(*k)) {
                return Err(DbError::invalid(format!("Every item needs its key attribute `{missing}`.")));
            }
            requests.push(json!({ "PutRequest": { "Item": item } }));
        }
        let total = requests.len() as u64;
        for chunk in requests.chunks(BATCH_WRITE) {
            let mut pending = chunk.to_vec();
            for attempt in 1..=8 {
                let body = json!({ "RequestItems": { object.name.clone(): pending } });
                let out = self.client.call("BatchWriteItem", &body).await?;
                pending = out["UnprocessedItems"][&object.name].as_array().cloned().unwrap_or_default();
                if pending.is_empty() {
                    break;
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
            if !pending.is_empty() {
                return Err(DbError::new(
                    ErrorKind::Server,
                    format!("DynamoDB kept {} items unprocessed (throughput exceeded).", pending.len()),
                ));
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projections_are_quoted() {
        assert_eq!(projection("").unwrap(), "*");
        assert_eq!(
            projection("pk, \"e mail\", a.b, list[0], x-y").unwrap(),
            "\"pk\", \"e mail\", a.b, list[0], \"x-y\""
        );
        assert!(projection("a; DROP").is_err());
        assert!(projection("a,,b").is_err());
    }
}
