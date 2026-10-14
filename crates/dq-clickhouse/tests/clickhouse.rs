//! Integration tests against a real `ClickHouse` server.
//!
//! Start it with `docker compose -p dataquery up -d clickhouse` and run
//! `DATAQUERY_TEST_CLICKHOUSE=localhost:58123 cargo test -p dq-clickhouse`.
//! Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::table::{ColumnSearch, RowFilter, SortSpec, TablePageRequest};
use dq_core::edit::ColumnValue;
use dq_core::transfer::{CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_clickhouse::ClickhouseAdapter;

const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_CLICKHOUSE").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "8123"));
    Some(ConnConfig {
        engine: Engine::Clickhouse,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_CLICKHOUSE to run ClickHouse integration tests");
                return;
            }
        }
    };
}

fn params(cfg: &ConnConfig, password: &str) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some(password.into()))
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ClickhouseAdapter.connect(&params(cfg, "dataquery")).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, None, name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn connects_and_reports_errors() {
    let cfg = require_db!();
    let info = ClickhouseAdapter.test(&params(&cfg, "dataquery")).await.unwrap();
    assert!(info.version.starts_with("ClickHouse 25."), "{}", info.version);
    assert_eq!((info.database.as_str(), info.user.as_str(), info.engine), (DB, "dataquery", Engine::Clickhouse));
    let secret = "wrong-Secret-123";
    let err = ClickhouseAdapter.test(&params(&cfg, secret)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(!format!("{err:?}").contains(secret));
    let err = ClickhouseAdapter.test(&params(&ConnConfig { port: 1, ..cfg.clone() }, "dataquery")).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
    let s = session(&cfg).await;
    let err = s.execute("SELECT nope FROM orders", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax, "{err:?}");
    assert_eq!(s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    assert!(dbs.iter().any(|d| d.object.name == "billing"));
    assert!(!dbs.iter().any(|d| d.object.name == "system"));
    let current = dbs.iter().find(|d| d.current).unwrap();
    let rels = s.list_children(Some(&current.object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert_eq!(kind("daily_revenue_mv"), Some(NodeKind::MaterializedView));
    assert_eq!(rels.iter().find(|r| r.object.name == "orders").unwrap().row_estimate, Some(1200));
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    let indexes = s.list_children(Some(&orders.object)).await.unwrap();
    assert!(indexes.iter().any(|i| i.object.name == "orders_status_idx"));
    assert!(s.ddl(&indexes[0].object).await.unwrap().contains("ADD INDEX"));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute("SELECT big, huge, exact, ts, day, doc, label, tags, attrs, flag, ratio, uid, ip FROM type_samples ORDER BY id", &[], OPTS)
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("18446744073709551615"));
    assert_eq!(row[1], Cell::number("-170141183460469231731687303715884105728"));
    assert_eq!(row[2], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[3], Cell::Temporal { value: "2026-10-14 09:59:00.123".into(), zoned: false });
    assert_eq!(row[4], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[5])).unwrap()["name"], "Zoë ✓");
    assert_eq!(row[6], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(text(&row[7]), "[\"a\",\"b c\"]");
    assert_eq!(text(&row[8]), "{\"x\":1}");
    assert_eq!(row[9], Cell::Bool { value: true });
    assert_eq!(row[10], Cell::number("1.5"));
    assert_eq!(text(&row[11]), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(text(&row[12]), "192.168.0.1");
    assert_eq!(r.rows[1][2], Cell::number("-0.0000000001"));
    assert_eq!(r.rows[1][6], Cell::Null);
    assert!(r.columns[0].numeric && !r.columns[6].numeric);
    assert_eq!(r.columns[2].type_name, "Decimal(38, 10)");
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("SELECT number FROM system.numbers LIMIT 10000000", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "500");
    let started = Instant::now();
    assert_eq!(text(&s.execute("SELECT 1", &[], OPTS).await.unwrap().rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(s.fetch_more(10).await.is_err());
    let slow = "SELECT count() FROM numbers_mt(100000000000) WHERE sipHash64(number) % 7 = 3";
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(cancelled_at.elapsed() < Duration::from_secs(10));
    let err = s.execute(slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn sessions_parameters_and_writes() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    // The server-side session keeps settings and temporary tables.
    s.execute("CREATE TEMPORARY TABLE scratch (id UInt32)", &[], OPTS).await.unwrap();
    let r = s.execute("INSERT INTO scratch SELECT number FROM numbers(3)", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    assert_eq!(text(&s.execute("SELECT count() FROM scratch", &[], OPTS).await.unwrap().rows[0][0]), "3");
    s.execute("USE billing", &[], OPTS).await.unwrap();
    assert_eq!(text(&s.execute("SELECT currentDatabase()", &[], OPTS).await.unwrap().rows[0][0]), "billing");
    let r = s
        .execute(
            "SELECT total, {note:Nullable(String)} IS NULL, {text:String} FROM dataquery_test.orders WHERE id = {id:UInt64}",
            &[Some("7".into()), None, Some("it's; DROP".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!((text(&r.rows[0][1]).as_str(), text(&r.rows[0][2]).as_str()), ("true", "it's; DROP"));
}

#[tokio::test]
async fn read_only_profiles_are_enforced_by_the_server() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert!(s.execute("SELECT count() FROM orders", &[], OPTS).await.is_ok());
    let err = s.execute("INSERT INTO no_key VALUES (9, 'ro')", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
}

#[tokio::test]
async fn structure_ddl_and_model() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert!(st.row_identity.is_empty(), "no grid edits");
    assert!(st.columns.iter().all(|c| !c.editable));
    let total = st.column("total").unwrap();
    assert!(total.numeric && !total.nullable && total.data_type == "Decimal(12, 2)");
    assert!(st.column("discount").unwrap().nullable);
    assert!(st.indexes.iter().any(|i| i.primary && i.columns == ["id"]));
    assert!(st.indexes.iter().any(|i| i.name == "orders_status_idx"));
    assert!(st.size.is_some_and(|s| s.total_bytes > 0));
    assert!(s.ddl(&table("orders")).await.unwrap().starts_with("CREATE TABLE dataquery_test.orders"));
    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.name == "daily_revenue_mv"), "{deps:?}");
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert!(s.dependencies(&view).await.unwrap().uses.iter().any(|d| d.object.name == "orders"));
    let model = s.schema_model(DB, None).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "orders" && t.columns.iter().any(|c| c.primary_key)));
    assert_eq!(s.structure(&table("missing")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn pages_and_transfers() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let req = TablePageRequest {
        object: table("orders"),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await?;
    assert_eq!((page.rows.len(), page.has_more), (100, true));
    assert_eq!(ops::row_count(&*s, &req.object, &req.rows).await?.count, 400);
    let search = RowFilter { searches: vec![ColumnSearch { column: "status".into(), contains: "PAI".into() }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &search).await?.count, 400);
    let literal = RowFilter { searches: vec![ColumnSearch { column: "status".into(), contains: "_aid".into() }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &literal).await?.count, 0);
    let by_id = RowFilter { matches: vec![ColumnValue { column: "id".into(), value: Some("7".into()) }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &by_id).await?.count, 1);
    let nulls = RowFilter { matches: vec![ColumnValue { column: "discount".into(), value: None }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &nulls).await?.count, 400);
    let other = ObjectRef::new(NodeKind::Table, "billing", None, "invoices");
    let page = ops::table_page(&*s, &TablePageRequest { object: other, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 }).await?;
    assert_eq!(page.rows.len(), 2);

    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: table("orders"), rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() }, sort: None };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 10);
    s.execute("DROP TABLE IF EXISTS it_import", &[], OPTS).await?;
    s.execute("CREATE TABLE it_import (id UInt64, status String, total Decimal(12, 2)) ENGINE = MergeTree ORDER BY id", &[], OPTS).await?;
    let csv_source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions { empty_as_null: true, ..CsvOptions::default() } };
    let map: Vec<ImportMapping> =
        [("id", 0), ("status", 2), ("total", 3)].iter().map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) }).collect();
    assert_eq!(ops::import(&*s, &table("it_import"), &csv_source, &map, &ImportOptions::default(), &none).await?.rows, 10);
    let sql_file = dir.path().join("rows.sql");
    ops::export(&*s, &ExportSource::Table { object: table("it_import"), rows: RowFilter::default(), sort: None }, ExportFormat::SqlInsert, &ExportOptions::default(), &sql_file, &none).await?;
    s.execute("TRUNCATE TABLE it_import", &[], OPTS).await?;
    for statement in dq_core::sql::split_statements(&std::fs::read_to_string(&sql_file).unwrap(), s.dialect().flavor()) {
        let script = std::fs::read_to_string(&sql_file).unwrap();
        s.execute(statement.text(&script), &[], OPTS).await?;
    }
    assert_eq!(text(&s.execute("SELECT count(), sum(total) FROM it_import", &[], OPTS).await?.rows[0][0]), "10");
    s.execute("DROP TABLE it_import", &[], OPTS).await?;
    Ok(())
}

#[tokio::test]
async fn explain_monitoring_and_schema_changes() {
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    let plan = s.explain("SELECT * FROM orders WHERE id = {id:UInt64}", &[Some("7".into())], false, 30).await.unwrap();
    assert!(plan.plan.is_some(), "{}", plan.raw);
    assert_eq!(s.explain("SELECT 1", &[], true, 30).await.unwrap_err().kind, ErrorKind::Unsupported);

    let runner = session(&cfg).await;
    let watcher = session(&cfg).await;
    let slow = tokio::spawn({
        let runner = runner.clone();
        async move { runner.execute("SELECT sleepEachRow(1) FROM numbers(30) SETTINGS max_block_size = 1", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("clickhouse monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        let snapshot = monitor.snapshot(1).await.unwrap();
        if let Some(p) = snapshot.processes.iter().find(|p| p.query.as_deref().is_some_and(|q| q.contains("sleepEachRow"))) {
            break p.pid;
        }
        assert!(Instant::now() < deadline, "the slow query never showed up");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(monitor.kill(pid, false).await.unwrap());
    assert!(slow.await.unwrap().is_err());
    assert!(monitor.server_settings(Some("max_threads")).await.unwrap().iter().any(|s| s.name == "max_threads"));
    assert!(monitor.largest_tables(DB, 5).await.unwrap().iter().any(|t| t.object.name == "orders"));

    s.execute("DROP TABLE IF EXISTS it_accounts", &[], OPTS).await.unwrap();
    let col = |name: &str, ty: &str| ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, default: None, identity: false };
    let changes = vec![
        SchemaChange::CreateTable {
            table: table("it_accounts"),
            columns: vec![ColumnDef { nullable: false, ..col("id", "UInt64") }, col("email", "String")],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddColumn { table: table("it_accounts"), column: ColumnDef { default: Some("0".into()), nullable: false, ..col("qty", "UInt32") } },
        SchemaChange::RenameColumn { table: table("it_accounts"), column: "qty".into(), new_name: "quantity".into() },
    ];
    for change in &changes {
        for statement in ddl::statements(s.dialect(), change, None).unwrap() {
            s.execute(&statement, &[], OPTS).await.unwrap_or_else(|e| panic!("{statement}: {e:?}"));
        }
    }
    let structure = ops::structure(&*s, &table("it_accounts")).await.unwrap();
    let alter = SchemaChange::AlterColumn {
        table: table("it_accounts"),
        column: "email".into(),
        data_type: None,
        nullable: Some(false),
        default: Some(DefaultChange::Set { expression: "'none'".into() }),
    };
    for statement in ddl::statements(s.dialect(), &alter, Some(&structure)).unwrap() {
        s.execute(&statement, &[], OPTS).await.unwrap_or_else(|e| panic!("{statement}: {e:?}"));
    }
    let email = ops::structure(&*s, &table("it_accounts")).await.unwrap().column("email").cloned().unwrap();
    assert_eq!((email.data_type.as_str(), email.default.as_deref()), ("String", Some("'none'")));
    s.execute("DROP TABLE it_accounts", &[], OPTS).await.unwrap();
}
