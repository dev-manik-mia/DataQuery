//! Integration tests against a real libSQL server (`sqld`, what Turso runs).
//!
//! Start it with `docker compose -p dataquery --profile libsql up -d libsql`
//! and run `DATAQUERY_TEST_LIBSQL=http://localhost:58080 cargo test -p dq-sqlite-http`,
//! with `DATAQUERY_TEST_LIBSQL_TOKEN` set to a token the server accepts (see
//! `npm run test:integration:libsql`).
//! Without the variable the suite is skipped. The server has one database,
//! so the scenarios run one after another on a freshly seeded copy; they
//! follow the local `SQLite` suite in `dq-sqlite/tests/sqlite.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{BoundStatement, ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::sql::ddl::{self, ColumnDef, SchemaChange};
use dq_core::sql::{SqlFlavor, split_statements};
use dq_core::table::{ColumnSearch, FilterCondition, FilterOp, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_sqlite_http::LibsqlAdapter;

const SEED: &str = include_str!("../../dq-sqlite/tests/seed.sql");
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_LIBSQL").ok()?;
    let (tls, rest) = match target.split_once("://") {
        Some((scheme, rest)) => (scheme == "https", rest),
        None => (false, target.as_str()),
    };
    let (host, port) = rest.trim_end_matches('/').split_once(':').unwrap_or((rest, if tls { "443" } else { "8080" }));
    Some(ConnConfig {
        engine: Engine::Libsql,
        host: host.into(),
        port: port.parse().unwrap(),
        ssl_mode: if tls { SslMode::VerifyFull } else { SslMode::Disable },
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

/// The server's auth token (a JWT), when it requires one.
fn token() -> Option<String> {
    std::env::var("DATAQUERY_TEST_LIBSQL_TOKEN").ok().filter(|t| !t.is_empty())
}

async fn connect(cfg: &ConnConfig) -> Arc<dyn Session> {
    LibsqlAdapter.connect(&ConnectParams::new(cfg.clone(), token())).await.expect("connect")
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

async fn scalar(s: &dyn Session, sql: &str) -> String {
    text(&s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}")).rows[0][0])
}

/// Drops every user object, then loads the shared seed.
async fn reseed(s: &dyn Session) {
    s.execute("PRAGMA foreign_keys = OFF", &[], OPTS).await.unwrap();
    let found = s
        .execute(
            "SELECT type, name FROM sqlite_master WHERE type IN ('view', 'table') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND name NOT LIKE 'libsql\\_%' ESCAPE '\\' ORDER BY type DESC",
            &[],
            OPTS,
        )
        .await
        .unwrap();
    for row in &found.rows {
        let drop =
            format!("DROP {} IF EXISTS \"{}\"", text(&row[0]).to_uppercase(), text(&row[1]).replace('"', "\"\""));
        s.execute(&drop, &[], OPTS).await.unwrap();
    }
    s.execute("PRAGMA foreign_keys = ON", &[], OPTS).await.unwrap();
    run_all(s, SEED).await;
}

#[tokio::test]
async fn reports_unreachable_servers() {
    let cfg = ConnConfig {
        engine: Engine::Libsql,
        host: "127.0.0.1".into(),
        port: 9,
        ssl_mode: SslMode::Disable,
        ..ConnConfig::default()
    };
    let err = LibsqlAdapter.test(&ConnectParams::new(cfg, Some("token-never-shown".into()))).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Network, "{err:?}");
    assert!(!err.message.contains("token-never-shown"));
}

#[tokio::test]
async fn libsql_suite() -> DbResult<()> {
    let Some(cfg) = config() else {
        eprintln!("skipped: set DATAQUERY_TEST_LIBSQL (e.g. http://localhost:58080) to run libSQL integration tests");
        return Ok(());
    };
    if let Some(token) = token() {
        let wrong = format!("{token}x");
        let err = LibsqlAdapter.test(&ConnectParams::new(cfg.clone(), Some(wrong.clone()))).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
        assert!(!err.message.contains(&wrong));
        let err = LibsqlAdapter.test(&ConnectParams::new(cfg.clone(), None)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    }
    let s = connect(&cfg).await;
    assert!(s.info().version.starts_with("libSQL (SQLite 3."), "{}", s.info().version);
    reseed(&*s).await;
    errors_and_values(&*s).await;
    explorer_structure_and_model(&*s).await;
    bounded_cancellable_results(&s).await;
    transactions_on_the_stream(&*s).await;
    pages_edits_and_transfers(&*s).await?;
    schema_changes_and_table_actions(&*s).await?;
    read_only(&cfg).await;
    s.close().await;
    Ok(())
}

async fn errors_and_values(s: &dyn Session) {
    assert_eq!(s.execute("SELEC 1", &[], OPTS).await.unwrap_err().kind, ErrorKind::Syntax);
    assert_eq!(s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err().kind, ErrorKind::NotFound);
    let dup = s.execute("INSERT INTO customers (id, email) VALUES (1, 'x')", &[], OPTS).await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint, "{dup:?}");
    // Foreign keys are enforced, as on a local file.
    let err = s.execute("INSERT INTO orders (customer_id, total) VALUES (999, 1)", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint, "{err:?}");

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

    let r = s
        .execute(
            "SELECT total, ?2 IS NULL, ?3 FROM orders WHERE id = ?1",
            &[Some("7".into()), None, Some("it's; DROP".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!((text(&r.rows[0][1]).as_str(), text(&r.rows[0][2]).as_str()), ("1", "it's; DROP"));
    let r = s.execute("UPDATE orders SET notes = 'x' WHERE id <= 3", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("UPDATE", Some(3)));
    s.execute("UPDATE orders SET status = 'paid' WHERE id = 3", &[], OPTS).await.unwrap();
    assert_eq!(scalar(s, "SELECT status FROM audit").await, "PAID", "the trigger body ran as one statement");
}

async fn explorer_structure_and_model(s: &dyn Session) {
    let dbs = s.list_children(None).await.unwrap();
    assert_eq!(dbs.iter().map(|d| d.object.name.as_str()).collect::<Vec<_>>(), ["main"]);
    let rels = s.list_children(Some(&dbs[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!((kind("orders"), kind("paid_orders")), (Some(NodeKind::Table), Some(NodeKind::View)));
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    let trigger = children.iter().find(|c| c.object.name == "orders_audit").unwrap();
    assert!(s.ddl(&trigger.object).await.unwrap().starts_with("CREATE TRIGGER orders_audit"));
    assert!(children.iter().any(|c| c.object.name == "orders_status_idx" && c.object.kind == NodeKind::Index));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));

    let st = ops::structure(s, &table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert!(st.columns[0].primary_key && st.columns[0].identity);
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && c.data_type == "NUMERIC(12,2)"));
    let fk = &st.foreign_keys[0];
    assert_eq!((fk.target.name.as_str(), fk.on_delete.as_deref()), ("customers", Some("CASCADE")));
    assert!(st.indexes.iter().any(|i| i.name == "orders_status_idx" && i.definition.starts_with("CREATE INDEX")));
    let customers = ops::structure(s, &table("customers")).await.unwrap();
    assert!(customers.constraints.iter().any(|c| c.kind == dq_core::schema::ConstraintKind::Unique));
    assert!(ops::structure(s, &table("type_samples")).await.unwrap().column("total").is_some_and(|c| c.generated));
    let no_pk = ops::structure(s, &table("no_pk")).await.unwrap();
    assert!(no_pk.row_identity.is_empty() && no_pk.full_row_key == ["a", "b"]);
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE orders") && ddl.contains("CREATE INDEX orders_status_idx"));
    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "customers"));
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders"));
    let model = s.schema_model("main", None).await.unwrap();
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    assert_eq!(s.structure(&table("missing")).await.unwrap_err().kind, ErrorKind::NotFound);
    let plan = s.explain("SELECT * FROM orders WHERE status = ?1", &[Some("paid".into())], false, 30).await.unwrap();
    assert!(plan.raw.contains("orders_status_idx"), "{}", plan.raw);
    assert_eq!(s.explain("SELECT 1", &[], true, 30).await.unwrap_err().kind, ErrorKind::Unsupported);
}

async fn bounded_cancellable_results(s: &Arc<dyn Session>) {
    let r = s.execute("SELECT id FROM orders ORDER BY id", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!((text(&page.rows[0][0]).as_str(), page.has_more), ("501", true));
    assert_eq!(scalar(&**s, "SELECT 1").await, "1");
    assert!(s.fetch_more(10).await.is_err());

    let slow =
        "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 30000000) SELECT count(*) FROM g";
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    assert_eq!(handle.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    let err = s.execute(slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert_eq!(scalar(&**s, "SELECT 2").await, "2", "a new stream replaces the abandoned one");
}

async fn transactions_on_the_stream(s: &dyn Session) {
    s.execute("BEGIN", &[], OPTS).await.unwrap();
    s.execute("DELETE FROM no_pk", &[], OPTS).await.unwrap();
    assert_eq!(scalar(s, "SELECT count(*) FROM no_pk").await, "0");
    // Idle longer than the server keeps a stream: the keepalive holds the transaction.
    tokio::time::sleep(Duration::from_secs(12)).await;
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert_eq!(scalar(s, "SELECT count(*) FROM no_pk").await, "2");
    // sqld has no temporary tables: it says so.
    assert_eq!(s.execute("CREATE TEMP TABLE scratch (v)", &[], OPTS).await.unwrap_err().kind, ErrorKind::Syntax);
}

async fn pages_edits_and_transfers(s: &dyn Session) -> DbResult<()> {
    let req = TablePageRequest {
        object: table("orders"),
        rows: RowFilter { filter: Some("status = 'refunded'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 100,
        limit: 100,
    };
    let page = ops::table_page(s, &req).await?;
    assert_eq!((page.rows.len(), page.has_more), (100, true));
    assert_eq!(ops::row_count(s, &req.object, &req.rows).await?.count, 400);
    let search = RowFilter {
        searches: vec![ColumnSearch { column: "status".into(), contains: "REFUN".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(s, &req.object, &search).await?.count, 400);
    let cond = |column: &str, op, value: &str| FilterCondition { column: column.into(), op, value: value.into() };
    let built = RowFilter {
        conditions: vec![cond("id", FilterOp::Lte, "10"), cond("notes", FilterOp::IsNull, "")],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(s, &req.object, &built).await?.count, 2);

    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("1"))],
            values: vec![cv("total", Some("84.50")), cv("notes", Some("Zoë ✓"))],
        },
        RowChange::Insert { values: vec![cv("customer_id", Some("1")), cv("total", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(s, &table("orders"), &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT total, notes, typeof(total) FROM orders WHERE id = 1", &[], OPTS).await?;
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("84.5", "Zoë ✓"));
    assert_eq!(text(&r.rows[0][2]), "real", "text parameters take the column's affinity");
    // An unmet expectation rolls back the whole edit, the update before it too.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("4"))], values: vec![cv("notes", Some("lost"))] },
        RowChange::Delete { key: vec![cv("id", Some("404040"))] },
    ];
    assert_eq!(ops::apply_changes(s, &table("orders"), &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);
    assert_ne!(scalar(s, "SELECT coalesce(notes, '') FROM orders WHERE id = 4").await, "lost");

    let none = TransferControl::none();
    let dir = std::env::temp_dir().join(format!("dq-libsql-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    assert_eq!(ops::export(s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 9);
    run_all(s, "CREATE TABLE it_import (id INTEGER PRIMARY KEY, status TEXT, total NUMERIC(12,2), raw BLOB)").await;
    let csv_source = ImportSource::Csv {
        path: csv.to_string_lossy().into_owned(),
        options: CsvOptions { empty_as_null: true, ..CsvOptions::default() },
    };
    let map: Vec<ImportMapping> = [("id", 0), ("status", 2), ("total", 3)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    let imported = ops::import(s, &table("it_import"), &csv_source, &map, &ImportOptions::default(), &none).await?;
    assert_eq!(imported.rows, 9);
    s.execute("UPDATE it_import SET raw = X'0102FF' WHERE id = 1", &[], OPTS).await?;
    let sql_file = dir.join("rows.sql");
    let all = ExportSource::Table { object: table("it_import"), rows: RowFilter::default(), sort: None };
    ops::export(s, &all, ExportFormat::SqlInsert, &ExportOptions::default(), &sql_file, &none).await?;
    s.execute("DELETE FROM it_import", &[], OPTS).await?;
    run_all(s, &std::fs::read_to_string(&sql_file).unwrap()).await;
    let r = s.execute("SELECT count(*), max(raw) FROM it_import", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "9");
    assert_eq!(r.rows[0][1], Cell::Binary { length: 3, head_hex: "0102ff".into() });
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

async fn schema_changes_and_table_actions(s: &dyn Session) -> DbResult<()> {
    let col = |name: &str, ty: &str| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        default: None,
        identity: false,
    };
    let changes = vec![
        SchemaChange::CreateTable {
            table: table("accounts"),
            columns: vec![ColumnDef { nullable: false, ..col("id", "INTEGER") }, col("email", "TEXT")],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddColumn { table: table("accounts"), column: col("note", "TEXT") },
        SchemaChange::RenameColumn { table: table("accounts"), column: "note".into(), new_name: "remark".into() },
        SchemaChange::CreateIndex {
            table: table("accounts"),
            name: "accounts_email".into(),
            columns: vec![ddl::IndexColumn { name: "email".into(), descending: false }],
            unique: true,
        },
    ];
    for change in &changes {
        run_all(s, &ddl::preview(&ddl::statements(s.dialect(), change, None)?)).await;
    }
    let accounts = ops::structure(s, &table("accounts")).await?;
    assert!(
        accounts.column("remark").is_some() && accounts.indexes.iter().any(|i| i.name == "accounts_email" && i.unique)
    );

    let copy = SchemaChange::DuplicateTable { table: table("orders"), new_name: "ta_orders".into(), with_data: true };
    let sql = ops::schema_statements(s, &[copy]).await?;
    let mut bound = sql.into_iter().map(|q| Ok(BoundStatement { sql: q, params: Vec::new(), expect_rows: None }));
    s.run_in_transaction("main", &mut bound).await?;
    assert_eq!(scalar(s, "SELECT count(*) FROM ta_orders").await, "1200");
    Ok(())
}

async fn read_only(cfg: &ConnConfig) {
    let ro = connect(&ConnConfig { read_only: true, ..cfg.clone() }).await;
    assert!(ro.info().read_only);
    assert!(ro.execute("SELECT count(*) FROM orders", &[], OPTS).await.is_ok());
    assert_eq!(ro.execute("DELETE FROM orders", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
    let changes = vec![RowChange::Delete { key: vec![ColumnValue { column: "id".into(), value: Some("1".into()) }] }];
    assert_eq!(
        ops::apply_changes(&*ro, &table("orders"), &changes, false).await.unwrap_err().kind,
        ErrorKind::Permission
    );
    ro.close().await;
}
