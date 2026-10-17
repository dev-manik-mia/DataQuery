//! Integration tests against a real SQL Server.
//!
//! Start it with `docker compose -p dataquery up -d sqlserver` and run
//! `DATAQUERY_TEST_SQLSERVER=localhost:51433 cargo test -p dq-sqlserver`.
//! Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{BoundStatement, ColumnValue, RowChange};
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
use dq_sqlserver::SqlServerAdapter;

const PASSWORD: &str = "dataquery";
const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const ADAPTER: SqlServerAdapter = SqlServerAdapter;

const MILLION_ROWS: &str = "SELECT TOP 1000000 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS g \
     FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c";

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_SQLSERVER").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "1433"));
    Some(ConnConfig {
        engine: Engine::Sqlserver,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 10,
        ..ConnConfig::default()
    })
}

fn params(cfg: &ConnConfig, password: &str) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some(password.to_owned()))
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_SQLSERVER to run SQL Server integration tests");
                return;
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ADAPTER.connect(&params(cfg, PASSWORD)).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, Some("dbo".into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Sqlserver) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// Creates a scratch table unique to the calling test.
async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    run_all(s, &format!("DROP TABLE IF EXISTS dbo.{name};\nCREATE TABLE dbo.{name} {ddl}")).await;
    table(name)
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("SQL Server 2022 16."), "{}", info.version);
    assert_eq!((info.database.as_str(), info.user.as_str()), (DB, "dataquery"));
    assert_eq!(info.engine, Engine::Sqlserver);
}

#[tokio::test]
async fn invalid_credentials_are_actionable_and_redacted() {
    let cfg = require_db!();
    let secret = "wrong-Secret-123";
    let err = ADAPTER.test(&params(&cfg, secret)).await.unwrap_err();
    assert_eq!((err.kind, err.code.as_deref()), (ErrorKind::Auth, Some("18456")));
    assert!(err.action.as_deref().unwrap_or_default().contains("username and password"));
    let json = serde_json::to_string(&err).unwrap_or_default();
    assert!(!format!("{err:?}{json}").contains(secret));
}

#[tokio::test]
async fn network_failure_is_classified() {
    let mut cfg = require_db!();
    cfg.port = 1;
    let err = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn tls_modes() {
    let mut cfg = require_db!();
    // The container's certificate is self-signed: full verification must refuse it.
    cfg.ssl_mode = SslMode::VerifyFull;
    let err = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
    cfg.ssl_mode = SslMode::Require;
    let info = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap();
    let tls = info.tls.expect("TLS in use");
    assert!(!tls.verified);
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    assert!(dbs.iter().any(|d| d.object.name == "billing"));
    assert!(!dbs.iter().any(|d| d.object.name == "master"), "system databases are hidden");
    let current = dbs.iter().find(|d| d.current).unwrap();
    let schemas = s.list_children(Some(&current.object)).await.unwrap();
    let names: Vec<_> = schemas.iter().map(|n| n.object.name.as_str()).collect();
    assert_eq!(names[0], "dbo");
    assert!(names.contains(&"sales") && !names.contains(&"sys") && !names.contains(&"db_owner"), "{names:?}");
    let rels = s.list_children(Some(&schemas[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert_eq!(rels.iter().find(|r| r.object.name == "orders").unwrap().row_estimate, Some(1200));
    for group in ["sequences", "functions", "procedures"] {
        assert!(rels.iter().any(|r| r.object.kind == NodeKind::Group && r.object.name == group), "{group}");
    }
    // Another database lists through the same session.
    let billing = dbs.iter().find(|d| d.object.name == "billing").unwrap();
    let dbo = &s.list_children(Some(&billing.object)).await.unwrap()[0];
    assert!(s.list_children(Some(&dbo.object)).await.unwrap().iter().any(|t| t.object.name == "invoices"));
    let found = s.search("ORDER", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "orders" && n.object.schema.as_deref() == Some("dbo")));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT big, exact, ts, doc, raw, label, flag, uid, price, day, at, ratio, geo.ToString(), x, node, geo \
             FROM dbo.type_samples ORDER BY id",
            &[],
            OPTS,
        )
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[2], Cell::Temporal { value: "2026-10-14 09:59:00.123 +02:00".into(), zoned: true });
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[3])).unwrap()["name"], "Zoë ✓");
    assert_eq!(row[4], Cell::Binary { length: 8, head_hex: "89504e470d0a1a0a".into() });
    assert_eq!(row[5], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(row[6], Cell::Bool { value: true });
    assert_eq!(text(&row[7]), "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11");
    assert_eq!(row[8], Cell::number("12.3456"));
    assert_eq!(row[9], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(row[10], Cell::Temporal { value: "09:59:00.123".into(), zoned: false });
    assert_eq!(row[11], Cell::number("1.5"));
    assert_eq!(text(&row[12]), "POINT (2.35 48.85)");
    assert_eq!(text(&row[13]), "<a>1</a>");
    // CLR values without a cast arrive as their serialized bytes instead of failing.
    assert!(matches!(row[15], Cell::Binary { .. }), "{:?}", row[15]);
    let second = &r.rows[1];
    assert_eq!(second[1], Cell::number("-0.0000000001"));
    assert!(second[2..].iter().all(|c| *c == Cell::Null));
    assert!(r.columns[0].numeric && !r.columns[5].numeric);
    assert_eq!((r.columns[0].type_name.as_str(), r.columns[1].type_name.as_str()), ("bigint", "decimal"));
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
    assert_eq!(text(&page.rows[0][0]), "501");
    // A new statement stops the open result instead of reading it to the end.
    let started = Instant::now();
    let r = s.execute("SELECT 1", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(s.fetch_more(10).await.is_err());
    // Exactly `limit` rows: no read-ahead row is lost and `has_more` is right.
    let r = s
        .execute("SELECT TOP 3 name FROM sys.objects", &[], ExecOptions { row_limit: 3, timeout_secs: 5 })
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.has_more), (3, false));
}

#[tokio::test]
async fn cancel_and_timeout() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute("WAITFOR DELAY '00:00:30'; SELECT 1", &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!s.cancel().await.unwrap(), "nothing left to cancel");
    let err =
        s.execute("WAITFOR DELAY '00:00:30'", &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    // The session keeps working, on the same connection.
    let r = s.execute("SELECT @@SPID", &[], OPTS).await.unwrap();
    assert!(!r.rows.is_empty(), "no rows");
}

#[tokio::test]
async fn session_state_and_transactions_persist() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(&*s, "CREATE TABLE #scratch (id INT);\nINSERT INTO #scratch VALUES (1), (2)").await;
    let r = s.execute("SELECT COUNT(*) FROM #scratch", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "2");
    s.execute("BEGIN TRANSACTION", &[], OPTS).await.unwrap();
    s.execute("DELETE FROM #scratch", &[], OPTS).await.unwrap();
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    let r = s.execute("SELECT COUNT(*), @@TRANCOUNT FROM #scratch", &[], OPTS).await.unwrap();
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("2", "0"));
    s.execute("USE billing", &[], OPTS).await.unwrap();
    let r = s.execute("SELECT DB_NAME()", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "billing");
}

#[tokio::test]
async fn errors_counts_and_batches() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT id, totl FROM dbo.orders", &[], OPTS).await.unwrap_err();
    assert_eq!((err.code.as_deref(), err.kind), (Some("207"), ErrorKind::Syntax));
    let err = s.execute("SELECT * FROM dbo.missing_table", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
    scratch(&*s, "it_affected", "(id INT PRIMARY KEY)").await;
    run_all(&*s, "CREATE TABLE dbo.it_affected_log (n INT)\nGO\nCREATE OR ALTER TRIGGER dbo.it_affected_tr ON dbo.it_affected AFTER INSERT AS INSERT INTO dbo.it_affected_log VALUES (1), (2), (3), (4), (5)\nGO").await;
    let r = s.execute("INSERT INTO dbo.it_affected VALUES (1), (2), (3)", &[], OPTS).await.unwrap();
    // The trigger's own five rows do not count.
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    let r = s.execute("DELETE FROM dbo.it_affected OUTPUT deleted.id WHERE id > 1", &[], OPTS).await.unwrap();
    assert_eq!(r.rows.len(), 2);
    let r = s.execute("SELECT 1 AS a; SELECT 2 AS b", &[], OPTS).await.unwrap();
    assert_eq!(r.columns[0].name, "a");
    assert!(!r.notices.is_empty(), "no notices");
    run_all(&*s, "DROP TABLE dbo.it_affected;\nDROP TABLE dbo.it_affected_log").await;
}

#[tokio::test]
async fn structure_and_ddl() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    let id = &st.columns[0];
    assert!(id.primary_key && id.generated && id.identity && !id.editable);
    let status = st.column("status").unwrap();
    assert_eq!((status.data_type.as_str(), status.default.as_deref()), ("nvarchar(20)", Some("'pending'")));
    assert!(st.columns.iter().any(|c| c.name == "total" && c.numeric && !c.nullable && c.data_type == "decimal(12,2)"));
    let fk = st.constraints.iter().find(|c| c.name == "orders_customer_fk").expect("fk");
    assert_eq!(fk.definition, "FOREIGN KEY (customer_id) REFERENCES customers (id)");
    assert!(st.constraints.iter().any(|c| c.name == "orders_status_check"));
    assert!(
        st.indexes
            .iter()
            .any(|i| i.name == "orders_created_at_idx" && !i.unique && i.definition.contains("created_at DESC"))
    );
    assert!(st.indexes.iter().any(|i| i.primary));
    assert!(st.size.is_some());
    assert!(
        s.structure(&table("no_pk")).await.unwrap().row_identity.is_empty(),
        "a table without a key has no row identity"
    );
    let samples = s.structure(&table("type_samples")).await.unwrap();
    let geo = samples.column("geo").unwrap();
    assert!(!geo.editable);
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE dbo.orders (\n  id bigint IDENTITY(1,1) NOT NULL"), "{ddl}");
    assert!(ddl.contains("CREATE NONCLUSTERED INDEX orders_created_at_idx"), "{ddl}");
    // The generated DDL runs: recreate the table under another name.
    let copy = ddl
        .replace("dbo.orders", "dbo.it_orders_copy")
        .replace("CONSTRAINT orders_", "CONSTRAINT it_copy_")
        .replace("INDEX orders_", "INDEX it_copy_");
    run_all(&*s, &format!("DROP TABLE IF EXISTS dbo.it_orders_copy;\n{copy}\nDROP TABLE dbo.it_orders_copy")).await;
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert_eq!(s.structure(&view).await.unwrap().object.kind, NodeKind::View);
    assert!(s.ddl(&view).await.unwrap().starts_with("CREATE VIEW dbo.paid_orders"));
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
    let next = ops::table_page(&*s, &TablePageRequest { offset: 390, ..req.clone() }).await.unwrap();
    assert_eq!((next.rows.len(), next.has_more), (10, false));
    let count = ops::row_count(&*s, &req.object, &req.rows).await.unwrap();
    assert_eq!((count.count, count.exact), (400, true));
    let bad = TablePageRequest {
        rows: RowFilter { filter: Some("1=1; DELETE FROM dbo.orders".into()), ..RowFilter::default() },
        ..req.clone()
    };
    assert!(ops::table_page(&*s, &bad).await.is_err());
    // A table without a key pages without ORDER BY, and CLR columns read as text.
    let samples = ops::table_page(
        &*s,
        &TablePageRequest {
            object: table("type_samples"),
            rows: RowFilter::default(),
            sort: None,
            offset: 0,
            limit: 10,
        },
    )
    .await
    .unwrap();
    let geo = samples.columns.iter().position(|c| c.name == "geo").unwrap();
    assert_eq!(text(&samples.rows[0][geo]), "POINT (2.35 48.85)");
    let other = ObjectRef::new(NodeKind::Table, "billing", Some("dbo".into()), "invoices");
    let page = ops::table_page(
        &*s,
        &TablePageRequest { object: other, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 },
    )
    .await
    .unwrap();
    assert_eq!(page.columns[0].name, "id");
}

#[tokio::test]
async fn edits_apply_atomically_to_the_right_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t =
        scratch(&*s, "it_edits", "(id BIGINT PRIMARY KEY, amount DECIMAL(12,2), note NVARCHAR(MAX), at DATETIME2(3))")
            .await;
    s.execute(
        "INSERT INTO dbo.it_edits VALUES (9223372036854775807, 1.00, N'a', '2026-01-02 03:04:05.678'), (2, 2.00, N'b', NULL)",
        &[],
        OPTS,
    )
    .await
    .unwrap();
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("9223372036854775807"))],
            values: vec![
                cv("amount", Some("84.50")),
                cv("note", Some("Zoë ✓")),
                cv("at", Some("2026-10-14 09:59:00.123")),
            ],
        },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("amount", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let preview = ops::preview_changes(&*s, &t, &changes, false).await.unwrap();
    assert!(preview.contains("UPDATE [dbo].[it_edits] SET [amount] = N'84.50'"), "{preview}");
    let report = ops::apply_changes(&*s, &t, &changes, false).await.unwrap();
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT id, amount, note, at FROM dbo.it_edits ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(
        rows,
        [vec!["3", "NULL", "NULL", "NULL"], vec!["9223372036854775807", "84.50", "Zoë ✓", "2026-10-14 09:59:00.123"]]
    );
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("note", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    let err = ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
    let r = s.execute("SELECT note FROM dbo.it_edits WHERE id = 3", &[], OPTS).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Null);
    s.execute("DROP TABLE dbo.it_edits", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn export_and_import_round_trip() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 10);
    let json = dir.path().join("q.json");
    let q = ExportSource::Query { sql: "SELECT big, exact FROM dbo.type_samples ORDER BY id".into() };
    ops::export(&*s, &q, ExportFormat::Json, &ExportOptions::default(), &json, &none).await?;
    let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(parsed[0]["big"], "9223372036854775807");

    let t = scratch(
        &*s,
        "it_import",
        "(id BIGINT PRIMARY KEY, status NVARCHAR(20), total DECIMAL(12,2), raw VARBINARY(8))",
    )
    .await;
    let csv_source = |path: &std::path::Path| ImportSource::Csv {
        path: path.to_string_lossy().into_owned(),
        options: CsvOptions { empty_as_null: true, ..CsvOptions::default() },
    };
    let map: Vec<ImportMapping> = [("id", 0), ("status", 2), ("total", 3)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    let report = ops::import(&*s, &t, &csv_source(&csv), &map, &ImportOptions::default(), &none).await?;
    assert_eq!((report.rows, report.error_count), (10, 0));
    // Batches stay under SQL Server's 2,100 parameters per statement.
    let many = dir.path().join("many.csv");
    let body: String = std::iter::once("id,x,status,total\n".to_owned())
        .chain((1000..3500).map(|i| format!("{i},,s,{i}.25\n")))
        .collect();
    std::fs::write(&many, body).unwrap();
    let big_batches = ImportOptions { batch_rows: 5_000, ..ImportOptions::default() };
    assert_eq!(ops::import(&*s, &t, &csv_source(&many), &map, &big_batches, &none).await?.rows, 2500);
    // SQL INSERT export runs back into the table it came from.
    s.execute("UPDATE dbo.it_import SET raw = 0x0102FF WHERE id = 1", &[], OPTS).await?;
    let sql_file = dir.path().join("rows.sql");
    let first_rows = ExportSource::Table {
        object: t.clone(),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    ops::export(&*s, &first_rows, ExportFormat::SqlInsert, &ExportOptions::default(), &sql_file, &none).await?;
    s.execute("DELETE FROM dbo.it_import WHERE id <= 10", &[], OPTS).await?;
    let script = std::fs::read_to_string(&sql_file).unwrap();
    run_all(&*s, &script).await;
    let r = s.execute("SELECT COUNT(*), MAX(raw) FROM dbo.it_import WHERE id <= 10", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "10");
    assert_eq!(r.rows[0][1], Cell::Binary { length: 3, head_hex: "0102ff".into() });
    s.execute("DROP TABLE dbo.it_import", &[], OPTS).await?;
    Ok(())
}

#[tokio::test]
async fn parameters_are_bound() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT big, CASE WHEN @P2 IS NULL THEN 1 ELSE 0 END, @P3 FROM dbo.type_samples WHERE id = CAST(@P1 AS int)",
            &[Some("1".into()), None, Some("it's; DROP TABLE x".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!(text(&r.rows[0][0]), "9223372036854775807");
    assert_eq!(text(&r.rows[0][1]), "1");
    assert_eq!(text(&r.rows[0][2]), "it's; DROP TABLE x");
    let big = s
        .execute(
            "SELECT id FROM dbo.orders WHERE id > CAST(@P1 AS int)",
            &[Some("0".into())],
            ExecOptions { row_limit: 10, timeout_secs: 30 },
        )
        .await
        .unwrap();
    assert!(big.has_more);
    assert_eq!(s.fetch_more(10).await.unwrap().rows.len(), 10);
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    let err = s.execute("INSERT INTO dbo.no_pk VALUES (99, 'ro')", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    let changes = vec![RowChange::Delete { key: vec![ColumnValue { column: "id".into(), value: Some("1".into()) }] }];
    assert_eq!(
        ops::apply_changes(&*s, &table("orders"), &changes, false).await.unwrap_err().kind,
        ErrorKind::Permission
    );
}

#[tokio::test]
async fn row_filters_bind_matches_and_searches() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let object = table("orders");
    let rows = RowFilter {
        matches: vec![ColumnValue { column: "status".into(), value: Some("paid".into()) }],
        searches: vec![ColumnSearch { column: "status".into(), contains: "PAI".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &object, &rows).await.unwrap().count, 400);
    let id = RowFilter {
        matches: vec![ColumnValue { column: "id".into(), value: Some("7".into()) }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &object, &id).await.unwrap().count, 1);
    let none = RowFilter {
        searches: vec![ColumnSearch { column: "status".into(), contains: "_aid".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &object, &none).await.unwrap().count, 0, "`_` is a literal");
}

#[tokio::test]
async fn explorer_lists_routines_indexes_and_triggers() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let schema = ObjectRef::new(NodeKind::Schema, DB, Some("dbo".into()), "dbo");
    let nodes = s.list_children(Some(&schema)).await.unwrap();
    for (group, name) in [("functions", "order_total"), ("procedures", "order_count"), ("sequences", "invoice_seq")] {
        let node = nodes.iter().find(|n| n.object.kind == NodeKind::Group && n.object.name == group).unwrap();
        let routines = s.list_children(Some(&node.object)).await.unwrap();
        let routine = routines.iter().find(|n| n.object.name == name).unwrap_or_else(|| panic!("{name}"));
        assert!(s.ddl(&routine.object).await.unwrap().starts_with("CREATE"), "{name}");
    }
    let orders = nodes.iter().find(|n| n.object.name == "orders").unwrap();
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    let names: Vec<_> = children.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect();
    assert!(
        names.contains(&(NodeKind::Index, "orders_pkey"))
            && names.contains(&(NodeKind::Index, "orders_created_at_idx"))
    );
    assert!(names.contains(&(NodeKind::Trigger, "orders_touch")), "{names:?}");
    for child in &children {
        let ddl = s.ddl(&child.object).await.unwrap();
        assert!(ddl.contains("orders"), "{ddl}");
    }
    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.kind == NodeKind::Trigger));
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders"));
    assert!(deps.uses.iter().any(|d| d.object.name == "customers"));
    let model = s.schema_model(DB, Some("dbo")).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "orders" && t.columns.iter().any(|c| c.primary_key)));
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
}

#[tokio::test]
async fn explain_reads_showplan() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let sql = "SELECT * FROM dbo.orders WHERE status = 'paid' ORDER BY total";
    let plan = s.explain(sql, &[], false, 30).await.unwrap();
    let root = plan.plan.expect("plan tree");
    assert!(root.rows.is_some() && root.cost.is_some(), "{root:?}");
    let analyzed = s.explain(sql, &[], true, 30).await.unwrap();
    let root = analyzed.plan.unwrap();
    assert!(root.actual_rows.is_some(), "{root:?}");
    // The plan settings are off again afterwards.
    assert_eq!(s.execute("SELECT 1 AS one", &[], OPTS).await.unwrap().columns[0].name, "one");
}

#[tokio::test]
async fn monitor_shows_blocking_and_ends_sessions() {
    let cfg = require_db!();
    let holder = session(&cfg).await;
    let waiter = session(&cfg).await;
    let watcher = session(&cfg).await;
    scratch(&*holder, "it_monitor", "(id INT PRIMARY KEY, v INT)").await;
    holder.execute("INSERT INTO dbo.it_monitor VALUES (1, 0)", &[], OPTS).await.unwrap();
    let pid = |s: Arc<dyn Session>| async move {
        text(&s.execute("SELECT @@SPID", &[], OPTS).await.unwrap().rows[0][0]).parse::<i64>().unwrap()
    };
    let (holder_pid, waiter_pid) = (pid(holder.clone()).await, pid(waiter.clone()).await);
    holder.execute("BEGIN TRANSACTION", &[], OPTS).await.unwrap();
    holder.execute("UPDATE dbo.it_monitor SET v = 1 WHERE id = 1", &[], OPTS).await.unwrap();
    let blocked = tokio::spawn({
        let waiter = waiter.clone();
        async move { waiter.execute("UPDATE dbo.it_monitor SET v = 2 WHERE id = 1", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("sql server monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        let snapshot = monitor.snapshot(30).await.unwrap();
        if snapshot.processes.iter().any(|p| p.pid == waiter_pid && !p.blocked_by.is_empty())
            || Instant::now() > deadline
        {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let waiting = snapshot.processes.iter().find(|p| p.pid == waiter_pid).expect("waiter listed");
    assert_eq!(waiting.blocked_by, [holder_pid]);
    assert!(waiting.query.as_deref().unwrap_or_default().contains("UPDATE dbo.it_monitor"));
    assert!(snapshot.blocked >= Some(1));
    assert!(snapshot.locks.iter().any(|l| !l.granted));
    assert!(monitor.kill(waiter_pid, false).await.is_err(), "only whole sessions can be ended");
    assert!(monitor.kill(waiter_pid, true).await.unwrap());
    assert!(blocked.await.unwrap().is_err());
    holder.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert!(
        monitor.server_settings(Some("max server")).await.unwrap().iter().any(|s| s.name == "max server memory (MB)")
    );
    assert!(monitor.largest_tables(DB, 5).await.unwrap().iter().any(|t| t.object.name == "orders"));
    holder.execute("DROP TABLE dbo.it_monitor", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn schema_changes_and_diff() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(&*s, "DROP TABLE IF EXISTS it_diff_a.invoices; DROP TABLE IF EXISTS it_diff_a.accounts; DROP TABLE IF EXISTS it_diff_b.invoices; DROP TABLE IF EXISTS it_diff_b.accounts;\nDROP SCHEMA IF EXISTS it_diff_a\nGO\nDROP SCHEMA IF EXISTS it_diff_b\nGO\nCREATE SCHEMA it_diff_a\nGO\nCREATE SCHEMA it_diff_b\nGO").await;
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
                ColumnDef { default: Some("'x'".into()), ..col("email", "nvarchar(100)") },
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
        SchemaChange::AddForeignKey {
            table: t("it_diff_a", "invoices"),
            name: "invoices_account_fk".into(),
            columns: vec!["account_id".into()],
            target: t("it_diff_a", "accounts"),
            target_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        },
        SchemaChange::RenameColumn {
            table: t("it_diff_a", "invoices"),
            column: "amount".into(),
            new_name: "total".into(),
        },
    ];
    for change in &changes {
        run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    // ALTER COLUMN restates the type and keeps what it did not change.
    let structure = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    let alter = SchemaChange::AlterColumn {
        table: t("it_diff_a", "accounts"),
        column: "email".into(),
        data_type: Some("nvarchar(200)".into()),
        nullable: Some(false),
        default: Some(DefaultChange::Set { expression: "'y'".into() }),
    };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &alter, Some(&structure)).unwrap())).await;
    let after = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    let email = after.column("email").unwrap();
    assert_eq!(
        (email.data_type.as_str(), email.nullable, email.default.as_deref()),
        ("nvarchar(200)", false, Some("'y'"))
    );
    let drop_default = SchemaChange::AlterColumn {
        table: t("it_diff_a", "accounts"),
        column: "email".into(),
        data_type: None,
        nullable: None,
        default: Some(DefaultChange::Drop),
    };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &drop_default, None).unwrap())).await;
    assert!(ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap().column("email").unwrap().default.is_none());
    let renamed = SchemaChange::RenameTable { table: t("it_diff_a", "invoices"), new_name: "bills".into() };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &renamed, None).unwrap())).await;
    let back = SchemaChange::RenameTable { table: t("it_diff_a", "bills"), new_name: "invoices".into() };
    run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), &back, None).unwrap())).await;

    let snapshot = |schema: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model(DB, Some(schema)).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap().into());
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
    run_all(&*s, "DROP TABLE it_diff_a.invoices; DROP TABLE it_diff_a.accounts; DROP TABLE it_diff_b.invoices; DROP TABLE it_diff_b.accounts;\nDROP SCHEMA it_diff_a\nGO\nDROP SCHEMA it_diff_b\nGO").await;
}

/// Runs `sql` in one transaction on the session's write connection.
async fn transact(s: &dyn Session, sql: &[String]) -> DbResult<Vec<u64>> {
    let mut bound = sql.iter().map(|q| Ok(BoundStatement { sql: q.clone(), params: Vec::new(), expect_rows: None }));
    s.run_in_transaction(DB, &mut bound).await
}

#[tokio::test]
async fn write_connection_is_reused_until_an_error() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_writer", "(id int PRIMARY KEY, conn int, note nvarchar(10))").await;
    let insert = |id: u32| format!("INSERT INTO dbo.it_writer VALUES ({id}, @@SPID, NULL)");
    transact(&*s, &[insert(1)]).await.unwrap();
    transact(&*s, &[insert(2)]).await.unwrap();
    // A duplicate key rolls back the whole transaction and drops its connection.
    assert!(transact(&*s, &[insert(3), insert(1)]).await.is_err());
    transact(&*s, &[insert(4)]).await.unwrap();
    // So does an error from the statement source (e.g. a cancelled import).
    let mut failing = [
        Ok(BoundStatement { sql: insert(5), params: Vec::new(), expect_rows: None }),
        Err(dq_core::DbError::invalid("stop")),
    ]
    .into_iter();
    assert!(s.run_in_transaction(DB, &mut failing).await.is_err());
    transact(&*s, &[insert(6)]).await.unwrap();
    // Two grid saves in a row.
    let cv = |c: &str, v: &str| ColumnValue { column: c.into(), value: Some(v.into()) };
    for note in ["a", "b"] {
        let change = RowChange::Update { key: vec![cv("id", "1")], values: vec![cv("note", note)] };
        ops::apply_changes(&*s, &t, &[change], false).await.unwrap();
    }
    let r = s.execute("SELECT id, conn, note FROM dbo.it_writer ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["1", "2", "4", "6"]);
    assert_eq!(rows[0][2], "b");
    assert_eq!(rows[0][1], rows[1][1], "the second transaction reused the connection");
    // Session ids are recycled by the server, so a dropped connection is not
    // checked here by id; the transactions after each failure succeeding is the check.
    run_all(&*s, "DROP TABLE dbo.it_writer").await;
}
