//! Integration tests against a real `CockroachDB` node, through the PostgreSQL adapter.
//!
//! Start it with `docker compose -p dataquery up -d cockroach` and run
//! `DATAQUERY_TEST_COCKROACH=localhost:56257 cargo test -p dq-postgres --test cockroach`.
//! Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::sql::{SqlFlavor, split_statements};
use dq_core::table::{ColumnSearch, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_postgres::PostgresAdapter;

const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const ADAPTER: PostgresAdapter = PostgresAdapter::COCKROACH;

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_COCKROACH").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "26257"));
    Some(ConnConfig {
        engine: Engine::Cockroachdb,
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
                eprintln!("skipped: set DATAQUERY_TEST_COCKROACH to run CockroachDB integration tests");
                return;
            }
        }
    };
}

/// The test node runs `--insecure`, which accepts any password.
fn params(cfg: &ConnConfig) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some("dataquery".into()))
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ADAPTER.connect(&params(cfg)).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, Some("public".into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Postgres) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    run_all(s, &format!("DROP TABLE IF EXISTS public.{name}; CREATE TABLE public.{name} {ddl}")).await;
    table(name)
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = ADAPTER.test(&params(&cfg)).await.unwrap();
    assert!(info.version.starts_with("CockroachDB 25."), "{}", info.version);
    assert_eq!((info.engine, info.database.as_str(), info.user.as_str()), (Engine::Cockroachdb, DB, "dataquery"));
    let mut other = cfg.clone();
    other.database = String::new();
    assert_eq!(ADAPTER.test(&params(&other)).await.unwrap().database, "defaultdb");
}

#[tokio::test]
async fn network_failure_is_classified() {
    let mut cfg = require_db!();
    cfg.port = 1;
    let err = ADAPTER.test(&params(&cfg)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    let current = dbs.iter().find(|d| d.current).unwrap();
    let schemas = s.list_children(Some(&current.object)).await.unwrap();
    let names: Vec<_> = schemas.iter().map(|n| n.object.name.as_str()).collect();
    assert!(names.contains(&"public") && names.contains(&"billing"), "{names:?}");
    assert!(!names.contains(&"crdb_internal") && !names.contains(&"pg_catalog"), "{names:?}");
    let public = schemas.iter().find(|n| n.object.name == "public").unwrap();
    let rels = s.list_children(Some(&public.object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert_eq!(kind("daily_revenue"), Some(NodeKind::MaterializedView));
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    assert!(children.iter().any(|c| c.object.kind == NodeKind::Index && c.object.name == "orders_created_at_idx"));
    let found = s.search("ORDER", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "orders"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT big, exact, ts, doc, raw, label, tags, uid, addr, span FROM type_samples ORDER BY id",
            &[],
            OPTS,
        )
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9007199254740993"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert!(matches!(&row[2], Cell::Temporal { zoned: true, .. }), "{:?}", row[2]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[3])).unwrap()["name"], "Zoë ✓");
    assert_eq!(row[4], Cell::Binary { length: 8, head_hex: "89504e470d0a1a0a".into() });
    assert_eq!(row[5], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(text(&row[7]), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(r.rows[1][1], Cell::number("-0.0000000001"));
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("SELECT g FROM generate_series(1, 1000000) g", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    let started = Instant::now();
    assert_eq!(text(&s.execute("SELECT 1", &[], OPTS).await.unwrap().rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5));

    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute("SELECT pg_sleep(30)", &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    let err = s.execute("SELECT pg_sleep(30)", &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Timeout | ErrorKind::Cancelled), "{err:?}");
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn structure_and_ddl() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && !c.nullable));
    assert!(st.constraints.iter().any(|c| c.name == "orders_customer_fk"));
    assert!(st.indexes.iter().any(|i| i.name == "orders_created_at_idx" && !i.unique));
    assert_eq!(st.foreign_keys[0].target.name, "customers");
    // CockroachDB adds a hidden `rowid` key to tables without one, so even they can be edited safely.
    let no_pk = s.structure(&table("no_pk")).await.unwrap();
    assert_eq!(no_pk.row_identity, ["rowid"]);
    let rowid = no_pk.column("rowid").unwrap();
    assert!(rowid.generated && !rowid.editable);
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(ddl.contains("CREATE TABLE"), "{ddl}");
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert!(s.ddl(&view).await.unwrap().contains("paid"));
    let deps = s.dependencies(&table("customers")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.name == "orders"));
    let model = s.schema_model(DB, Some("public")).await.unwrap();
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
}

#[tokio::test]
async fn table_pages_edits_and_transfers() -> DbResult<()> {
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
    let search = RowFilter {
        searches: vec![ColumnSearch { column: "status".into(), contains: "PAI".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &search).await?.count, 400);

    let t = scratch(&*s, "it_edits", "(id INT8 PRIMARY KEY, amount DECIMAL(12,2), note STRING)").await;
    s.execute("INSERT INTO it_edits VALUES (1, 1.00, 'a'), (2, 2.00, 'b')", &[], OPTS).await?;
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("amount", Some("84.50"))] },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("amount", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &t, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let failing = vec![RowChange::Delete { key: vec![cv("id", Some("404"))] }];
    assert_eq!(ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);

    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 10);
    let imported = scratch(&*s, "it_import", "(id INT8 PRIMARY KEY, status STRING, total DECIMAL(12,2))").await;
    let csv_source = ImportSource::Csv {
        path: csv.to_string_lossy().into_owned(),
        options: CsvOptions { empty_as_null: true, ..CsvOptions::default() },
    };
    let map: Vec<ImportMapping> = [("id", 0), ("status", 2), ("total", 3)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    assert_eq!(ops::import(&*s, &imported, &csv_source, &map, &ImportOptions::default(), &none).await?.rows, 10);
    run_all(&*s, "DROP TABLE it_edits; DROP TABLE it_import").await;
    Ok(())
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    let err = s.execute("INSERT INTO no_pk VALUES (99, 'ro')", &[], OPTS).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Permission | ErrorKind::Server), "{err:?}");
}

#[tokio::test]
async fn explain_reads_the_text_tree() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let plan = s.explain("SELECT * FROM orders WHERE status = 'paid' ORDER BY total", &[], false, 30).await.unwrap();
    let root = plan.plan.expect("plan tree");
    let mut labels = Vec::new();
    let mut stack = vec![&root];
    while let Some(n) = stack.pop() {
        labels.push(n.label.clone());
        stack.extend(n.children.iter());
    }
    assert!(labels.iter().any(|l| l == "scan"), "{labels:?}\n{}", plan.raw);
    let analyzed = s.explain("SELECT count(*) FROM orders", &[], true, 30).await.unwrap();
    assert!(analyzed.execution_ms.is_some() || analyzed.raw.contains("execution time"), "{}", analyzed.raw);
}

#[tokio::test]
async fn monitor_lists_sessions_and_cancels() {
    let cfg = require_db!();
    let runner = session(&cfg).await;
    let watcher = session(&cfg).await;
    let slow = tokio::spawn({
        let runner = runner.clone();
        async move { runner.execute("SELECT pg_sleep(20)", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("cockroach monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let (snapshot, pid) = loop {
        let snapshot = monitor.snapshot(1).await.unwrap();
        let pid = snapshot
            .processes
            .iter()
            .find(|p| p.query.as_deref().is_some_and(|q| q.contains("pg_sleep(20)")))
            .map(|p| p.pid);
        if pid.is_some() || Instant::now() > deadline {
            break (snapshot, pid);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let pid = pid.unwrap_or_else(|| panic!("{:#?}", snapshot.processes));
    assert!(snapshot.active_queries >= Some(1));
    assert!(monitor.kill(pid, false).await.unwrap());
    assert!(slow.await.unwrap().is_err());
    assert!(!monitor.server_settings(Some("sql.defaults")).await.unwrap().is_empty());
    assert!(monitor.largest_tables(DB, 5).await.unwrap().iter().any(|t| t.object.name == "orders"));
}

#[tokio::test]
async fn schema_changes_and_diff() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(&*s, "DROP SCHEMA IF EXISTS it_diff_a CASCADE; DROP SCHEMA IF EXISTS it_diff_b CASCADE; CREATE SCHEMA it_diff_a; CREATE SCHEMA it_diff_b").await;
    let t = |schema: &str, name: &str| ObjectRef::new(NodeKind::Table, DB, Some(schema.into()), name);
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
        SchemaChange::AddColumn { table: t("it_diff_a", "accounts"), column: col("note", "text") },
        SchemaChange::AlterColumn {
            table: t("it_diff_a", "accounts"),
            column: "email".into(),
            data_type: None,
            nullable: Some(false),
            default: Some(DefaultChange::Set { expression: "'y'".into() }),
        },
    ];
    for change in &changes {
        run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    let email = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap().column("email").cloned().unwrap();
    assert_eq!((email.nullable, email.default.as_deref().map(|d| d.starts_with("'y'"))), (false, Some(true)));
    let snapshot = |schema: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model(DB, Some(schema)).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap());
            }
            tables
        }
    };
    let a = Container { database: DB.into(), schema: Some("it_diff_a".into()) };
    let b = Container { database: DB.into(), schema: Some("it_diff_b".into()) };
    let plan = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    run_all(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(again.items.is_empty(), "{:#?}\n{}", again.items, again.sql);
    run_all(&*s, "DROP SCHEMA it_diff_a CASCADE; DROP SCHEMA it_diff_b CASCADE").await;
}
