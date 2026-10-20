//! Integration tests against a real MySQL or MariaDB server.
//!
//! Start them with `docker compose -p dataquery up -d mysql mariadb` and run
//! `DATAQUERY_TEST_MYSQL=localhost:53306 cargo test -p dq-mysql` (MySQL 8.4) or
//! `DATAQUERY_TEST_MYSQL=localhost:53307 cargo test -p dq-mysql` (MariaDB 11.4).
//! Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{BoundStatement, ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::ops::ImportMapping;
use dq_core::result::ExecOptions;
use dq_core::table::{FilterCondition, FilterOp, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_mysql::MysqlAdapter;

const PASSWORD: &str = "dataquery";
const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const ADAPTER: MysqlAdapter = MysqlAdapter::MYSQL;

/// A million rows from a cross join of digits, without any table.
const MILLION_ROWS: &str = "SELECT a.n + b.n * 10 + c.n * 100 + d.n * 1000 + e.n * 10000 + f.n * 100000 AS g \
     FROM (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a \
     CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b \
     CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c \
     CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d \
     CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) e \
     CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
           UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) f";

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_MYSQL").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "3306"));
    Some(ConnConfig {
        engine: Engine::Mysql,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

fn with_password(cfg: &ConnConfig, password: &str) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some(password.to_owned()))
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_MYSQL to run MySQL integration tests");
                return;
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ADAPTER.connect(&with_password(cfg, PASSWORD)).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, None, name)
}

/// Creates a scratch table unique to the calling test.
async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    s.execute(&format!("DROP TABLE IF EXISTS {name}"), &[], OPTS).await.unwrap();
    s.execute(&format!("CREATE TABLE {name} {ddl}"), &[], OPTS).await.unwrap();
    table(name)
}

/// MariaDB stores JSON as text and differs in a few labels.
fn is_mariadb(s: &dyn Session) -> bool {
    s.info().version.starts_with("MariaDB ")
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = ADAPTER.test(&with_password(&cfg, PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("MySQL ") || info.version.starts_with("MariaDB "), "{}", info.version);
    assert_eq!(info.database, DB);
    assert_eq!(info.engine, Engine::Mysql);
    assert!(info.tls.is_none());
}

#[tokio::test]
async fn invalid_credentials_are_actionable_and_redacted() {
    let cfg = require_db!();
    let secret = "wrong-Secret-123";
    let err = ADAPTER.test(&with_password(&cfg, secret)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert_eq!(err.code.as_deref(), Some("1045"));
    assert!(err.action.as_deref().unwrap_or_default().contains("username and password"));
    let json = serde_json::to_string(&err).unwrap_or_default();
    assert!(!format!("{err:?}{json}").contains(secret));
}

#[tokio::test]
async fn network_failure_is_classified() {
    let mut cfg = require_db!();
    cfg.port = 1;
    let err = ADAPTER.test(&with_password(&cfg, PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn tls_modes() {
    let mut cfg = require_db!();
    // The container's certificate is self-signed: full verification must refuse it.
    cfg.ssl_mode = SslMode::VerifyFull;
    let err = ADAPTER.test(&with_password(&cfg, PASSWORD)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
    // `require` encrypts without verifying, and reports the negotiated version.
    cfg.ssl_mode = SslMode::Require;
    let info = ADAPTER.test(&with_password(&cfg, PASSWORD)).await.unwrap();
    let tls = info.tls.expect("TLS in use");
    assert!(tls.version.unwrap_or_default().starts_with("TLS"));
    assert!(!tls.verified);
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    assert!(dbs.iter().any(|d| d.object.name == "billing"));
    assert!(!dbs.iter().any(|d| d.object.name == "mysql"), "system databases are hidden");
    let current = dbs.iter().find(|d| d.current).unwrap();
    let rels = s.list_children(Some(&current.object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert!(rels.iter().all(|r| r.object.schema.is_none() && r.object.database == DB));
    let found = s.search("ORDER", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "orders"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r =
        s.execute("SELECT big, exact, ts, doc, raw, label, id FROM type_samples ORDER BY id", &[], OPTS).await.unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("18446744073709551615"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert!(matches!(&row[2], Cell::Temporal { value, zoned: false } if value.ends_with(":00.123")), "{:?}", row[2]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[3])).unwrap()["name"], "Zoë ✓");
    assert!(matches!(row[3], Cell::Json { .. }) || is_mariadb(&*s), "{:?}", row[3]);
    assert_eq!(row[4], Cell::Binary { length: 8, head_hex: "89504e470d0a1a0a".into() });
    assert_eq!(row[5], Cell::Text { value: String::new(), truncated: None });
    let second = &r.rows[1];
    assert_eq!(second[1], Cell::number("-0.0000000001"));
    assert_eq!(second[2], Cell::Null);
    assert_eq!(second[5], Cell::Null);
    assert!(r.columns[0].numeric && !r.columns[5].numeric);
    assert_eq!(r.columns[0].type_name, "bigint unsigned");
    let e = s.execute("SELECT status FROM orders LIMIT 1", &[], OPTS).await.unwrap();
    assert_eq!(e.columns[0].type_name, "enum");
}

#[tokio::test]
async fn results_are_bounded_and_resumable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute(MILLION_ROWS, &[], OPTS).await.unwrap();
    assert_eq!(r.rows.len(), 500);
    assert!(r.has_more);
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(page.rows.len(), 500);
    assert!(page.has_more);
    // A new statement closes the open result promptly instead of reading it to the end.
    let started = Instant::now();
    let r = s.execute("SELECT 1", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(s.fetch_more(10).await.is_err());
}

#[tokio::test]
async fn cancel_and_timeout() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let runner = s.clone();
    let slow = format!("SELECT COUNT(*) FROM ({MILLION_ROWS}) x CROSS JOIN ({MILLION_ROWS}) y");
    let handle = tokio::spawn(async move { runner.execute(&slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(!s.cancel().await.unwrap(), "nothing left to cancel");

    let slow = format!("SELECT COUNT(*) FROM ({MILLION_ROWS}) x CROSS JOIN ({MILLION_ROWS}) y");
    let err = s.execute(&slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn one_statement_per_call() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT 1; DELETE FROM no_pk", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
}

#[tokio::test]
async fn errors_and_warnings() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT id, totl FROM orders", &[], OPTS).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("1054"));
    assert_eq!(err.kind, ErrorKind::Syntax);
    let r = s.execute("SELECT CAST('12abc' AS SIGNED)", &[], OPTS).await.unwrap();
    assert!(r.notices.iter().any(|n| n.contains("1292")), "{:?}", r.notices);
    let t = scratch(&*s, "it_affected", "(id INT PRIMARY KEY)").await;
    let r = s.execute("INSERT INTO it_affected VALUES (1), (2), (3)", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    s.execute(&format!("DROP TABLE {}", s.dialect().quoted_name(&t)), &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn structure_and_ddl() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert!(st.columns[0].primary_key && st.columns[0].generated && st.columns[0].editable);
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && !c.nullable));
    let fk = st.constraints.iter().find(|c| c.name == "orders_customer_fk").expect("fk");
    assert_eq!(fk.definition, "FOREIGN KEY (customer_id) REFERENCES customers (id)");
    assert!(st.indexes.iter().any(|i| i.name == "orders_created_at_idx" && !i.unique));
    assert!(st.indexes.iter().any(|i| i.primary));
    assert!(
        s.structure(&table("no_pk")).await.unwrap().row_identity.is_empty(),
        "a table without a key has no row identity"
    );
    // A NOT NULL unique key is accepted as row identity.
    let customers = s.structure(&table("customers")).await.unwrap();
    assert_eq!(customers.row_identity, ["id"]);
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE `orders` ("), "{ddl}");
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert_eq!(s.structure(&view).await.unwrap().object.kind, NodeKind::View);
    assert!(s.ddl(&view).await.unwrap().contains("VIEW `paid_orders`"));
    assert_eq!(s.structure(&table("missing_table")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn table_page_filter_sort_and_count() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let req = TablePageRequest {
        object: table("orders"),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await.unwrap();
    assert_eq!(page.rows.len(), 100);
    assert!(page.has_more);
    let totals: Vec<f64> = page.rows.iter().map(|r| text(&r[3]).parse().unwrap()).collect();
    assert!(totals.windows(2).all(|w| w[0] >= w[1]));
    let count = ops::row_count(&*s, &req.object, &req.rows).await.unwrap();
    assert_eq!((count.count, count.exact), (400, true));
    let bad = TablePageRequest {
        rows: RowFilter { filter: Some("1=1; DELETE FROM orders".into()), ..RowFilter::default() },
        ..req.clone()
    };
    assert!(ops::table_page(&*s, &bad).await.is_err());
    // Another database is reachable through qualified names.
    let other = ObjectRef { database: "billing".into(), ..table("invoices") };
    let page = ops::table_page(
        &*s,
        &TablePageRequest { object: other, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 },
    )
    .await
    .unwrap();
    assert_eq!(page.columns[0].name, "id");
}

#[tokio::test]
async fn catalog_connections_are_read_only() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.read_query(DB, "DELETE FROM no_pk", &[], dq_core::value::CellLimits::GRID).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("1792"), "{err:?}");
}

#[tokio::test]
async fn edits_apply_atomically_to_the_right_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_edits", "(id BIGINT UNSIGNED PRIMARY KEY, amount DECIMAL(12,2), note TEXT)").await;
    s.execute("INSERT INTO it_edits VALUES (18446744073709551615, 1.00, 'a'), (2, 2.00, 'b')", &[], OPTS)
        .await
        .unwrap();
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("18446744073709551615"))],
            values: vec![cv("amount", Some("84.50")), cv("note", Some(""))],
        },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("amount", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let preview = ops::preview_changes(&*s, &t, &changes, false).await.unwrap();
    assert!(preview.contains("UPDATE `dataquery_test`.`it_edits` SET `amount` = '84.50'"), "{preview}");
    let report = ops::apply_changes(&*s, &t, &changes, false).await.unwrap();
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT id, amount, note FROM it_edits ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(rows, [["3", "NULL", "NULL"], ["18446744073709551615", "84.50", ""]]);

    // Saving a value equal to the current one still counts as a matched row.
    let same = vec![RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("amount", None)] }];
    assert_eq!(ops::apply_changes(&*s, &t, &same, false).await.unwrap().updated, 1);

    // A change that matches no row rolls everything back.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("note", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    let err = ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
    let r = s.execute("SELECT note FROM it_edits WHERE id = 3", &[], OPTS).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Null);
    s.execute("DROP TABLE it_edits", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn tables_without_a_key_edit_only_after_opt_in() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_no_key", "(a INT, b VARCHAR(10), doc JSON)").await;
    s.execute("INSERT INTO it_no_key VALUES (1, NULL, '{}'), (2, 'x', '{}'), (2, 'x', '{}')", &[], OPTS).await.unwrap();
    let st = ops::structure(&*s, &t).await.unwrap();
    assert!(st.row_identity.is_empty(), "{:?}", st.row_identity);
    // MySQL's JSON never equals a text parameter; MariaDB's JSON is LONGTEXT and does.
    let expected: &[&str] = if is_mariadb(&*s) { &["a", "b", "doc"] } else { &["a", "b"] };
    assert_eq!(st.full_row_key, expected);
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    // Key values for whichever columns the server lets us compare.
    let key = |a: &str, b: Option<&str>| -> Vec<ColumnValue> {
        st.full_row_key
            .iter()
            .map(|c| match c.as_str() {
                "a" => cv("a", Some(a)),
                "b" => cv("b", b),
                other => cv(other, Some("{}")),
            })
            .collect()
    };
    let fix_null = vec![RowChange::Update { key: key("1", None), values: vec![cv("b", Some("y"))] }];
    // Without the opt-in nothing is editable.
    assert!(ops::apply_changes(&*s, &t, &fix_null, false).await.is_err());
    assert_eq!(ops::apply_changes(&*s, &t, &fix_null, true).await.unwrap().updated, 1);
    // Duplicate rows match twice, so the change is refused and rolled back.
    let dup = vec![RowChange::Delete { key: key("2", Some("x")) }];
    assert_eq!(ops::apply_changes(&*s, &t, &dup, true).await.unwrap_err().kind, ErrorKind::Constraint);
    let r = s.execute("SELECT COUNT(*) FROM it_no_key WHERE b IS NOT NULL", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "3");
    s.execute("DROP TABLE it_no_key", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn export_and_import_round_trip() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    let report =
        ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &TransferControl::none()).await?;
    assert_eq!(report.rows, 10);
    let json = dir.path().join("q.json");
    let q = ExportSource::Query { sql: "SELECT big, doc FROM type_samples ORDER BY id".into() };
    ops::export(&*s, &q, ExportFormat::Json, &ExportOptions::default(), &json, &TransferControl::none()).await?;
    let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(parsed[0]["big"], "18446744073709551615");
    // MySQL marks JSON columns, so documents are embedded. MariaDB's JSON is
    // LONGTEXT on the wire, so the document is exported as a string.
    let doc = if is_mariadb(&*s) {
        serde_json::from_str(parsed[0]["doc"].as_str().unwrap()).unwrap()
    } else {
        parsed[0]["doc"].clone()
    };
    assert_eq!(doc["name"], "Zoë ✓");

    let t = scratch(&*s, "it_import", "(id BIGINT PRIMARY KEY, status VARCHAR(20), total DECIMAL(12,2))").await;
    let csv_source = |path: &std::path::Path| ImportSource::Csv {
        path: path.to_string_lossy().into_owned(),
        options: CsvOptions { empty_as_null: true, ..CsvOptions::default() },
    };
    let mapping = |pairs: &[(&str, usize)]| -> Vec<ImportMapping> {
        pairs.iter().map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) }).collect()
    };
    let map = mapping(&[("id", 0), ("status", 2), ("total", 3)]);
    let none = TransferControl::none();
    let report = ops::import(&*s, &t, &csv_source(&csv), &map, &ImportOptions::default(), &none).await?;
    assert_eq!((report.rows, report.error_count), (10, 0));
    // Every bad row is reported with its number, and nothing is written.
    let bad = dir.path().join("bad.csv");
    std::fs::write(&bad, "id,x,status,total\n100,,ok,1\n101,,ok,not-a-number\n,,ok,2\n103,,ok,3\n").unwrap();
    let report = ops::import(&*s, &t, &csv_source(&bad), &map, &ImportOptions::default(), &none).await?;
    assert!(report.stopped);
    assert_eq!(report.rows, 0);
    assert_eq!(report.errors.iter().map(|e| e.row).collect::<Vec<_>>(), [2, 3]);
    assert!(report.errors[0].message.contains("total"));
    // Skipping invalid rows imports the rest.
    let skip = ImportOptions { skip_invalid: true, ..ImportOptions::default() };
    let report = ops::import(&*s, &t, &csv_source(&bad), &map, &skip, &none).await?;
    assert_eq!((report.rows, report.skipped), (2, 2));
    // With a commit size, rows committed before a server error stay.
    let dup = dir.path().join("dup.csv");
    std::fs::write(&dup, "id,x,status,total\n200,,a,1\n201,,b,1\n200,,c,1\n").unwrap();
    let chunks = ImportOptions { batch_rows: 1, commit_rows: 2, skip_invalid: false };
    let report = ops::import(&*s, &t, &csv_source(&dup), &map, &chunks, &none).await?;
    assert!(report.stopped && report.rows == 2, "{report:?}");
    // JSON import maps by key and keeps exact numbers.
    let json_in = dir.path().join("rows.json");
    std::fs::write(
        &json_in,
        r#"[{"id": 300, "status": "json", "total": 12.50}, {"id": 301, "status": null, "total": "7"}]"#,
    )
    .unwrap();
    let keys = ["id", "status", "total"]
        .iter()
        .map(|k| ImportMapping { column: (*k).into(), field: FieldRef::Key((*k).into()) })
        .collect::<Vec<_>>();
    let json_source = ImportSource::Json { path: json_in.to_string_lossy().into_owned() };
    assert_eq!(ops::import(&*s, &t, &json_source, &keys, &ImportOptions::default(), &none).await?.rows, 2);
    let r = s.execute("SELECT total FROM it_import WHERE id = 300", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "12.50");
    // SQL INSERT export runs back into the table it came from.
    let sql_file = dir.path().join("rows.sql");
    let rows_300 = ExportSource::Table {
        object: t.clone(),
        rows: RowFilter { filter: Some("id >= 300".into()), ..RowFilter::default() },
        sort: None,
    };
    let options = ExportOptions { sql_batch_rows: 1, ..ExportOptions::default() };
    ops::export(&*s, &rows_300, ExportFormat::SqlInsert, &options, &sql_file, &none).await?;
    s.execute("DELETE FROM it_import WHERE id >= 300", &[], OPTS).await?;
    let script = std::fs::read_to_string(&sql_file).unwrap();
    for statement in dq_core::sql::split_statements(&script, s.dialect().flavor()) {
        s.execute(statement.text(&script), &[], OPTS).await?;
    }
    let r = s.execute("SELECT count(*) FROM it_import WHERE id >= 300", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "2");
    // XLSX export and cancellation.
    let xlsx = dir.path().join("orders.xlsx");
    assert_eq!(ops::export(&*s, &source, ExportFormat::Xlsx, &ExportOptions::default(), &xlsx, &none).await?.rows, 10);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let control = TransferControl::new(cancelled, |_, _| {});
    let err =
        ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &control).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!csv.exists(), "a cancelled export leaves no partial file");
    s.execute("DROP TABLE it_import", &[], OPTS).await?;
    Ok(())
}

#[tokio::test]
async fn parameters_are_bound_with_prepared_statements() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let params = [Some("1".to_owned()), None, Some("it's; DROP TABLE x".to_owned())];
    let r = s
        .execute(
            "SELECT big, exact, ? IS NULL, ? FROM type_samples WHERE id = ?",
            &[params[1].clone(), params[2].clone(), params[0].clone()],
            OPTS,
        )
        .await
        .unwrap();
    // Unsigned BIGINT and DECIMAL keep every digit through the binary protocol.
    assert_eq!(text(&r.rows[0][0]), "18446744073709551615");
    assert_eq!(text(&r.rows[0][1]), "1234567890123456789012345678.0123456789");
    assert_eq!(text(&r.rows[0][2]), "1");
    assert_eq!(text(&r.rows[0][3]), "it's; DROP TABLE x");
    // Binary-protocol dates and times read like the text protocol.
    let sql = "SELECT ts, DATE(ts), TIME(ts) FROM type_samples WHERE id = 1";
    let plain = s.execute(sql, &[], OPTS).await.unwrap();
    let bound = s.execute(&format!("{sql} AND ? = 1"), &[Some("1".into())], OPTS).await.unwrap();
    for i in 0..3 {
        assert_eq!(text(&bound.rows[0][i]), text(&plain.rows[0][i]), "column {i}");
    }
    assert_eq!(text(&plain.rows[0][0]), "2026-10-14 09:59:00.123");
    // A prepared result can be paged like a plain one.
    let big = s
        .execute(
            "SELECT id FROM orders WHERE id > ?",
            &[Some("0".into())],
            ExecOptions { row_limit: 10, timeout_secs: 30 },
        )
        .await
        .unwrap();
    assert!(big.has_more);
    assert_eq!(s.fetch_more(10).await.unwrap().rows.len(), 10);
    let err = s.execute("SELECT ?, ?", &[Some("1".into())], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    let err = s.execute("INSERT INTO no_pk VALUES (99, 'ro')", &[], OPTS).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Permission | ErrorKind::Server), "{err:?}");
    assert_eq!(err.code.as_deref(), Some("1792"));
}

#[tokio::test]
async fn row_filters_bind_matches_and_searches() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let object = table("orders");
    let rows = RowFilter {
        matches: vec![ColumnValue { column: "status".into(), value: Some("paid".into()) }],
        searches: vec![dq_core::table::ColumnSearch { column: "status".into(), contains: "PAI".into() }],
        ..RowFilter::default()
    };
    // The filter builder's conditions select the same rows, and OR joins them.
    let cond = |op, value: &str| FilterCondition { column: "status".into(), op, value: value.into() };
    let count = |conditions, match_any| {
        let filter = RowFilter { conditions, match_any, ..RowFilter::default() };
        let (s, object) = (&s, &object);
        async move { ops::row_count(&**s, object, &filter).await.unwrap().count }
    };
    assert_eq!(
        count(vec![cond(FilterOp::Eq, "paid")], false).await,
        ops::row_count(&*s, &object, &rows).await.unwrap().count
    );
    assert_eq!(
        count(vec![cond(FilterOp::Eq, "paid"), cond(FilterOp::Ne, "paid")], true).await,
        count(vec![cond(FilterOp::IsNotNull, "")], false).await
    );
    assert_eq!(
        count(vec![cond(FilterOp::In, "paid, nope")], false).await,
        count(vec![cond(FilterOp::StartsWith, "pai")], false).await
    );
    let page =
        ops::table_page(&*s, &TablePageRequest { object: object.clone(), rows, sort: None, offset: 0, limit: 5 })
            .await
            .unwrap();
    assert!(!page.rows.is_empty(), "no rows");
    let none = RowFilter {
        searches: vec![dq_core::table::ColumnSearch { column: "status".into(), contains: "_aid".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &object, &none).await.unwrap().count, 0, "`_` is a literal");
}

#[tokio::test]
async fn explorer_lists_routines_indexes_and_triggers() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    for sql in [
        "DROP TABLE IF EXISTS it_events",
        "DROP FUNCTION IF EXISTS it_add",
        "DROP PROCEDURE IF EXISTS it_noop",
        "CREATE TABLE it_events (id INT AUTO_INCREMENT PRIMARY KEY, at DATETIME, KEY it_events_at (at))",
        "CREATE TRIGGER it_events_touch BEFORE INSERT ON it_events FOR EACH ROW SET NEW.at = NOW()",
        "CREATE FUNCTION it_add(a INT, b INT) RETURNS INT DETERMINISTIC RETURN a + b",
        "CREATE PROCEDURE it_noop() SELECT 1",
    ] {
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let db = ObjectRef::new(NodeKind::Database, DB, None, DB);
    let nodes = s.list_children(Some(&db)).await.unwrap();
    for group in ["functions", "procedures"] {
        let node = nodes.iter().find(|n| n.object.kind == NodeKind::Group && n.object.name == group).unwrap();
        let routines = s.list_children(Some(&node.object)).await.unwrap();
        let routine = routines.iter().find(|n| n.object.name.starts_with("it_")).unwrap();
        assert!(s.ddl(&routine.object).await.unwrap().starts_with("CREATE"));
    }
    let events = nodes.iter().find(|n| n.object.name == "it_events").unwrap();
    let children = s.list_children(Some(&events.object)).await.unwrap();
    let names: Vec<_> = children.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect();
    assert!(names.contains(&(NodeKind::Index, "PRIMARY")) && names.contains(&(NodeKind::Index, "it_events_at")));
    assert!(names.contains(&(NodeKind::Trigger, "it_events_touch")), "{names:?}");
    for child in &children {
        let ddl = s.ddl(&child.object).await.unwrap();
        assert!(ddl.contains("it_events"), "{ddl}");
    }
    let deps = s.dependencies(&table("it_events")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.kind == NodeKind::Trigger));

    let structure = ops::structure(&*s, &table("orders")).await.unwrap();
    assert_eq!(structure.foreign_keys.len(), 1);
    assert_eq!(structure.foreign_keys[0].target.name, "customers");
    assert!(structure.size.is_some());
    let model = s.schema_model(DB, None).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "orders" && t.columns.iter().any(|c| c.primary_key)));
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    let deps = s.dependencies(&table("customers")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.name == "orders"));
}

#[tokio::test]
async fn monitor_shows_blocking_and_cancels() {
    let cfg = require_db!();
    let holder = session(&cfg).await;
    let waiter = session(&cfg).await;
    let watcher = session(&cfg).await;
    scratch(&*holder, "it_monitor", "(id INT PRIMARY KEY, v INT)").await;
    holder.execute("INSERT INTO it_monitor VALUES (1, 0)", &[], OPTS).await.unwrap();
    let waiter_pid: i64 =
        text(&waiter.execute("SELECT CONNECTION_ID()", &[], OPTS).await.unwrap().rows[0][0]).parse().unwrap();
    holder.execute("BEGIN", &[], OPTS).await.unwrap();
    holder.execute("UPDATE it_monitor SET v = 1 WHERE id = 1", &[], OPTS).await.unwrap();
    let blocked = tokio::spawn({
        let waiter = waiter.clone();
        async move { waiter.execute("UPDATE it_monitor SET v = 2 WHERE id = 1", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("mysql monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        let snapshot = monitor.snapshot(30).await.unwrap();
        let blocked = snapshot.processes.iter().any(|p| p.pid == waiter_pid && !p.blocked_by.is_empty());
        let no_lock_view = snapshot.unavailable.iter().any(|u| u.starts_with("Lock waits"));
        if blocked || no_lock_view || Instant::now() > deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let waiting = snapshot.processes.iter().find(|p| p.pid == waiter_pid).expect("waiter listed");
    assert!(waiting.query.as_deref().unwrap_or_default().contains("UPDATE it_monitor"));
    let holder_pid: i64 =
        text(&holder.execute("SELECT CONNECTION_ID()", &[], OPTS).await.unwrap().rows[0][0]).parse().unwrap();
    if !snapshot.unavailable.iter().any(|u| u.starts_with("Lock waits")) {
        assert_eq!(waiting.blocked_by, [holder_pid], "{:?}", snapshot.unavailable);
        assert_eq!(snapshot.blocked, Some(1));
    }
    assert!(snapshot.connections.unwrap().max > 0);
    assert!(monitor.kill(waiter_pid, false).await.unwrap());
    let err = blocked.await.unwrap().unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Cancelled | ErrorKind::Server), "{err:?}");
    holder.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert!(monitor.server_settings(Some("max_conn")).await.unwrap().iter().any(|s| s.name == "max_connections"));
    assert!(!monitor.largest_tables(DB, 5).await.unwrap().is_empty(), "no largest tables");
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in dq_core::sql::split_statements(sql, dq_core::sql::SqlFlavor::Mysql) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

#[tokio::test]
async fn schema_changes_diff_and_migrations() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(&*s, "DROP DATABASE IF EXISTS it_diff_a; DROP DATABASE IF EXISTS it_diff_b; CREATE DATABASE it_diff_a; CREATE DATABASE it_diff_b").await;
    let t = |db: &str, name: &str| ObjectRef::new(NodeKind::Table, db, None, name);
    let col = |name: &str, ty: &str| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        default: None,
        identity: false,
    };
    let changes = vec![
        SchemaChange::CreateTable {
            table: t("it_diff_a", "accounts"),
            columns: vec![
                ColumnDef { identity: true, nullable: false, ..col("id", "bigint") },
                ColumnDef { default: Some("'x'".into()), ..col("email", "varchar(100)") },
            ],
            primary_key: vec!["id".into()],
        },
        SchemaChange::CreateTable {
            table: t("it_diff_a", "invoices"),
            columns: vec![
                ColumnDef { nullable: false, ..col("id", "int") },
                col("account_id", "bigint"),
                col("amount", "decimal(12,2)"),
            ],
            primary_key: vec!["id".into()],
        },
    ];
    let mut statements = Vec::new();
    for change in &changes {
        statements.extend(ddl::statements(s.dialect(), change, None).unwrap());
    }
    run_all(&*s, &ddl::preview(&statements)).await;
    // MODIFY keeps the default, comment and AUTO_INCREMENT it did not change.
    let structure = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    let modify = SchemaChange::AlterColumn {
        table: t("it_diff_a", "accounts"),
        column: "email".into(),
        data_type: Some("varchar(200)".into()),
        nullable: Some(false),
        default: None,
    };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &modify, Some(&structure)).unwrap())).await;
    let id_modify = SchemaChange::AlterColumn {
        table: t("it_diff_a", "accounts"),
        column: "id".into(),
        data_type: Some("bigint unsigned".into()),
        nullable: None,
        default: None,
    };
    let structure = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &id_modify, Some(&structure)).unwrap())).await;
    let after = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    let email = after.column("email").unwrap();
    assert_eq!(
        (email.data_type.as_str(), email.nullable, email.default.as_deref()),
        ("varchar(200)", false, Some("'x'"))
    );
    assert!(after.column("id").unwrap().identity);
    let set_default = SchemaChange::AlterColumn {
        table: t("it_diff_a", "accounts"),
        column: "email".into(),
        data_type: None,
        nullable: None,
        default: Some(DefaultChange::Drop),
    };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &set_default, None).unwrap())).await;

    let snapshot = |db: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model(db, None).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap().into());
            }
            tables
        }
    };
    let a = Container { database: "it_diff_a".into(), schema: None };
    let b = Container { database: "it_diff_b".into(), schema: None };
    let plan = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    run_all(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(again.items.is_empty(), "{:#?}\n{}", again.items, again.sql);

    run_all(&*s, "CREATE TABLE it_diff_a.migrations (id INT AUTO_INCREMENT PRIMARY KEY, migration VARCHAR(255), batch INT); \
                  INSERT INTO it_diff_a.migrations (migration, batch) VALUES ('2014_10_12_create_users', 1), ('2026_01_01_add_orders', 2)").await;
    let model = s.schema_model("it_diff_a", None).await.unwrap();
    let found = dq_core::migrations::inspect(&*s, &model).await.unwrap();
    let laravel = found.iter().find(|f| f.framework == "Laravel").unwrap();
    assert_eq!(
        (laravel.entries[0].version.as_str(), laravel.entries[0].batch.as_deref()),
        ("2026_01_01_add_orders", Some("2"))
    );
    run_all(&*s, "DROP DATABASE it_diff_a; DROP DATABASE it_diff_b").await;
}

/// Runs `sql` in one transaction on a write connection.
async fn transact(s: &dyn Session, sql: &[String]) -> DbResult<Vec<u64>> {
    let mut bound = sql.iter().map(|q| Ok(BoundStatement { sql: q.clone(), params: Vec::new(), expect_rows: None }));
    s.run_in_transaction(DB, &mut bound).await
}

#[tokio::test]
async fn write_connections_are_pooled_and_reset_after_errors() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_writer", "(id int PRIMARY KEY, conn bigint, note text)").await;
    let insert = |id: u32| format!("INSERT INTO {DB}.it_writer VALUES ({id}, CONNECTION_ID(), NULL)");
    for id in 1..=4 {
        transact(&*s, &[insert(id)]).await.unwrap();
    }
    // A duplicate key rolls back the whole transaction; the next one still works.
    assert!(transact(&*s, &[insert(5), insert(1)]).await.is_err());
    transact(&*s, &[insert(6)]).await.unwrap();
    let mut failing = [
        Ok(BoundStatement { sql: insert(7), params: Vec::new(), expect_rows: None }),
        Err(dq_core::DbError::invalid("stop")),
    ]
    .into_iter();
    assert!(s.run_in_transaction(DB, &mut failing).await.is_err());
    transact(&*s, &[insert(8)]).await.unwrap();
    // Two grid saves in a row.
    let cv = |c: &str, v: &str| ColumnValue { column: c.into(), value: Some(v.into()) };
    for note in ["a", "b"] {
        let change = RowChange::Update { key: vec![cv("id", "1")], values: vec![cv("note", note)] };
        ops::apply_changes(&*s, &t, &[change], false).await.unwrap();
    }
    let r = s.execute("SELECT id, conn, note FROM it_writer ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["1", "2", "3", "4", "6", "8"]);
    assert_eq!(rows[0][2], "b");
    let mut conns: Vec<&str> = rows[..4].iter().map(|r| r[1].as_str()).collect();
    conns.sort_unstable();
    conns.dedup();
    assert!(conns.len() < 4, "pooled connections are reused: {conns:?}");
    s.execute("DROP TABLE it_writer", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn table_actions_truncate_empty_duplicate_and_bulk_drop() {
    use dq_core::sql::ddl::SchemaChange;

    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(
        &*s,
        "DROP TABLE IF EXISTS ta_child, ta_parent, ta_parent_copy, ta_parent_shape; \
         CREATE TABLE ta_parent (id int AUTO_INCREMENT PRIMARY KEY, name varchar(20) NOT NULL DEFAULT 'x', \
           upper_name varchar(20) GENERATED ALWAYS AS (UPPER(name)) VIRTUAL, KEY ta_parent_name (name)) ENGINE = InnoDB; \
         CREATE TABLE ta_child (id int PRIMARY KEY, parent_id int, FOREIGN KEY (parent_id) REFERENCES ta_parent (id)) ENGINE = InnoDB; \
         INSERT INTO ta_parent (name) VALUES ('a'), ('b'), ('c'); INSERT INTO ta_child VALUES (1, 1)",
    )
    .await;
    let apply = |changes: Vec<SchemaChange>| {
        let s = s.clone();
        async move { transact(&*s, &ops::schema_statements(&*s, &changes).await?).await }
    };
    let scalar = |sql: &'static str| {
        let s = s.clone();
        async move { text(&s.execute(sql, &[], OPTS).await.unwrap().rows[0][0]) }
    };

    // Duplicate with rows: ids kept, the generated column recomputed, the index copied, AUTO_INCREMENT continued.
    let copy =
        SchemaChange::DuplicateTable { table: table("ta_parent"), new_name: "ta_parent_copy".into(), with_data: true };
    apply(vec![copy]).await.unwrap();
    assert_eq!(
        scalar("SELECT GROUP_CONCAT(id, ':', name, ':', upper_name ORDER BY id) FROM ta_parent_copy").await,
        "1:a:A,2:b:B,3:c:C"
    );
    s.execute("INSERT INTO ta_parent_copy (name) VALUES ('d')", &[], OPTS).await.unwrap();
    assert_eq!(scalar("SELECT MAX(id) FROM ta_parent_copy").await, "4");
    let copied = ops::structure(&*s, &table("ta_parent_copy")).await.unwrap();
    assert!(copied.indexes.iter().any(|i| i.name == "ta_parent_name"), "{:?}", copied.indexes);
    let shape = SchemaChange::DuplicateTable {
        table: table("ta_parent"),
        new_name: "ta_parent_shape".into(),
        with_data: false,
    };
    apply(vec![shape]).await.unwrap();
    assert_eq!(scalar("SELECT COUNT(*) FROM ta_parent_shape").await, "0");

    // InnoDB refuses to truncate a referenced table unless foreign key checks are off.
    let truncate = |disable_foreign_keys: bool| SchemaChange::TruncateTable {
        table: table("ta_parent"),
        restart_identity: false,
        cascade: false,
        disable_foreign_keys,
    };
    assert!(apply(vec![truncate(false)]).await.is_err());
    // Empty works where TRUNCATE is blocked: the child's rows go first, in the same run.
    let empty = |name: &str| SchemaChange::EmptyTable { table: table(name) };
    apply(vec![empty("ta_child"), empty("ta_parent_copy")]).await.unwrap();
    assert_eq!(scalar("SELECT COUNT(*) FROM ta_child").await, "0");
    apply(vec![truncate(true)]).await.unwrap();
    assert_eq!(scalar("SELECT COUNT(*) FROM ta_parent").await, "0");
    s.execute("INSERT INTO ta_parent (name) VALUES ('again')", &[], OPTS).await.unwrap();
    assert_eq!(scalar("SELECT MAX(id) FROM ta_parent").await, "1", "TRUNCATE resets AUTO_INCREMENT");
    // CASCADE is a PostgreSQL / Oracle option.
    let cascade = SchemaChange::TruncateTable {
        table: table("ta_parent"),
        restart_identity: false,
        cascade: true,
        disable_foreign_keys: false,
    };
    assert_eq!(ops::schema_statements(&*s, &[cascade]).await.unwrap_err().kind, ErrorKind::Unsupported);

    let drop = |name: &str| SchemaChange::DropTable { table: table(name), cascade: false };
    apply(vec![drop("ta_parent_copy"), drop("ta_parent_shape"), drop("ta_child"), drop("ta_parent")]).await.unwrap();
    assert_eq!(
        scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name LIKE 'ta\\_parent%'").await,
        "0"
    );
}
