//! Integration tests against `DynamoDB` Local.
//!
//! Start it with `docker compose -p dataquery up -d dynamodb` and run
//! `DATAQUERY_TEST_DYNAMODB=localhost:58002 cargo test -p dq-dynamodb`.
//! Without the variable every test is skipped. Each test creates its own
//! tables and deletes them afterwards.

use std::sync::Arc;

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::document::FindRequest;
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_dynamodb::DynamodbAdapter;
use dq_dynamodb::client::Client;
use serde_json::{Value, json};

const REGION: &str = "us-east-1";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_DYNAMODB").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "8000"));
    Some(ConnConfig {
        engine: Engine::Dynamodb,
        host: host.into(),
        port: port.parse().unwrap(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        properties: [("region".to_owned(), REGION.to_owned())].into(),
        ..ConnConfig::default()
    })
}

/// The test configuration, or returns (with `$skip`) when the variable is unset.
macro_rules! require_db {
    ($($skip:expr)?) => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_DYNAMODB to run DynamoDB integration tests");
                return $($skip)?;
            }
        }
    };
}

fn params(cfg: &ConnConfig) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some("dataquery".into()))
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    DynamodbAdapter.connect(&params(cfg)).await.expect("connect")
}

/// A table that is deleted when the test ends (also on failure).
struct Table {
    name: String,
    client: Arc<Client>,
    config: ConnConfig,
}

impl Table {
    /// `pk` (S) partition key, optional `sk` (N) sort key, optional GSI on `status`.
    async fn create(cfg: &ConnConfig, sort_key: bool, gsi: bool) -> Self {
        let name = format!("dq_{}", uuid::Uuid::new_v4().simple());
        let client = Arc::new(Client::new(&params(cfg)).unwrap());
        let mut keys = vec![json!({"AttributeName": "pk", "KeyType": "HASH"})];
        let mut defs = vec![json!({"AttributeName": "pk", "AttributeType": "S"})];
        if sort_key {
            keys.push(json!({"AttributeName": "sk", "KeyType": "RANGE"}));
            defs.push(json!({"AttributeName": "sk", "AttributeType": "N"}));
        }
        let mut body = json!({
            "TableName": name, "KeySchema": keys, "AttributeDefinitions": defs, "BillingMode": "PAY_PER_REQUEST",
        });
        if gsi {
            body["AttributeDefinitions"]
                .as_array_mut()
                .unwrap()
                .push(json!({"AttributeName": "status", "AttributeType": "S"}));
            body["GlobalSecondaryIndexes"] = json!([{
                "IndexName": "by_status",
                "KeySchema": [{"AttributeName": "status", "KeyType": "HASH"}],
                "Projection": {"ProjectionType": "ALL"},
            }]);
        }
        client.call("CreateTable", &body).await.expect("create table");
        for _ in 0..50 {
            let t = client.call("DescribeTable", &json!({"TableName": name})).await.unwrap();
            if t["Table"]["TableStatus"] == "ACTIVE" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Self { name, client, config: cfg.clone() }
    }

    fn object(&self) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, REGION, None, self.name.clone())
    }

    fn quoted(&self) -> String {
        format!("\"{}\"", self.name)
    }

    async fn put(&self, item: Value) {
        self.client.call("PutItem", &json!({"TableName": self.name, "Item": item})).await.expect("put");
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // The test's runtime is blocked here, so delete with a client and runtime of our own.
        let (config, name) = (self.config.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let client = Client::new(&params(&config)).unwrap();
                let _ = client.call("DeleteTable", &json!({"TableName": name})).await;
            });
        })
        .join();
    }
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

fn column<'a>(columns: &[dq_core::result::ColumnMeta], row: &'a [Cell], name: &str) -> &'a Cell {
    &row[columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"))]
}

#[tokio::test]
async fn connects_and_reports_the_server() {
    let cfg = require_db!();
    let info = DynamodbAdapter.test(&params(&cfg)).await.unwrap();
    assert_eq!(
        (info.version.as_str(), info.database.as_str(), info.user.as_str()),
        ("DynamoDB Local", REGION, "dataquery")
    );
    let missing = DynamodbAdapter.test(&ConnectParams::new(cfg.clone(), None)).await.unwrap_err();
    assert_eq!(missing.kind, ErrorKind::Auth);
    let creds = r#"{"Version":1,"AccessKeyId":"profilekey","SecretAccessKey":"s","SessionToken":"t"}"#;
    // AWS CLI profiles pass `aws configure export-credentials` JSON as the password.
    let info = DynamodbAdapter.test(&ConnectParams::new(cfg, Some(creds.into()))).await.unwrap();
    assert_eq!(info.user, "profilekey");
}

#[tokio::test]
async fn explorer_lists_tables_with_estimates() {
    let cfg = require_db!();
    let table = Table::create(&cfg, false, false).await;
    table.put(json!({"pk": {"S": "a"}})).await;
    let s = session(&cfg).await;
    let roots = s.list_children(None).await.unwrap();
    assert_eq!((roots.len(), roots[0].object.name.as_str(), roots[0].current), (1, REGION, true));
    let tables = s.list_children(Some(&roots[0].object)).await.unwrap();
    let node = tables.iter().find(|n| n.object.name == table.name).expect("table listed");
    assert_eq!(node.object.kind, NodeKind::Table);
    assert!(node.row_estimate.is_some());
    let found = s.search(&table.name[3..12].to_uppercase(), 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == table.name));
    assert_eq!(s.row_estimate(&table.object()).await.unwrap(), Some(1));
}

#[tokio::test]
async fn partiql_binds_parameters_and_keeps_values_exact() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, true, false).await;
    let s = session(&cfg).await;
    let insert =
        format!("INSERT INTO {} VALUE {{'pk': ?, 'sk': ?, 'big': ?, 'bin': ?, 'tags': ?, 'note': ?}}", table.quoted());
    let params = [
        Some("a".to_owned()),
        Some("1".to_owned()),
        Some("9007199254740993.5".to_owned()),
        Some(r#"{"B": "AAEC"}"#.to_owned()),
        Some(r#"{"SS": ["x", "y"]}"#.to_owned()),
        None,
    ];
    let r = s.execute(&insert, &params, OPTS).await?;
    assert_eq!((r.affected, r.command.as_str()), (Some(1), "INSERT"));
    let r = s.execute(&format!("SELECT * FROM {} WHERE pk = ?", table.quoted()), &[Some("a".into())], OPTS).await?;
    assert_eq!(r.rows.len(), 1);
    let row = &r.rows[0];
    assert_eq!(column(&r.columns, row, "big"), &Cell::number("9007199254740993.5"));
    assert!(r.columns.iter().find(|c| c.name == "big").unwrap().numeric);
    assert_eq!(column(&r.columns, row, "bin"), &Cell::Binary { length: 3, head_hex: "000102".into() });
    assert_eq!(text(column(&r.columns, row, "tags")), r#"["x","y"]"#);
    assert_eq!(column(&r.columns, row, "note"), &Cell::Null);
    let err = s.execute("SELEC nothing", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
    Ok(())
}

#[tokio::test]
async fn results_page_with_fetch_more() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, false, false).await;
    let s = session(&cfg).await;
    let columns = [dq_core::schema::ColumnInfo::new_field("pk"), dq_core::schema::ColumnInfo::new_field("n")];
    let rows = (0..30).map(|i| vec![Some(format!("k{i:02}")), Some(i.to_string())]).collect();
    assert_eq!(s.documents().unwrap().insert_rows(&table.object(), &columns, rows).await?, 30);
    let r = s.execute(&format!("SELECT * FROM {}", table.quoted()), &[], ExecOptions { row_limit: 10, ..OPTS }).await?;
    assert_eq!((r.rows.len(), r.has_more), (10, true));
    assert!(r.notices.iter().any(|n| n.contains("first 10 items")));
    let mut total = r.rows.len();
    loop {
        let page = s.fetch_more(7).await?;
        assert!(page.rows.len() <= 7);
        total += page.rows.len();
        if !page.has_more {
            break;
        }
    }
    assert_eq!(total, 30);
    assert!(s.fetch_more(5).await.is_err());
    Ok(())
}

#[tokio::test]
async fn documents_find_count_and_indexes() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, true, true).await;
    for i in 0..5 {
        let status = if i % 2 == 0 { "paid" } else { "open" };
        table.put(json!({"pk": {"S": "c"}, "sk": {"N": i.to_string()}, "status": {"S": status}, "total": {"N": format!("{i}.50")}})).await;
    }
    let s = session(&cfg).await;
    let store = s.documents().unwrap();
    assert_eq!((store.features().language, store.features().sort), ("PartiQL", false));
    let mut request = FindRequest::new(table.object(), 2);
    request.filter = "status = 'paid'".into();
    request.projection = "pk, sk, total".into();
    assert_eq!(
        store.find_statement(&request)?,
        format!("SELECT \"pk\", \"sk\", \"total\" FROM {} WHERE status = 'paid'", table.quoted())
    );
    let mut seen = Vec::new();
    loop {
        let page = store.find(&request).await?;
        for d in &page.documents {
            assert!(d.fields.iter().all(|f| f.key != "status"));
            assert_eq!(d.fields[0].key, "pk");
            seen.push(d.label.clone());
        }
        match page.next {
            Some(next) => request.cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, ["pk: \"c\" · sk: 0", "pk: \"c\" · sk: 2", "pk: \"c\" · sk: 4"]);
    let first = store.find(&FindRequest::new(table.object(), 1)).await?;
    let doc = &first.documents[0];
    assert_eq!(doc.key, r#"{"pk":{"S":"c"},"sk":{"N":"0"}}"#);
    assert!(doc.text.starts_with("{\n  \"pk\": {\n    \"S\": \"c\"\n  },\n  \"sk\""));
    assert!(doc.size_bytes.unwrap() > 0);
    let count = store.count(&table.object(), "").await?.unwrap();
    assert_eq!((count.count, count.exact), (5, false));
    assert!(store.count(&table.object(), "status = 'paid'").await?.is_none());
    let indexes = store.indexes(&table.object()).await?;
    assert_eq!(indexes[0].keys, "pk (HASH) · sk (RANGE)");
    assert!(indexes.iter().any(|i| i.name == "by_status" && i.props.iter().any(|(k, v)| k == "type" && v == "GSI")));
    let mut bad = FindRequest::new(table.object(), 10);
    bad.filter = "1 = 1; DELETE".into();
    assert!(store.find(&bad).await.is_err());
    Ok(())
}

#[tokio::test]
async fn document_writes_are_conditional() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, false, false).await;
    let s = session(&cfg).await;
    let store = s.documents().unwrap();
    let object = table.object();
    let key = store.insert(&object, r#"{"pk": {"S": "a"}, "n": {"N": "1"}}"#).await?;
    assert_eq!(key, r#"{"pk":{"S":"a"}}"#);
    let dup = store.insert(&object, r#"{"pk": {"S": "a"}}"#).await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint);
    assert!(store.insert(&object, r#"{"n": {"N": "1"}}"#).await.is_err());
    assert!(
        store.insert(&object, r#"{"pk": {"S": "b"}, "n": {"N": "x"}}"#).await.unwrap_err().message.contains("number")
    );
    store.replace(&object, &key, r#"{"pk": {"S": "a"}, "n": {"N": "2"}}"#).await?;
    let page = store.find(&FindRequest::new(object.clone(), 10)).await?;
    assert!(page.documents[0].text.contains("\"2\""));
    let moved = store.replace(&object, &key, r#"{"pk": {"S": "z"}}"#).await.unwrap_err();
    assert!(moved.message.contains("To change the key"));
    store.delete(&object, &key).await?;
    assert_eq!(store.delete(&object, &key).await.unwrap_err().kind, ErrorKind::Constraint);
    let gone = store.replace(&object, &key, r#"{"pk": {"S": "a"}}"#).await.unwrap_err();
    assert_eq!(gone.kind, ErrorKind::Constraint);
    Ok(())
}

#[tokio::test]
async fn transactions_queue_until_commit() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, false, false).await;
    let s = session(&cfg).await;
    let count =
        || async { s.execute(&format!("SELECT * FROM {}", table.quoted()), &[], OPTS).await.unwrap().rows.len() };
    let insert = |k: &str| format!("INSERT INTO {} VALUE {{'pk': '{k}'}}", table.quoted());
    s.execute("BEGIN", &[], OPTS).await?;
    let queued = s.execute(&insert("a"), &[], OPTS).await?;
    assert_eq!(queued.affected, None);
    assert!(queued.notices[0].contains("COMMIT"));
    assert!(s.execute("BEGIN", &[], OPTS).await.is_err());
    s.execute("ROLLBACK", &[], OPTS).await?;
    assert_eq!(count().await, 0);
    s.execute("BEGIN", &[], OPTS).await?;
    s.execute(&insert("a"), &[], OPTS).await?;
    s.execute(&insert("b"), &[], OPTS).await?;
    assert_eq!(count().await, 0);
    let commit = s.execute("COMMIT", &[], OPTS).await?;
    assert_eq!(commit.affected, Some(2));
    assert_eq!(count().await, 2);
    // A failing statement cancels the whole transaction.
    s.execute("BEGIN", &[], OPTS).await?;
    s.execute(&insert("c"), &[], OPTS).await?;
    s.execute(&insert("a"), &[], OPTS).await?;
    assert_eq!(s.execute("COMMIT", &[], OPTS).await.unwrap_err().kind, ErrorKind::Constraint);
    assert_eq!(count().await, 2);
    Ok(())
}

#[tokio::test]
async fn read_only_refuses_writes() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, false, false).await;
    let s = session(&ConnConfig { read_only: true, ..cfg }).await;
    let err = s.execute(&format!("INSERT INTO {} VALUE {{'pk': 'a'}}", table.quoted()), &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    let store = s.documents().unwrap();
    assert_eq!(store.insert(&table.object(), r#"{"pk": {"S": "a"}}"#).await.unwrap_err().kind, ErrorKind::Permission);
    assert_eq!(store.delete(&table.object(), r#"{"pk": {"S": "a"}}"#).await.unwrap_err().kind, ErrorKind::Permission);
    assert!(s.execute(&format!("SELECT * FROM {}", table.quoted()), &[], OPTS).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn imports_and_exports_items() -> DbResult<()> {
    let cfg = require_db!(Ok(()));
    let table = Table::create(&cfg, false, false).await;
    table.put(json!({"pk": {"S": "seed"}, "qty": {"N": "1"}, "active": {"BOOL": true}})).await;
    let s = session(&cfg).await;
    let structure = ops::structure(&*s, &table.object()).await?;
    let names: Vec<&str> = structure.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["pk", "active", "qty"]);
    assert_eq!((structure.columns[2].data_type.as_str(), structure.row_identity.clone()), ("N", vec!["pk".to_owned()]));
    let ddl = s.ddl(&table.object()).await?;
    assert!(ddl.contains("\"KeySchema\"") && ddl.contains("PAY_PER_REQUEST"));

    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("in.csv");
    std::fs::write(&csv, "pk,qty,active,extra\nk1,007,true,{\"a\":1}\nk2,2.5,false,plain\n").unwrap();
    let source = ImportSource::Csv { path: csv.to_string_lossy().into(), options: CsvOptions::default() };
    let map: Vec<ImportMapping> = ["pk", "qty", "active", "extra"]
        .iter()
        .enumerate()
        .map(|(i, c)| ImportMapping { column: (*c).into(), field: FieldRef::Index(i) })
        .collect();
    let none = TransferControl::none();
    let report = ops::import(&*s, &table.object(), &source, &map, &ImportOptions::default(), &none).await?;
    assert_eq!((report.rows, report.error_count), (2, 0));
    let store = s.documents().unwrap();
    let mut request = FindRequest::new(table.object(), 10);
    request.filter = "pk = 'k1'".into();
    let doc = &store.find(&request).await?.documents[0];
    let qty = doc.fields.iter().find(|f| f.key == "qty").unwrap();
    assert_eq!((qty.type_label.as_str(), text(&qty.value).trim_start_matches('0')), ("N", "7"));
    assert!(doc.text.contains("\"BOOL\": true"));
    assert!(doc.fields.iter().any(|f| f.key == "extra" && f.type_label == "M"));

    // Export: every item; a column set that does not cover every attribute stops instead of dropping data.
    let out = dir.path().join("out.json");
    let projected = FindRequest { projection: "pk, qty".into(), ..FindRequest::new(table.object(), 10) };
    let exported = ops::export(
        &*s,
        &ExportSource::Documents { request: projected },
        ExportFormat::Json,
        &ExportOptions::default(),
        &out,
        &none,
    )
    .await?;
    assert_eq!(exported.rows, 3);
    let json: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 3);
    Ok(())
}
