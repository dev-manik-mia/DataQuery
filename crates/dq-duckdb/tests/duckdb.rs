//! Integration tests against real `DuckDB` database files in a temporary folder.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::ConnConfig;
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::sql::{SqlFlavor, split_statements};
use dq_core::table::{ColumnSearch, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_duckdb::DuckdbAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

const SEED: &str = "
CREATE TABLE customers (id BIGINT PRIMARY KEY, email VARCHAR NOT NULL UNIQUE, created_at TIMESTAMP DEFAULT now());
CREATE SEQUENCE order_ids START 1;
CREATE TABLE orders (
  id BIGINT PRIMARY KEY DEFAULT nextval('order_ids'),
  customer_id BIGINT NOT NULL REFERENCES customers (id),
  status VARCHAR NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'refunded')),
  total DECIMAL(12,2) NOT NULL,
  notes VARCHAR
);
CREATE INDEX orders_status_idx ON orders (status);
CREATE TABLE type_samples (id INTEGER PRIMARY KEY, big HUGEINT, exact DECIMAL(38,10), ts TIMESTAMPTZ, day DATE,
  doc JSON, raw BLOB, label VARCHAR, tags VARCHAR[], flag BOOLEAN, ratio DOUBLE, uid UUID);
CREATE TABLE no_pk (a INTEGER, b VARCHAR);
CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid';
CREATE MACRO double_it(x) AS x * 2;
INSERT INTO customers (id, email) SELECT g, 'user' || g || '@example.com' FROM range(1, 51) t(g);
INSERT INTO orders (customer_id, status, total, notes)
SELECT (g % 50) + 1, ['pending', 'paid', 'refunded'][(g % 3) + 1], round((g * 37 % 400 + 12) + (g % 100) / 100.0, 2),
       CASE g % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM range(1, 1201) t(g);
INSERT INTO type_samples VALUES
  (1, 170141183460469231731687303715884105727, 1234567890123456789012345678.0123456789,
   TIMESTAMPTZ '2026-10-14 09:59:00.123+00', DATE '2026-10-14', '{\"name\": \"Zoë ✓\", \"n\": 1}',
   '\\x89\\x50\\x4E\\x47'::BLOB, '', ['a', 'b c', NULL], true, 1.5, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'),
  (2, NULL, -0.0000000001, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
INSERT INTO no_pk VALUES (1, 'x'), (1, 'x');
";

fn config(path: &Path) -> ConnConfig {
    ConnConfig {
        engine: Engine::Duckdb,
        host: String::new(),
        port: 0,
        database: path.to_string_lossy().into_owned(),
        username: String::new(),
        ..ConnConfig::default()
    }
}

/// A seeded database file in its own temporary folder.
async fn seeded() -> (tempfile::TempDir, ConnConfig, Arc<dyn Session>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("sales.duckdb"));
    let s = DuckdbAdapter.connect(&ConnectParams::new(cfg.clone(), None)).await.expect("open");
    run_all(&*s, SEED).await;
    (dir, cfg, s)
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Duckdb) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, "sales", Some("main".into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn opens_files_and_memory_and_reports_errors() {
    let (_dir, cfg, s) = seeded().await;
    assert!(s.info().version.starts_with("DuckDB 1."), "{}", s.info().version);
    assert_eq!(s.info().database, "sales");
    assert!(DuckdbAdapter.test(&ConnectParams::new(ConnConfig { database: ":memory:".into(), ..cfg.clone() }, None)).await.is_ok());
    let missing = ConnConfig { database: "/no/such/folder/x.duckdb".into(), ..cfg.clone() };
    assert_eq!(DuckdbAdapter.test(&ConnectParams::new(missing, None)).await.unwrap_err().kind, ErrorKind::Storage);
    let relative = ConnConfig { database: "x.duckdb".into(), ..cfg };
    assert_eq!(DuckdbAdapter.test(&ConnectParams::new(relative, None)).await.unwrap_err().kind, ErrorKind::InvalidInput);
    let err = s.execute("SELECT nope FROM orders", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert_eq!(s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let (dir, _cfg, s) = seeded().await;
    let other = dir.path().join("billing.duckdb");
    run_all(&*s, &format!("ATTACH '{}' AS billing; CREATE TABLE billing.main.invoices (id INTEGER PRIMARY KEY, amount DECIMAL(12,2))", other.display())).await;
    let dbs = s.list_children(None).await.unwrap();
    let names: Vec<_> = dbs.iter().map(|d| d.object.name.as_str()).collect();
    assert_eq!(names, ["sales", "billing"]);
    assert!(dbs[0].current);
    let schemas = s.list_children(Some(&dbs[0].object)).await.unwrap();
    assert_eq!(schemas[0].object.name, "main");
    let rels = s.list_children(Some(&schemas[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    for group in ["sequences", "functions"] {
        let node = rels.iter().find(|r| r.object.kind == NodeKind::Group && r.object.name == group).unwrap();
        let children = s.list_children(Some(&node.object)).await.unwrap();
        assert!(s.ddl(&children[0].object).await.unwrap().starts_with("CREATE"), "{group}");
    }
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    let indexes = s.list_children(Some(&orders.object)).await.unwrap();
    assert!(indexes.iter().any(|i| i.object.name == "orders_status_idx"));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));
    // Attached databases page like the main one.
    let invoices = ObjectRef::new(NodeKind::Table, "billing", Some("main".into()), "invoices");
    let page = ops::table_page(&*s, &TablePageRequest { object: invoices, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 }).await.unwrap();
    assert_eq!(page.columns[0].name, "id");
}

#[tokio::test]
async fn value_rendering_contract() {
    let (_dir, _cfg, s) = seeded().await;
    let r = s.execute("SELECT big, exact, ts AT TIME ZONE 'UTC', day, doc, raw, label, tags, flag, ratio, uid FROM type_samples ORDER BY id", &[], OPTS).await.unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("170141183460469231731687303715884105727"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[2], Cell::Temporal { value: "2026-10-14 09:59:00.123".into(), zoned: false });
    assert_eq!(row[3], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[4])).unwrap()["name"], "Zoë ✓");
    assert_eq!(row[5], Cell::Binary { length: 4, head_hex: "89504e47".into() });
    assert_eq!(row[6], Cell::Text { value: String::new(), truncated: None });
    assert!(text(&row[7]).contains("b c"));
    assert_eq!((row[8].clone(), row[9].clone()), (Cell::Bool { value: true }, Cell::number("1.5")));
    assert_eq!(text(&row[10]), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(r.rows[1][1], Cell::number("-0.0000000001"));
    assert!(r.rows[1][2..].iter().all(|c| *c == Cell::Null));
    assert_eq!(r.columns[1].type_name, "DECIMAL(38,10)");
    assert!(r.columns[0].numeric && !r.columns[6].numeric);
    // Zoned values read in UTC with their offset, whatever the session time zone.
    s.execute("SET TimeZone = 'Asia/Dhaka'", &[], OPTS).await.unwrap();
    let zoned = s.execute("SELECT ts FROM type_samples WHERE id = 1", &[], OPTS).await.unwrap();
    assert_eq!(zoned.rows[0][0], Cell::Temporal { value: "2026-10-14 09:59:00.123+00:00".into(), zoned: true });
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let (_dir, _cfg, s) = seeded().await;
    let r = s.execute("SELECT range AS g FROM range(1, 10000001)", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    let r = s.execute("SELECT 1", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(s.fetch_more(10).await.is_err());
    let r = s.execute("SELECT * FROM range(3)", &[], ExecOptions { row_limit: 3, timeout_secs: 5 }).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (3, false));
    let slow = "SELECT count(*) FROM range(1000000000) a, range(1000) b";
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    assert_eq!(handle.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_secs(5));
    let err = s.execute(slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn counts_transactions_and_parameters() {
    let (_dir, _cfg, s) = seeded().await;
    let r = s.execute("UPDATE orders SET notes = 'x' WHERE id <= 3", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("UPDATE", Some(3)));
    s.execute("BEGIN TRANSACTION", &[], OPTS).await.unwrap();
    s.execute("DELETE FROM no_pk", &[], OPTS).await.unwrap();
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert_eq!(text(&s.execute("SELECT count(*) FROM no_pk", &[], OPTS).await.unwrap().rows[0][0]), "2");
    let r = s
        .execute("SELECT total, $2 IS NULL, $3 FROM orders WHERE id = $1", &[Some("7".into()), None, Some("it's; DROP".into())], OPTS)
        .await
        .unwrap();
    assert_eq!((text(&r.rows[0][1]).as_str(), text(&r.rows[0][2]).as_str()), ("true", "it's; DROP"));
}

#[tokio::test]
async fn structure_ddl_dependencies_and_model() {
    let (_dir, _cfg, s) = seeded().await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    let id = &st.columns[0];
    assert!(id.primary_key && id.generated);
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && !c.nullable && c.data_type == "DECIMAL(12,2)"));
    assert!(st.constraints.iter().any(|c| c.kind == dq_core::schema::ConstraintKind::ForeignKey && c.definition.contains("REFERENCES customers (id)")));
    assert!(st.constraints.iter().any(|c| c.kind == dq_core::schema::ConstraintKind::Check));
    assert!(st.indexes.iter().any(|i| i.name == "orders_status_idx"));
    assert_eq!(st.foreign_keys[0].target.name, "customers");
    assert!(s.structure(&table("no_pk")).await.unwrap().row_identity.is_empty());
    assert!(s.ddl(&table("orders")).await.unwrap().starts_with("CREATE TABLE orders"));
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert!(s.ddl(&view).await.unwrap().contains("VIEW paid_orders"));
    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "customers"));
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders"));
    let model = s.schema_model("sales", Some("main")).await.unwrap();
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    assert_eq!(s.structure(&table("missing")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn pages_edits_and_transfers() -> DbResult<()> {
    let (dir, _cfg, s) = seeded().await;
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

    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("total", Some("84.50")), cv("notes", Some("Zoë ✓"))] },
        RowChange::Insert { values: vec![cv("customer_id", Some("1")), cv("total", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &table("orders"), &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT total, notes FROM orders WHERE id = 1", &[], OPTS).await?;
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("84.50", "Zoë ✓"));
    let failing = vec![RowChange::Delete { key: vec![cv("id", Some("404040"))] }];
    assert_eq!(ops::apply_changes(&*s, &table("orders"), &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);

    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: table("orders"), rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() }, sort: None };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 9);
    // DuckDB reads the exported file straight from SQL.
    let r = s.execute(&format!("SELECT count(*) FROM read_csv('{}')", csv.display()), &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "9");
    run_all(&*s, "CREATE TABLE it_import (id BIGINT PRIMARY KEY, status VARCHAR, total DECIMAL(12,2), raw BLOB)").await;
    let csv_source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions { empty_as_null: true, ..CsvOptions::default() } };
    let map: Vec<ImportMapping> =
        [("id", 0), ("status", 2), ("total", 3)].iter().map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) }).collect();
    assert_eq!(ops::import(&*s, &table("it_import"), &csv_source, &map, &ImportOptions::default(), &none).await?.rows, 9);
    s.execute("UPDATE it_import SET raw = from_hex('0102ff') WHERE id = 1", &[], OPTS).await?;
    let sql_file = dir.path().join("rows.sql");
    ops::export(&*s, &ExportSource::Table { object: table("it_import"), rows: RowFilter::default(), sort: None }, ExportFormat::SqlInsert, &ExportOptions::default(), &sql_file, &none).await?;
    s.execute("DELETE FROM it_import", &[], OPTS).await?;
    run_all(&*s, &std::fs::read_to_string(&sql_file).unwrap()).await;
    let r = s.execute("SELECT count(*), max(raw) FROM it_import", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "9");
    assert_eq!(r.rows[0][1], Cell::Binary { length: 3, head_hex: "0102ff".into() });
    let parquet = dir.path().join("orders.parquet");
    run_all(&*s, &format!("COPY orders TO '{}' (FORMAT parquet)", parquet.display())).await;
    let r = s.execute(&format!("SELECT count(*) FROM '{}'", parquet.display()), &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "1200");
    Ok(())
}

#[tokio::test]
async fn read_only_files_refuse_writes() {
    let (_dir, cfg, s) = seeded().await;
    s.close().await;
    drop(s);
    let ro = DuckdbAdapter.connect(&ConnectParams::new(ConnConfig { read_only: true, ..cfg }, None)).await.unwrap();
    assert!(ro.info().read_only);
    assert!(ro.execute("SELECT count(*) FROM orders", &[], OPTS).await.is_ok());
    assert_eq!(ro.execute("DELETE FROM orders", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
    let changes = vec![RowChange::Delete { key: vec![ColumnValue { column: "id".into(), value: Some("1".into()) }] }];
    assert_eq!(ops::apply_changes(&*ro, &table("orders"), &changes, false).await.unwrap_err().kind, ErrorKind::Permission);
}

#[tokio::test]
async fn explain_and_schema_changes() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let (_dir, _cfg, s) = seeded().await;
    let plan = s.explain("SELECT * FROM orders WHERE status = 'paid'", &[], false, 30).await.unwrap();
    assert!(plan.plan.is_some(), "{}", plan.raw);
    let analyzed = s.explain("SELECT count(*) FROM orders", &[], true, 30).await.unwrap();
    assert!(analyzed.execution_ms.is_some(), "{}", analyzed.raw);

    run_all(&*s, "CREATE SCHEMA a; CREATE SCHEMA b").await;
    let t = |schema: &str, name: &str| ObjectRef::new(NodeKind::Table, "sales", Some(schema.into()), name);
    let col = |name: &str, ty: &str| ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, default: None, identity: false };
    let changes = vec![
        SchemaChange::CreateTable {
            table: t("a", "accounts"),
            columns: vec![ColumnDef { nullable: false, ..col("id", "BIGINT") }, ColumnDef { default: Some("'x'".into()), ..col("email", "VARCHAR") }],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddColumn { table: t("a", "accounts"), column: col("note", "VARCHAR") },
        SchemaChange::RenameColumn { table: t("a", "accounts"), column: "note".into(), new_name: "remark".into() },
        SchemaChange::AlterColumn {
            table: t("a", "accounts"),
            column: "email".into(),
            data_type: Some("VARCHAR(200)".into()),
            nullable: Some(false),
            default: Some(DefaultChange::Set { expression: "'y'".into() }),
        },
        SchemaChange::CreateIndex { table: t("a", "accounts"), name: "accounts_email".into(), columns: vec![ddl::IndexColumn { name: "email".into(), descending: false }], unique: false },
    ];
    for change in &changes {
        run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    let email = ops::structure(&*s, &t("a", "accounts")).await.unwrap().column("email").cloned().unwrap();
    assert!(!email.nullable && email.default.as_deref() == Some("'y'"), "{email:?}");
    let snapshot = |schema: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model("sales", Some(schema)).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap());
            }
            tables
        }
    };
    let a = Container { database: "sales".into(), schema: Some("a".into()) };
    let b = Container { database: "sales".into(), schema: Some("b".into()) };
    let plan = diff(s.dialect(), &snapshot("a").await, &a, &snapshot("b").await, &b).unwrap();
    run_all(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot("a").await, &a, &snapshot("b").await, &b).unwrap();
    assert!(again.items.is_empty(), "{:#?}\n{}", again.items, again.sql);
}
