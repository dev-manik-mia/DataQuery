//! Integration tests against a real PostgreSQL server.
//!
//! Start one with `docker compose -p dataquery up -d postgres` and run
//! `DATAQUERY_TEST_PG=localhost:55432 cargo test -p dq-postgres`.
//! Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::result::ExecOptions;
use dq_core::table::{SortSpec, TablePageRequest};
use dq_core::transfer::{CsvOptions, ExportFormat, ExportSource};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_postgres::PostgresAdapter;

const PASSWORD: &str = "dataquery";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_PG").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "5432"));
    Some(ConnConfig {
        engine: Engine::Postgres,
        host: host.into(),
        port: port.parse().unwrap(),
        database: "dataquery_test".into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
    })
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_PG to run PostgreSQL integration tests");
                return;
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    PostgresAdapter.connect(cfg, Some(PASSWORD)).await.expect("connect")
}

fn table(schema: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: NodeKind::Table,
        database: "dataquery_test".into(),
        schema: Some(schema.into()),
        name: name.into(),
    }
}

/// Creates a scratch table unique to the calling test.
async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    s.execute(&format!("DROP TABLE IF EXISTS public.{name}"), OPTS).await.unwrap();
    s.execute(&format!("CREATE TABLE public.{name} {ddl}"), OPTS).await.unwrap();
    table("public", name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = PostgresAdapter.test(&cfg, Some(PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("PostgreSQL "), "{}", info.version);
    assert_eq!(info.database, "dataquery_test");
    assert!(info.tls.is_none());
}

#[tokio::test]
async fn invalid_credentials_are_actionable_and_redacted() {
    let cfg = require_db!();
    let secret = "wrong-Secret-123";
    let err = PostgresAdapter.test(&cfg, Some(secret)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert_eq!(err.code.as_deref(), Some("28P01"));
    assert!(err.action.as_deref().unwrap_or_default().contains("username and password"));
    let json = serde_json::to_string(&err).unwrap_or_default();
    assert!(!format!("{err:?}{json}").contains(secret));
}

#[tokio::test]
async fn network_failure_is_classified() {
    let mut cfg = require_db!();
    cfg.port = 1;
    let err = PostgresAdapter.test(&cfg, Some(PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn tls_is_required_when_asked() {
    let mut cfg = require_db!();
    cfg.ssl_mode = SslMode::VerifyFull;
    let err = PostgresAdapter.test(&cfg, Some(PASSWORD)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert!(dbs.iter().any(|d| d.object.name == "dataquery_test" && d.current));
    let schemas = s.list_children(Some(&dbs.iter().find(|d| d.current).unwrap().object)).await.unwrap();
    assert_eq!(schemas[0].object.name, "public");
    assert!(schemas.iter().any(|n| n.object.name == "billing"));
    let rels = s.list_children(Some(&schemas[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert_eq!(kind("daily_revenue"), Some(NodeKind::MaterializedView));
    let found = s.search("ORDER", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "orders"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute("SELECT big, exact, ts, doc, raw, label, tags, uid, addr, span, (big + 0)::int8 FROM public.type_samples ORDER BY id", OPTS)
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9007199254740993"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[2], Cell::Temporal { value: "2026-10-14T07:59:00.123+00:00".into(), zoned: true });
    assert_eq!(text(&row[3]), r#"{"n": 1, "name": "Zoë ✓"}"#);
    assert_eq!(row[4], Cell::Binary { length: 8, head_hex: "89504e470d0a1a0a".into() });
    assert_eq!(row[5], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(text(&row[6]), r#"{a,"b c",NULL}"#);
    assert_eq!(text(&row[7]), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(text(&row[8]), "192.168.0.1/24");
    assert_eq!(text(&row[9]), "1 year 2 mons 3 days 04:05:06");
    let second = &r.rows[1];
    assert_eq!(second[0], Cell::number("-9223372036854775808"));
    assert_eq!(second[1], Cell::number("-0.0000000001"));
    assert_eq!(second[2], Cell::Null);
    assert_eq!(second[5], Cell::Null);
    assert_eq!(text(&second[6]), "{}");
    assert!(r.columns[0].numeric && !r.columns[5].numeric);
    assert_eq!(r.columns[6].type_name, "text[]");
}

#[tokio::test]
async fn results_are_bounded_and_resumable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("SELECT g FROM generate_series(1, 1000000) g", OPTS).await.unwrap();
    assert_eq!(r.rows.len(), 500);
    assert!(r.has_more);
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    assert!(page.has_more);
    // A new statement closes the open cursor promptly.
    let started = Instant::now();
    let r = s.execute("SELECT 1", OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(s.fetch_more(10).await.is_err());
}

#[tokio::test]
async fn cancel_and_timeout() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute("SELECT pg_sleep(20)", OPTS).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!s.cancel().await.unwrap(), "nothing left to cancel");

    let err = s.execute("SELECT pg_sleep(3)", ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(s.execute("SELECT 1", OPTS).await.is_ok());
}

#[tokio::test]
async fn errors_carry_position_and_hint() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT id, totl FROM public.orders", OPTS).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("42703"));
    assert_eq!(err.position, Some(12));
    assert!(err.hint.unwrap_or_default().contains("total"));
}

#[tokio::test]
async fn notices_and_affected_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("DO $$ BEGIN RAISE NOTICE 'hello'; END $$", OPTS).await.unwrap();
    assert_eq!(r.notices, vec!["NOTICE: hello"]);
    let t = scratch(&*s, "it_affected", "(id int PRIMARY KEY)").await;
    let r = s.execute("INSERT INTO public.it_affected SELECT generate_series(1, 3)", OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    s.execute(&format!("DROP TABLE {}", s.dialect().quoted_name(&t)), OPTS).await.unwrap();
}

#[tokio::test]
async fn structure_and_ddl() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("public", "orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert!(st.columns[0].primary_key && st.columns[0].generated && !st.columns[0].editable);
    assert!(st.constraints.iter().any(|c| c.name == "orders_customer_fk"));
    assert!(st.indexes.iter().any(|i| i.name == "orders_created_at_idx"));
    assert!(s.structure(&table("public", "no_pk")).await.unwrap().row_identity.is_empty());
    // Unique NOT NULL key is accepted as row identity.
    let ddl = s.ddl(&table("public", "orders")).await.unwrap();
    assert!(
        ddl.starts_with("CREATE TABLE public.orders (\n  id bigint GENERATED ALWAYS AS IDENTITY NOT NULL"),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX orders_created_at_idx"));
    let view = ObjectRef { kind: NodeKind::View, ..table("public", "paid_orders") };
    assert!(s.ddl(&view).await.unwrap().starts_with("CREATE VIEW public.paid_orders AS"));
}

#[tokio::test]
async fn table_page_filter_sort_and_count() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let req = TablePageRequest {
        object: table("public", "orders"),
        filter: Some("status = 'paid'".into()),
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await.unwrap();
    assert_eq!(page.rows.len(), 100);
    assert!(page.has_more);
    let totals: Vec<f64> = page.rows.iter().map(|r| text(&r[3]).parse().unwrap()).collect();
    assert!(totals.windows(2).all(|w| w[0] >= w[1]));
    let count = ops::row_count(&*s, &req.object, req.filter.as_deref()).await.unwrap();
    assert_eq!((count.count, count.exact), (400, true));
    let bad = TablePageRequest { filter: Some("1=1; DELETE FROM public.orders".into()), ..req.clone() };
    assert!(ops::table_page(&*s, &bad).await.is_err());
}

#[tokio::test]
async fn catalog_connections_are_read_only() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err =
        s.read_query("dataquery_test", "DELETE FROM public.no_pk", dq_core::value::CellLimits::GRID).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("25006"));
}

#[tokio::test]
async fn edits_apply_atomically_to_the_right_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_edits", "(id int8 PRIMARY KEY, amount numeric(12,2), note text)").await;
    s.execute("INSERT INTO public.it_edits VALUES (9007199254740993, 1.00, 'a'), (2, 2.00, 'b')", OPTS).await.unwrap();
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("9007199254740993"))],
            values: vec![cv("amount", Some("84.50")), cv("note", Some(""))],
        },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("amount", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let preview = ops::preview_changes(&*s, &t, &changes, false).await.unwrap();
    assert!(preview.contains("UPDATE \"public\".\"it_edits\" SET \"amount\" = '84.50'"));
    let report = ops::apply_changes(&*s, &t, &changes, false).await.unwrap();
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT id, amount, note FROM public.it_edits ORDER BY id", OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(rows, [["3", "NULL", "NULL"], ["9007199254740993", "84.50", ""]]);

    // A change that matches no row rolls everything back.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("note", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    let err = ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
    let r = s.execute("SELECT note FROM public.it_edits WHERE id = 3", OPTS).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Null);
    s.execute("DROP TABLE public.it_edits", OPTS).await.unwrap();
}

#[tokio::test]
async fn tables_without_a_key_edit_only_after_opt_in() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_no_key", "(a int, b text, doc json)").await;
    s.execute("INSERT INTO public.it_no_key VALUES (1, NULL, '{}'), (2, 'x', '{}'), (2, 'x', '{}')", OPTS)
        .await
        .unwrap();
    let st = ops::structure(&*s, &t).await.unwrap();
    assert!(st.row_identity.is_empty());
    assert_eq!(st.full_row_key, ["a", "b"], "json has no equality operator");
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let fix_null =
        vec![RowChange::Update { key: vec![cv("a", Some("1")), cv("b", None)], values: vec![cv("b", Some("y"))] }];
    // Without the opt-in nothing is editable.
    assert!(ops::apply_changes(&*s, &t, &fix_null, false).await.is_err());
    assert_eq!(ops::apply_changes(&*s, &t, &fix_null, true).await.unwrap().updated, 1);
    // Duplicate rows match twice, so the change is refused and rolled back.
    let dup = vec![RowChange::Delete { key: vec![cv("a", Some("2")), cv("b", Some("x"))] }];
    assert_eq!(ops::apply_changes(&*s, &t, &dup, true).await.unwrap_err().kind, ErrorKind::Constraint);
    let r = s.execute("SELECT count(*) FROM public.it_no_key WHERE b IS NOT NULL", OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "3");
    s.execute("DROP TABLE public.it_no_key", OPTS).await.unwrap();
}

#[tokio::test]
async fn export_and_import_round_trip() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: table("public", "orders"), filter: Some("id <= 10".into()), sort: None };
    let report = ops::export(&*s, &source, ExportFormat::Csv, &csv).await?;
    assert_eq!(report.rows, 10);
    let json = dir.path().join("q.json");
    let q = ExportSource::Query { sql: "SELECT big, doc FROM public.type_samples ORDER BY id".into() };
    ops::export(&*s, &q, ExportFormat::Json, &json).await?;
    let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(parsed[0]["big"], "9007199254740993");
    assert_eq!(parsed[0]["doc"]["name"], "Zoë ✓");
    assert!(
        ops::export(&*s, &ExportSource::Query { sql: "DELETE FROM public.no_pk".into() }, ExportFormat::Csv, &json)
            .await
            .is_err()
    );

    let t = scratch(&*s, "it_import", "(id int8 PRIMARY KEY, status text, total numeric(12,2))").await;
    let options = CsvOptions { delimiter: ",".into(), has_header: true, empty_as_null: true };
    let mapping = vec![("id".to_owned(), 0), ("status".to_owned(), 2), ("total".to_owned(), 3)];
    let imported = ops::import_csv(&*s, &t, &csv, &options, &mapping).await?;
    assert_eq!(imported, 10);
    // A bad row aborts the whole import.
    let bad = dir.path().join("bad.csv");
    std::fs::write(&bad, "id,x,status,total\n100,,ok,1\n101,,ok,not-a-number\n").unwrap();
    let err = ops::import_csv(&*s, &t, &bad, &options, &mapping).await.unwrap_err();
    assert!(err.message.starts_with("Rows from 1:") || err.code.is_some(), "{err:?}");
    let r = s.execute("SELECT count(*) FROM public.it_import", OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "10");
    s.execute("DROP TABLE public.it_import", OPTS).await?;
    Ok(())
}
