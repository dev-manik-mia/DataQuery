//! Integration tests against real `SQLite` database files in a temporary folder.

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
use dq_core::table::{ColumnSearch, FilterCondition, FilterOp, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_sqlite::SqliteAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

const SEED: &str = "
CREATE TABLE customers (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, created_at TEXT DEFAULT CURRENT_TIMESTAMP);
CREATE TABLE orders (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers (id) ON DELETE CASCADE,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'refunded')),
  total NUMERIC(12,2) NOT NULL,
  notes TEXT
);
CREATE INDEX orders_status_idx ON orders (status);
CREATE TABLE type_samples (id INTEGER PRIMARY KEY, big INTEGER, ratio REAL, doc JSON, raw BLOB, label TEXT,
  total REAL GENERATED ALWAYS AS (ratio * 2) VIRTUAL);
CREATE TABLE no_pk (a INTEGER, b TEXT);
CREATE TABLE audit (order_id INTEGER, status TEXT);
CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid';
CREATE TRIGGER orders_audit AFTER UPDATE OF status ON orders BEGIN
  INSERT INTO audit VALUES (new.id, CASE WHEN new.status = 'paid' THEN 'PAID' ELSE new.status END);
END;
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 50)
INSERT INTO customers (id, email) SELECT n, 'user' || n || '@example.com' FROM g;
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1200)
INSERT INTO orders (customer_id, status, total, notes)
SELECT (n % 50) + 1, CASE n % 3 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' ELSE 'refunded' END,
       round((n * 37 % 400 + 12) + (n % 100) / 100.0, 2),
       CASE n % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM g;
INSERT INTO type_samples (id, big, ratio, doc, raw, label) VALUES
  (1, 9223372036854775807, 1.5, '{\"name\": \"Zoë ✓\"}', X'89504E47', ''),
  (2, NULL, NULL, NULL, NULL, NULL);
INSERT INTO no_pk VALUES (1, 'x'), (1, 'x');
";

fn config(path: &Path) -> ConnConfig {
    ConnConfig {
        engine: Engine::Sqlite,
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
    let cfg = config(&dir.path().join("sales.sqlite"));
    let s = SqliteAdapter.connect(&ConnectParams::new(cfg.clone(), None)).await.expect("open");
    run_all(&*s, SEED).await;
    (dir, cfg, s)
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Sqlite) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, "main", None, name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn opens_files_and_memory_and_reports_errors() {
    let (dir, cfg, s) = seeded().await;
    assert!(s.info().version.starts_with("SQLite 3."), "{}", s.info().version);
    assert_eq!(s.info().database, "main");
    // In memory, the three connections share one database.
    let memory = SqliteAdapter
        .connect(&ConnectParams::new(ConnConfig { database: ":memory:".into(), ..cfg.clone() }, None))
        .await
        .unwrap();
    run_all(&*memory, "CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)").await;
    assert_eq!(
        memory.list_children(Some(&ObjectRef::new(NodeKind::Database, "main", None, "main"))).await.unwrap().len(),
        1
    );
    let missing = ConnConfig { database: "/no/such/folder/x.sqlite".into(), ..cfg.clone() };
    assert_eq!(SqliteAdapter.test(&ConnectParams::new(missing, None)).await.unwrap_err().kind, ErrorKind::Storage);
    let not_db = dir.path().join("notes.txt");
    std::fs::write(&not_db, "this is not a database file, just some text that is long enough").unwrap();
    let err = SqliteAdapter.test(&ConnectParams::new(config(&not_db), None)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Storage, "{err:?}");
    assert_eq!(s.execute("SELEC 1", &[], OPTS).await.unwrap_err().kind, ErrorKind::Syntax);
    assert_eq!(s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err().kind, ErrorKind::NotFound);
    let dup = s.execute("INSERT INTO customers (id, email) VALUES (1, 'x')", &[], OPTS).await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint);
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let (dir, _cfg, s) = seeded().await;
    let other = dir.path().join("billing.sqlite");
    run_all(
        &*s,
        &format!(
            "ATTACH '{}' AS billing; CREATE TABLE billing.invoices (id INTEGER PRIMARY KEY, amount REAL)",
            other.display()
        ),
    )
    .await;
    let dbs = s.list_children(None).await.unwrap();
    let names: Vec<_> = dbs.iter().map(|d| d.object.name.as_str()).collect();
    assert_eq!(names, ["main", "billing"]);
    assert!(dbs[0].current);
    let rels = s.list_children(Some(&dbs[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    let child = |n: &str| children.iter().find(|c| c.object.name == n).map(|c| c.object.clone());
    assert_eq!(child("orders_status_idx").map(|o| o.kind), Some(NodeKind::Index));
    let trigger = child("orders_audit").unwrap();
    assert_eq!(trigger.kind, NodeKind::Trigger);
    assert!(s.ddl(&trigger).await.unwrap().starts_with("CREATE TRIGGER orders_audit"));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));
    // Attached databases page like the main one, on the explorer's own connection.
    let invoices = ObjectRef::new(NodeKind::Table, "billing", None, "invoices");
    let page = ops::table_page(
        &*s,
        &TablePageRequest { object: invoices, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 },
    )
    .await
    .unwrap();
    assert_eq!(page.columns[0].name, "id");
    run_all(&*s, "DETACH billing").await;
    assert_eq!(s.list_children(None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn value_rendering_contract() {
    let (_dir, _cfg, s) = seeded().await;
    let r =
        s.execute("SELECT big, ratio, doc, raw, label, total FROM type_samples ORDER BY id", &[], OPTS).await.unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("1.5"));
    assert!(matches!(&row[2], Cell::Json { value, .. } if value.contains("Zoë ✓")));
    assert_eq!(row[3], Cell::Binary { length: 4, head_hex: "89504e47".into() });
    assert_eq!(row[4], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(row[5], Cell::number("3.0"));
    assert!(r.rows[1].iter().all(|c| *c == Cell::Null));
    assert_eq!(r.columns[0].type_name, "INTEGER");
    assert!(r.columns[0].numeric && !r.columns[4].numeric);
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let (_dir, _cfg, s) = seeded().await;
    let series = "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g) SELECT n FROM g";
    let r = s.execute(series, &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    let r = s.execute("SELECT 1", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(s.fetch_more(10).await.is_err());
    let r =
        s.execute("SELECT * FROM customers LIMIT 3", &[], ExecOptions { row_limit: 3, timeout_secs: 5 }).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (3, false));
    let slow = "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g) SELECT count(*) FROM g";
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    assert_eq!(handle.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_secs(5));
    let err = s.execute(slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn counts_transactions_triggers_and_parameters() {
    let (_dir, _cfg, s) = seeded().await;
    let r = s.execute("UPDATE orders SET notes = 'x' WHERE id <= 3", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("UPDATE", Some(3)));
    s.execute("BEGIN", &[], OPTS).await.unwrap();
    s.execute("DELETE FROM no_pk", &[], OPTS).await.unwrap();
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert_eq!(text(&s.execute("SELECT count(*) FROM no_pk", &[], OPTS).await.unwrap().rows[0][0]), "2");
    // The trigger body (with its `CASE … END`) ran as one statement.
    s.execute("UPDATE orders SET status = 'paid' WHERE id = 3", &[], OPTS).await.unwrap();
    assert_eq!(text(&s.execute("SELECT status FROM audit", &[], OPTS).await.unwrap().rows[0][0]), "PAID");
    let r = s
        .execute(
            "SELECT total, ?2 IS NULL, ?3 FROM orders WHERE id = ?1",
            &[Some("7".into()), None, Some("it's; DROP".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!((text(&r.rows[0][1]).as_str(), text(&r.rows[0][2]).as_str()), ("1", "it's; DROP"));
    let r = s.execute("INSERT INTO no_pk VALUES (2, 'y') RETURNING a", &[], OPTS).await.unwrap();
    assert_eq!((text(&r.rows[0][0]).as_str(), r.affected), ("2", Some(1)));
    // Foreign keys are enforced.
    let err = s.execute("INSERT INTO orders (customer_id, total) VALUES (999, 1)", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
}

#[tokio::test]
async fn structure_ddl_dependencies_and_model() {
    let (_dir, _cfg, s) = seeded().await;
    let st = ops::structure(&*s, &table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    let id = &st.columns[0];
    assert!(id.primary_key && id.identity && !id.nullable);
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && !c.nullable && c.data_type == "NUMERIC(12,2)"));
    assert!(st.columns.iter().any(|c| c.name == "status" && c.default.as_deref() == Some("'pending'")));
    let fk = &st.foreign_keys[0];
    assert_eq!(
        (fk.target.name.as_str(), fk.target_columns.as_slice(), fk.on_delete.as_deref()),
        ("customers", &["id".to_owned()][..], Some("CASCADE"))
    );
    assert!(st.indexes.iter().any(|i| i.name == "orders_status_idx" && i.definition.starts_with("CREATE INDEX")));
    let customers = ops::structure(&*s, &table("customers")).await.unwrap();
    assert!(customers.constraints.iter().any(|c| c.kind == dq_core::schema::ConstraintKind::Unique));
    let samples = ops::structure(&*s, &table("type_samples")).await.unwrap();
    assert!(samples.column("total").is_some_and(|c| c.generated && !c.editable));
    let no_pk = ops::structure(&*s, &table("no_pk")).await.unwrap();
    assert!(no_pk.row_identity.is_empty() && no_pk.full_row_key == ["a", "b"]);
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(
        ddl.starts_with("CREATE TABLE orders")
            && ddl.contains("CREATE INDEX orders_status_idx")
            && ddl.contains("CREATE TRIGGER")
    );
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert!(s.ddl(&view).await.unwrap().starts_with("CREATE VIEW paid_orders"));
    assert_eq!(ops::structure(&*s, &view).await.unwrap().object.kind, NodeKind::View);
    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "customers"));
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders"));
    assert!(deps.used_by.iter().any(|d| d.object.name == "orders_audit"));
    let used = s.dependencies(&table("customers")).await.unwrap();
    assert!(used.used_by.iter().any(|d| d.object.name == "orders"));
    let model = s.schema_model("main", None).await.unwrap();
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    assert!(model.tables.iter().any(|t| t.object.name == "paid_orders" && t.object.kind == NodeKind::View));
    assert_eq!(s.structure(&table("missing")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn pages_filters_edits_and_transfers() -> DbResult<()> {
    let (dir, _cfg, s) = seeded().await;
    let req = TablePageRequest {
        object: table("orders"),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 100,
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
    let literal = RowFilter {
        searches: vec![ColumnSearch { column: "status".into(), contains: "_aid".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &literal).await?.count, 0);
    let cond = |column: &str, op, value: &str| FilterCondition { column: column.into(), op, value: value.into() };
    let built = RowFilter {
        conditions: vec![cond("id", FilterOp::Lte, "10"), cond("notes", FilterOp::IsNull, "")],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &built).await?.count, 2);
    let any = RowFilter {
        conditions: vec![cond("id", FilterOp::In, "1, 2, 3"), cond("status", FilterOp::StartsWith, "ref")],
        match_any: true,
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &any).await?.count, 402);

    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("1"))],
            values: vec![cv("total", Some("84.50")), cv("notes", Some("Zoë ✓"))],
        },
        RowChange::Insert { values: vec![cv("customer_id", Some("1")), cv("total", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &table("orders"), &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT total, notes, typeof(total) FROM orders WHERE id = 1", &[], OPTS).await?;
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("84.5", "Zoë ✓"));
    assert_eq!(text(&r.rows[0][2]), "real", "text parameters take the column's affinity");
    let failing = vec![RowChange::Delete { key: vec![cv("id", Some("404040"))] }];
    assert_eq!(
        ops::apply_changes(&*s, &table("orders"), &failing, false).await.unwrap_err().kind,
        ErrorKind::Constraint
    );

    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 9);
    run_all(&*s, "CREATE TABLE it_import (id INTEGER PRIMARY KEY, status TEXT, total NUMERIC(12,2), raw BLOB)").await;
    let csv_source = ImportSource::Csv {
        path: csv.to_string_lossy().into_owned(),
        options: CsvOptions { empty_as_null: true, ..CsvOptions::default() },
    };
    let map: Vec<ImportMapping> = [("id", 0), ("status", 2), ("total", 3)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    assert_eq!(
        ops::import(&*s, &table("it_import"), &csv_source, &map, &ImportOptions::default(), &none).await?.rows,
        9
    );
    s.execute("UPDATE it_import SET raw = X'0102FF' WHERE id = 1", &[], OPTS).await?;
    let sql_file = dir.path().join("rows.sql");
    ops::export(
        &*s,
        &ExportSource::Table { object: table("it_import"), rows: RowFilter::default(), sort: None },
        ExportFormat::SqlInsert,
        &ExportOptions::default(),
        &sql_file,
        &none,
    )
    .await?;
    s.execute("DELETE FROM it_import", &[], OPTS).await?;
    run_all(&*s, &std::fs::read_to_string(&sql_file).unwrap()).await;
    let r = s.execute("SELECT count(*), max(raw) FROM it_import", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "9");
    assert_eq!(r.rows[0][1], Cell::Binary { length: 3, head_hex: "0102ff".into() });
    Ok(())
}

#[tokio::test]
async fn read_only_files_refuse_writes() {
    let (_dir, cfg, s) = seeded().await;
    s.close().await;
    drop(s);
    let ro = SqliteAdapter.connect(&ConnectParams::new(ConnConfig { read_only: true, ..cfg }, None)).await.unwrap();
    assert!(ro.info().read_only);
    assert!(ro.execute("SELECT count(*) FROM orders", &[], OPTS).await.is_ok());
    assert_eq!(ro.execute("DELETE FROM orders", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
    let changes = vec![RowChange::Delete { key: vec![ColumnValue { column: "id".into(), value: Some("1".into()) }] }];
    assert_eq!(
        ops::apply_changes(&*ro, &table("orders"), &changes, false).await.unwrap_err().kind,
        ErrorKind::Permission
    );
}

#[tokio::test]
async fn explain_and_schema_changes() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, SchemaChange};

    let (dir, _cfg, s) = seeded().await;
    let plan = s.explain("SELECT * FROM orders WHERE status = ?1", &[Some("paid".into())], false, 30).await.unwrap();
    let root = plan.plan.unwrap();
    assert!(
        root.label == "SEARCH" && root.detail.as_deref().is_some_and(|d| d.contains("orders_status_idx")),
        "{}",
        plan.raw
    );
    assert_eq!(s.explain("SELECT 1", &[], true, 30).await.unwrap_err().kind, ErrorKind::Unsupported);

    let other = dir.path().join("b.sqlite");
    run_all(&*s, &format!("ATTACH '{}' AS b", other.display())).await;
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
            table: t("main", "accounts"),
            columns: vec![
                ColumnDef { nullable: false, ..col("id", "INTEGER") },
                ColumnDef { default: Some("'x'".into()), ..col("email", "TEXT") },
            ],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddColumn { table: t("main", "accounts"), column: col("note", "TEXT") },
        SchemaChange::RenameColumn { table: t("main", "accounts"), column: "note".into(), new_name: "remark".into() },
        SchemaChange::CreateIndex {
            table: t("main", "accounts"),
            name: "accounts_email".into(),
            columns: vec![ddl::IndexColumn { name: "email".into(), descending: false }],
            unique: true,
        },
        SchemaChange::RenameTable { table: t("main", "no_pk"), new_name: "loose".into() },
        SchemaChange::DropColumn { table: t("main", "loose"), column: "b".into() },
    ];
    for change in &changes {
        run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    let accounts = ops::structure(&*s, &t("main", "accounts")).await.unwrap();
    assert!(
        accounts.column("remark").is_some() && accounts.indexes.iter().any(|i| i.name == "accounts_email" && i.unique)
    );
    let snapshot = |db: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model(db, None).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables.into_iter().filter(|t| t.object.kind == NodeKind::Table) {
                tables.push(ops::structure(&*s, &table.object).await.unwrap());
            }
            tables
        }
    };
    let main = Container { database: "main".into(), schema: None };
    let b = Container { database: "b".into(), schema: None };
    let plan = diff(s.dialect(), &snapshot("main").await, &main, &snapshot("b").await, &b).unwrap();
    run_all(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot("main").await, &main, &snapshot("b").await, &b).unwrap();
    assert!(again.sql.is_empty(), "{:#?}\n{}", again.items, again.sql);
    // Foreign keys can only be declared when a table is created, so the diff says so.
    assert!(plan.warnings.iter().any(|w| w.starts_with("orders: SQLite only adds")), "{:?}", plan.warnings);
    // A change SQLite cannot make is reported, not applied.
    run_all(&*s, "CREATE TABLE b.widen (id INTEGER PRIMARY KEY, v INTEGER); CREATE TABLE main.widen (id INTEGER PRIMARY KEY, v TEXT)").await;
    let plan = diff(s.dialect(), &snapshot("main").await, &main, &snapshot("b").await, &b).unwrap();
    assert!(plan.warnings.iter().any(|w| w.starts_with("widen: SQLite cannot change")), "{:?}", plan.warnings);
}
