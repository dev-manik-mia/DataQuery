//! Cloudflare D1 against a mock of the documented API
//! (`GET …/d1/database`, `POST …/d1/database/{id}/raw` with `sql`/`params`
//! or `batch`), answered from a real in-memory `SQLite` database. There is
//! no Cloudflare account in CI, so D1 stays in beta.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::sql::{SqlFlavor, split_statements};
use dq_core::transfer::{CsvOptions, FieldRef, ImportOptions, ImportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_sqlite_http::D1Adapter;
use rusqlite::types::ValueRef;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SEED: &str = include_str!("../../dq-sqlite/tests/seed.sql");
const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const DATABASE_ID: &str = "6c8a5d3e-4b1f-4f0e-9a5b-1c2d3e4f5a6b";
const TOKEN: &str = "cf-test-token-0000";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
/// D1 binds at most this many parameters per query.
const MAX_PARAMS: usize = 100;

/// The mock service: one database, and every request it answered.
struct MockD1 {
    db: Mutex<rusqlite::Connection>,
    /// `(method, path, body)` of each request.
    requests: Mutex<Vec<(String, String, Json)>>,
}

fn json_of(value: ValueRef<'_>) -> Json {
    match value {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(b),
    }
}

fn failure(status: u16, code: i64, message: &str) -> (u16, Json) {
    (
        status,
        json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null }),
    )
}

impl MockD1 {
    fn new() -> Arc<Self> {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        // D1 enforces foreign keys and keeps state in `_cf_*` tables.
        db.execute_batch("PRAGMA foreign_keys = ON; CREATE TABLE _cf_KV (key TEXT PRIMARY KEY, value BLOB);").unwrap();
        Arc::new(Self { db: Mutex::new(db), requests: Mutex::new(Vec::new()) })
    }

    /// One statement as D1's `/raw` answers it.
    fn run(db: &rusqlite::Connection, query: &Json) -> Result<Json, String> {
        let sql = query["sql"].as_str().unwrap_or_default();
        if sql.trim_start().to_ascii_uppercase().starts_with("BEGIN") {
            return Err("D1 runs each request as a transaction; BEGIN is not allowed".into());
        }
        let params: Vec<Option<String>> = query["params"]
            .as_array()
            .map(|p| p.iter().map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if params.len() > MAX_PARAMS {
            return Err("too many SQL variables: SQLITE_ERROR".into());
        }
        let mut statement = db.prepare(sql).map_err(|e| format!("{e}: SQLITE_ERROR"))?;
        let columns: Vec<String> = statement.column_names().iter().map(|c| (*c).to_owned()).collect();
        let read_only = statement.readonly();
        let mut found =
            statement.query(rusqlite::params_from_iter(params.iter())).map_err(|e| format!("{e}: SQLITE_ERROR"))?;
        let mut rows = Vec::new();
        while let Some(row) = found.next().map_err(|e| format!("{e}: SQLITE_ERROR"))? {
            rows.push((0..columns.len()).map(|i| json_of(row.get_ref(i).unwrap())).collect::<Vec<_>>());
        }
        let changes = if read_only { 0 } else { db.changes() };
        Ok(json!({
            "results": { "columns": columns, "rows": rows },
            "success": true,
            "meta": { "changes": changes, "duration": 0.1, "rows_read": rows.len(), "served_by_region": "WEUR" }
        }))
    }

    fn handle(&self, method: &str, target: &str, authorization: Option<&str>, body: Json) -> (u16, Json) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        self.requests.lock().unwrap().push((method.to_owned(), path.to_owned(), body.clone()));
        if authorization != Some(&format!("Bearer {TOKEN}")) {
            return failure(403, 10000, "Authentication error");
        }
        let databases = format!("/client/v4/accounts/{ACCOUNT}/d1/database");
        if method == "GET" && path == databases {
            let page = query.split('&').find_map(|p| p.strip_prefix("page=")).unwrap_or("1");
            let name = query.split('&').find_map(|p| p.strip_prefix("name="));
            let all = [("shop", DATABASE_ID), ("staging", "00000000-0000-4000-8000-000000000000")];
            let found: Vec<Json> = all
                .iter()
                .filter(|(n, _)| page == "1" && name.is_none_or(|w| w == *n))
                .map(|(n, id)| json!({ "uuid": id, "name": n, "version": "production", "num_tables": 3 }))
                .collect();
            return (200, json!({ "success": true, "errors": [], "messages": [], "result": found }));
        }
        if method != "POST" || path != format!("{databases}/{DATABASE_ID}/raw") {
            return failure(404, 7404, "The database could not be found.");
        }
        let db = self.db.lock().unwrap();
        let outcome = match body.get("batch").and_then(Json::as_array) {
            // A batch is one transaction: all statements or none.
            Some(batch) => {
                db.execute_batch("SAVEPOINT batch").unwrap();
                let results: Result<Vec<Json>, String> = batch.iter().map(|q| Self::run(&db, q)).collect();
                db.execute_batch(if results.is_ok() { "RELEASE batch" } else { "ROLLBACK TO batch; RELEASE batch" })
                    .unwrap();
                results
            }
            None => Self::run(&db, &body).map(|r| vec![r]),
        };
        match outcome {
            Ok(results) => (200, json!({ "success": true, "errors": [], "messages": [], "result": results })),
            Err(message) => failure(400, 7500, &format!("D1_ERROR: {message}")),
        }
    }

    /// Serves HTTP on a free local port, one request per connection.
    async fn serve(self: &Arc<Self>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = self.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mock = mock.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 16 * 1024];
                    let (head, body) = loop {
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        request.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&request).into_owned();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned())
                                })
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length || n == 0 {
                                break (text[..end].to_owned(), request[end + 4..].to_vec());
                            }
                        }
                        if n == 0 {
                            return;
                        }
                    };
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
                    let authorization = lines.find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization").then(|| value.trim().to_owned())
                    });
                    let body = serde_json::from_slice(&body).unwrap_or(Json::Null);
                    let (status, reply) = mock.handle(method, target, authorization.as_deref(), body);
                    let reply = reply.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    fn posts(&self) -> Vec<Json> {
        self.requests.lock().unwrap().iter().filter(|(m, ..)| m == "POST").map(|(.., b)| b.clone()).collect()
    }
}

fn config(port: u16, database: &str) -> ConnConfig {
    ConnConfig {
        engine: Engine::D1,
        host: "127.0.0.1".into(),
        port,
        database: database.into(),
        ssl_mode: SslMode::Disable,
        properties: BTreeMap::from([(ConnConfig::D1_ACCOUNT.to_owned(), ACCOUNT.to_owned())]),
        ..ConnConfig::default()
    }
}

fn params(port: u16, database: &str) -> ConnectParams {
    ConnectParams::new(config(port, database), Some(TOKEN.into()))
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Sqlite) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// A seeded mock database and a session on it, connected by name.
async fn seeded() -> (Arc<MockD1>, u16, Arc<dyn Session>) {
    let mock = MockD1::new();
    let port = mock.serve().await;
    let s = D1Adapter.connect(&params(port, "shop")).await.expect("connect");
    run_all(&*s, SEED).await;
    (mock, port, s)
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, "main", None, name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn connects_by_name_or_id_and_lists_databases() {
    let mock = MockD1::new();
    let port = mock.serve().await;
    assert_eq!(D1Adapter.databases(&params(port, "")).await.unwrap(), ["shop", "staging"]);
    let info = D1Adapter.test(&params(port, DATABASE_ID)).await.unwrap();
    assert!(info.version.starts_with("Cloudflare D1 (SQLite 3."), "{}", info.version);
    assert!(Engine::D1.is_beta() && !D1Adapter.capabilities().transactions);

    let missing = D1Adapter.test(&params(port, "nope")).await.unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound);
    let wrong = ConnectParams::new(config(port, "shop"), Some("wrong-token-1234".into()));
    let denied = D1Adapter.test(&wrong).await.unwrap_err();
    assert_eq!((denied.kind, denied.code.as_deref()), (ErrorKind::Auth, Some("10000")));
    assert!(!denied.message.contains("wrong-token-1234"));
    let no_token = ConnectParams::new(config(port, "shop"), None);
    assert_eq!(D1Adapter.test(&no_token).await.unwrap_err().kind, ErrorKind::Auth);
    let bad_account = ConnConfig { properties: BTreeMap::new(), ..config(port, "shop") };
    assert!(D1Adapter.test(&ConnectParams::new(bad_account, Some(TOKEN.into()))).await.is_err());
}

#[tokio::test]
async fn explorer_values_and_bounded_results() {
    let (_mock, _port, s) = seeded().await;
    let dbs = s.list_children(None).await.unwrap();
    assert_eq!(dbs.iter().map(|d| d.object.name.as_str()).collect::<Vec<_>>(), ["main"]);
    let rels = s.list_children(Some(&dbs[0].object)).await.unwrap();
    assert!(rels.iter().all(|r| !r.object.name.starts_with("_cf_")), "D1's own tables are hidden");
    assert!(rels.iter().any(|r| r.object.name == "paid_orders" && r.object.kind == NodeKind::View));
    let orders = ops::structure(&*s, &table("orders")).await.unwrap();
    assert_eq!(orders.row_identity, ["id"]);
    assert!(orders.columns[0].identity && orders.foreign_keys[0].target.name == "customers");
    assert!(s.ddl(&table("orders")).await.unwrap().contains("CREATE TRIGGER orders_audit"));
    assert!(s.dependencies(&table("customers")).await.unwrap().used_by.iter().any(|d| d.object.name == "orders"));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));

    let r = s.execute("SELECT big, ratio, doc, raw, label FROM type_samples ORDER BY id", &[], OPTS).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::number("9223372036854775807"));
    assert_eq!(r.rows[0][1], Cell::number("1.5"));
    assert_eq!(r.rows[0][3], Cell::Binary { length: 4, head_hex: "89504e47".into() });
    assert!(r.columns[0].numeric && !r.columns[4].numeric, "numeric from the values: D1 sends no types");

    let r = s.execute("SELECT id FROM orders ORDER BY id", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(1000).await.unwrap();
    assert_eq!((text(&page.rows[0][0]).as_str(), page.rows.len(), page.has_more), ("501", 700, false));
    assert!(s.fetch_more(10).await.is_err());

    let r = s.execute("UPDATE orders SET notes = ?1 WHERE id <= 3", &[Some("it's".into())], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("UPDATE", Some(3)));
    assert_eq!(s.execute("SELEC 1", &[], OPTS).await.unwrap_err().kind, ErrorKind::Syntax);
    assert_eq!(s.execute("SELECT * FROM missing", &[], OPTS).await.unwrap_err().kind, ErrorKind::NotFound);
    let dup = s.execute("INSERT INTO customers (id, email) VALUES (1, 'x')", &[], OPTS).await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint);
    let plan = s.explain("SELECT * FROM orders WHERE status = ?1", &[Some("paid".into())], false, 30).await.unwrap();
    assert!(plan.raw.contains("orders_status_idx"), "{}", plan.raw);
}

#[tokio::test]
async fn edits_and_scripts_are_one_atomic_batch() -> DbResult<()> {
    let (mock, _port, s) = seeded().await;
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("notes", Some("Zoë ✓"))] },
        RowChange::Insert { values: vec![cv("customer_id", Some("1")), cv("total", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let batches = || mock.posts().iter().filter(|b| b.get("batch").is_some()).count();
    let before = batches();
    let report = ops::apply_changes(&*s, &table("orders"), &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    assert_eq!(batches() - before, 1, "one request for the whole edit");

    // A row that no longer matches undoes the whole batch, the update before it too.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("notes", Some("lost"))] },
        RowChange::Delete { key: vec![cv("id", Some("404040"))] },
    ];
    let err = ops::apply_changes(&*s, &table("orders"), &failing, false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint, "{err:?}");
    let r = s.execute("SELECT coalesce(notes, 'NULL') FROM orders WHERE id = 3", &[], OPTS).await?;
    assert_ne!(text(&r.rows[0][0]), "lost");

    let script = ["INSERT INTO no_pk VALUES (7, 'a')", "SELECT count(*) FROM no_pk"];
    let results = s.execute_script(&script, OPTS).await.expect("D1 runs scripts as a batch")?;
    assert_eq!((results[0].affected, text(&results[1].rows[0][0]).as_str()), (Some(1), "3"));
    let broken = ["INSERT INTO no_pk VALUES (8, 'b')", "INSERT INTO missing VALUES (1)"];
    assert_eq!(s.execute_script(&broken, OPTS).await.unwrap().unwrap_err().kind, ErrorKind::NotFound);
    let r = s.execute("SELECT count(*) FROM no_pk WHERE a = 8", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "0", "nothing of a failed script is applied");
    assert!(s.execute("BEGIN", &[], OPTS).await.is_err());
    Ok(())
}

#[tokio::test]
async fn imports_stay_within_the_parameter_limit() -> DbResult<()> {
    let (mock, port, s) = seeded().await;
    run_all(&*s, "CREATE TABLE wide (id INTEGER PRIMARY KEY, a TEXT, b TEXT, c TEXT, d TEXT, e TEXT, f TEXT)").await;
    let dir = std::env::temp_dir().join(format!("dq-d1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("wide.csv");
    let lines: Vec<String> = (1..=300).map(|i| format!("{i},a{i},b,c,d,e,f")).collect();
    std::fs::write(&csv, format!("id,a,b,c,d,e,f\n{}\n", lines.join("\n"))).unwrap();
    let source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions::default() };
    let map: Vec<ImportMapping> = ["id", "a", "b", "c", "d", "e", "f"]
        .iter()
        .enumerate()
        .map(|(i, c)| ImportMapping { column: (*c).into(), field: FieldRef::Index(i) })
        .collect();
    let report =
        ops::import(&*s, &table("wide"), &source, &map, &ImportOptions::default(), &TransferControl::none()).await?;
    assert_eq!(report.rows, 300);
    let most = mock
        .posts()
        .iter()
        .flat_map(|b| b["batch"].as_array().cloned().unwrap_or_else(|| vec![b.clone()]))
        .map(|q| q["params"].as_array().map_or(0, Vec::len))
        .max()
        .unwrap_or(0);
    assert!(most <= MAX_PARAMS, "{most} parameters in one statement");
    std::fs::remove_dir_all(&dir).ok();

    let ro = D1Adapter
        .connect(&ConnectParams::new(ConnConfig { read_only: true, ..config(port, "shop") }, Some(TOKEN.into())))
        .await?;
    assert_eq!(ro.execute("DELETE FROM wide", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
    assert!(ro.execute("SELECT count(*) FROM wide", &[], OPTS).await.is_ok());
    Ok(())
}
