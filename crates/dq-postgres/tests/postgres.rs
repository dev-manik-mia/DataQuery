//! Integration tests against a real PostgreSQL server.
//!
//! Start one with `docker compose -p dataquery up -d postgres` and run
//! `DATAQUERY_TEST_PG=localhost:55432 cargo test -p dq-postgres`.
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
                eprintln!("skipped: set DATAQUERY_TEST_PG to run PostgreSQL integration tests");
                return;
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    PostgresAdapter::POSTGRES.connect(&with_password(cfg, PASSWORD)).await.expect("connect")
}

fn table(schema: &str, name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, "dataquery_test", Some(schema.into()), name)
}

/// Creates a scratch table unique to the calling test.
async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    s.execute(&format!("DROP TABLE IF EXISTS public.{name}"), &[], OPTS).await.unwrap();
    s.execute(&format!("CREATE TABLE public.{name} {ddl}"), &[], OPTS).await.unwrap();
    table("public", name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = PostgresAdapter::POSTGRES.test(&with_password(&cfg, PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("PostgreSQL "), "{}", info.version);
    assert_eq!(info.database, "dataquery_test");
    assert!(info.tls.is_none());
}

#[tokio::test]
async fn invalid_credentials_are_actionable_and_redacted() {
    let cfg = require_db!();
    let secret = "wrong-Secret-123";
    let err = PostgresAdapter::POSTGRES.test(&with_password(&cfg, secret)).await.unwrap_err();
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
    let err = PostgresAdapter::POSTGRES.test(&with_password(&cfg, PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn tls_is_required_when_asked() {
    let mut cfg = require_db!();
    cfg.ssl_mode = SslMode::VerifyFull;
    let err = PostgresAdapter::POSTGRES.test(&with_password(&cfg, PASSWORD)).await.unwrap_err();
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
        .execute("SELECT big, exact, ts, doc, raw, label, tags, uid, addr, span, (big + 0)::int8 FROM public.type_samples ORDER BY id", &[], OPTS)
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
    let r = s.execute("SELECT g FROM generate_series(1, 1000000) g", &[], OPTS).await.unwrap();
    assert_eq!(r.rows.len(), 500);
    assert!(r.has_more);
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    assert!(page.has_more);
    // A new statement closes the open cursor promptly.
    let started = Instant::now();
    let r = s.execute("SELECT 1", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(s.fetch_more(10).await.is_err());
}

#[tokio::test]
async fn cancel_and_timeout() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute("SELECT pg_sleep(20)", &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!s.cancel().await.unwrap(), "nothing left to cancel");

    let err = s.execute("SELECT pg_sleep(3)", &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
}

#[tokio::test]
async fn errors_carry_position_and_hint() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT id, totl FROM public.orders", &[], OPTS).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("42703"));
    assert_eq!(err.position, Some(12));
    assert!(err.hint.unwrap_or_default().contains("total"));
}

#[tokio::test]
async fn notices_and_affected_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("DO $$ BEGIN RAISE NOTICE 'hello'; END $$", &[], OPTS).await.unwrap();
    assert_eq!(r.notices, vec!["NOTICE: hello"]);
    let t = scratch(&*s, "it_affected", "(id int PRIMARY KEY)").await;
    let r = s.execute("INSERT INTO public.it_affected SELECT generate_series(1, 3)", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    s.execute(&format!("DROP TABLE {}", s.dialect().quoted_name(&t)), &[], OPTS).await.unwrap();
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
    assert!(
        s.structure(&table("public", "no_pk")).await.unwrap().row_identity.is_empty(),
        "a table without a key has no row identity"
    );
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
        rows: RowFilter { filter: Some("1=1; DELETE FROM public.orders".into()), ..RowFilter::default() },
        ..req.clone()
    };
    assert!(ops::table_page(&*s, &bad).await.is_err());
}

#[tokio::test]
async fn catalog_connections_are_read_only() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s
        .read_query("dataquery_test", "DELETE FROM public.no_pk", &[], dq_core::value::CellLimits::GRID)
        .await
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("25006"));
}

#[tokio::test]
async fn edits_apply_atomically_to_the_right_rows() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_edits", "(id int8 PRIMARY KEY, amount numeric(12,2), note text)").await;
    s.execute("INSERT INTO public.it_edits VALUES (9007199254740993, 1.00, 'a'), (2, 2.00, 'b')", &[], OPTS)
        .await
        .unwrap();
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
    let r = s.execute("SELECT id, amount, note FROM public.it_edits ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(rows, [["3", "NULL", "NULL"], ["9007199254740993", "84.50", ""]]);

    // A change that matches no row rolls everything back.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("note", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    let err = ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
    let r = s.execute("SELECT note FROM public.it_edits WHERE id = 3", &[], OPTS).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Null);
    s.execute("DROP TABLE public.it_edits", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn tables_without_a_key_edit_only_after_opt_in() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_no_key", "(a int, b text, doc json)").await;
    s.execute("INSERT INTO public.it_no_key VALUES (1, NULL, '{}'), (2, 'x', '{}'), (2, 'x', '{}')", &[], OPTS)
        .await
        .unwrap();
    let st = ops::structure(&*s, &t).await.unwrap();
    assert!(st.row_identity.is_empty(), "{:?}", st.row_identity);
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
    let r = s.execute("SELECT count(*) FROM public.it_no_key WHERE b IS NOT NULL", &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "3");
    s.execute("DROP TABLE public.it_no_key", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn export_and_import_round_trip() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("public", "orders"),
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    let report =
        ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &TransferControl::none()).await?;
    assert_eq!(report.rows, 10);
    let json = dir.path().join("q.json");
    let q = ExportSource::Query { sql: "SELECT big, doc FROM public.type_samples ORDER BY id".into() };
    ops::export(&*s, &q, ExportFormat::Json, &ExportOptions::default(), &json, &TransferControl::none()).await?;
    let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(parsed[0]["big"], "9007199254740993");
    assert_eq!(parsed[0]["doc"]["name"], "Zoë ✓");
    assert!(
        ops::export(
            &*s,
            &ExportSource::Query { sql: "DELETE FROM public.no_pk".into() },
            ExportFormat::Csv,
            &ExportOptions::default(),
            &json,
            &TransferControl::none()
        )
        .await
        .is_err()
    );

    let t = scratch(&*s, "it_import", "(id int8 PRIMARY KEY, status text, total numeric(12,2))").await;
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
    let r = s.execute("SELECT total FROM public.it_import WHERE id = 300", &[], OPTS).await?;
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
    s.execute("DELETE FROM public.it_import WHERE id >= 300", &[], OPTS).await?;
    let script = std::fs::read_to_string(&sql_file).unwrap();
    for statement in dq_core::sql::split_statements(&script, s.dialect().flavor()) {
        s.execute(statement.text(&script), &[], OPTS).await?;
    }
    let r = s.execute("SELECT count(*) FROM public.it_import WHERE id >= 300", &[], OPTS).await?;
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
    s.execute("DROP TABLE public.it_import", &[], OPTS).await?;
    Ok(())
}

#[tokio::test]
async fn parameters_are_bound_as_text() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let params = [Some("9007199254740993".to_owned()), None, Some("it's; DROP TABLE x".to_owned())];
    let r = s.execute("SELECT $1::int8 + 1, $2::text IS NULL, $3", &params, OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "9007199254740994");
    assert_eq!(text(&r.rows[0][1]), "true");
    assert_eq!(text(&r.rows[0][2]), "it's; DROP TABLE x");
    // Types are inferred by the server from the placeholder's context.
    let r = s.execute("SELECT count(*) FROM public.orders WHERE id = $1", &[Some("1".into())], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "1");
    let err = s.execute("SELECT $1, $2", &[Some("1".into())], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    let err = s.execute("CREATE TABLE public.it_read_only (a int)", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert_eq!(err.code.as_deref(), Some("25006"));
}

#[tokio::test]
async fn row_filters_bind_matches_and_searches() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let object = table("public", "orders");
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
    let page = ops::table_page(
        &*s,
        &TablePageRequest { object: object.clone(), rows: rows.clone(), sort: None, offset: 0, limit: 5 },
    )
    .await
    .unwrap();
    assert!(!page.rows.is_empty(), "no rows");
    assert!(page.rows.iter().all(|r| text(&r[2]) == "paid"));
    let none = RowFilter {
        searches: vec![dq_core::table::ColumnSearch { column: "status".into(), contains: "%".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &object, &none).await.unwrap().count, 0, "`%` is a literal");
    let null =
        RowFilter { matches: vec![ColumnValue { column: "discount".into(), value: None }], ..RowFilter::default() };
    assert!(ops::row_count(&*s, &object, &null).await.unwrap().count > 0);
}

#[tokio::test]
async fn explorer_lists_routines_sequences_indexes_and_triggers() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    for sql in [
        "CREATE SCHEMA IF NOT EXISTS it_objects",
        "DROP TABLE IF EXISTS it_objects.events",
        "DROP FUNCTION IF EXISTS it_objects.touch() CASCADE",
        "DROP FUNCTION IF EXISTS it_objects.add(int, int)",
        "DROP FUNCTION IF EXISTS it_objects.add(text, text)",
        "DROP PROCEDURE IF EXISTS it_objects.noop()",
        "DROP SEQUENCE IF EXISTS it_objects.ticket_seq",
        "CREATE TABLE it_objects.events (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, at timestamptz)",
        "CREATE INDEX events_at_idx ON it_objects.events (at)",
        "CREATE FUNCTION it_objects.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.at := now(); RETURN NEW; END $$",
        "CREATE TRIGGER events_touch BEFORE INSERT ON it_objects.events FOR EACH ROW EXECUTE FUNCTION it_objects.touch()",
        "CREATE FUNCTION it_objects.add(a int, b int) RETURNS int LANGUAGE sql AS 'SELECT a + b'",
        "CREATE FUNCTION it_objects.add(a text, b text) RETURNS text LANGUAGE sql AS 'SELECT a || b'",
        "CREATE PROCEDURE it_objects.noop() LANGUAGE sql AS 'SELECT 1'",
        "CREATE SEQUENCE it_objects.ticket_seq START 100 INCREMENT 5",
    ] {
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let schema = ObjectRef::new(NodeKind::Schema, "dataquery_test", Some("it_objects".into()), "it_objects");
    let nodes = s.list_children(Some(&schema)).await.unwrap();
    let groups: Vec<_> = nodes
        .iter()
        .filter(|n| n.object.kind == NodeKind::Group)
        .map(|n| (n.object.name.as_str(), n.row_estimate))
        .collect();
    assert_eq!(
        groups,
        [("sequences", Some(2)), ("functions", Some(3)), ("procedures", Some(1))],
        "identity sequence counts too"
    );

    let functions =
        s.list_children(Some(&nodes.iter().find(|n| n.object.name == "functions").unwrap().object)).await.unwrap();
    let overloads: Vec<_> =
        functions.iter().filter(|n| n.object.name == "add").map(|n| n.object.parent.clone().unwrap()).collect();
    assert_eq!(overloads, ["a integer, b integer", "a text, b text"]);
    let add_text = functions.iter().find(|n| n.object.parent.as_deref() == Some("a text, b text")).unwrap();
    assert!(s.ddl(&add_text.object).await.unwrap().contains("a || b"));

    let events = nodes.iter().find(|n| n.object.name == "events").unwrap();
    assert!(events.expandable);
    let children = s.list_children(Some(&events.object)).await.unwrap();
    let names: Vec<_> = children.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect();
    assert!(
        names.contains(&(NodeKind::Index, "events_at_idx")) && names.contains(&(NodeKind::Trigger, "events_touch")),
        "{names:?}"
    );
    for child in &children {
        let ddl = s.ddl(&child.object).await.unwrap();
        assert!(ddl.starts_with("CREATE"), "{ddl}");
    }
    let seq = ObjectRef::new(NodeKind::Sequence, "dataquery_test", Some("it_objects".into()), "ticket_seq");
    let ddl = s.ddl(&seq).await.unwrap();
    assert!(ddl.contains("INCREMENT BY 5") && ddl.contains("START WITH 100"), "{ddl}");

    let structure = ops::structure(&*s, &table("public", "orders")).await.unwrap();
    assert_eq!(structure.foreign_keys.len(), 1);
    assert_eq!(structure.foreign_keys[0].target.name, "customers");
    assert_eq!(structure.foreign_keys[0].columns, ["customer_id"]);
    assert!(structure.size.unwrap().total_bytes > 0);
    assert!(structure.indexes.iter().all(|i| i.size_bytes.is_some()));
    assert_eq!(structure.indexes.iter().find(|i| i.primary).unwrap().columns, ["id"]);
}

#[tokio::test]
async fn dependencies_and_schema_model() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let deps = s.dependencies(&table("public", "orders")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "customers" && d.relation.starts_with("foreign key")));
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders" && d.object.kind == NodeKind::View));
    let view = ObjectRef::new(NodeKind::View, "dataquery_test", Some("public".into()), "paid_orders");
    assert!(s.dependencies(&view).await.unwrap().uses.iter().any(|d| d.object.name == "orders"));
    let model = s.schema_model("dataquery_test", Some("public")).await.unwrap();
    let orders = model.tables.iter().find(|t| t.object.name == "orders").unwrap();
    assert!(orders.columns.iter().any(|c| c.name == "id" && c.primary_key && !c.nullable));
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    assert!(!model.truncated);
}

#[tokio::test]
async fn monitor_shows_blocking_and_cancels() {
    let cfg = require_db!();
    let holder = session(&cfg).await;
    let waiter = session(&cfg).await;
    let watcher = session(&cfg).await;
    scratch(&*holder, "it_monitor", "(id int PRIMARY KEY, v int)").await;
    holder.execute("INSERT INTO public.it_monitor VALUES (1, 0)", &[], OPTS).await.unwrap();
    let holder_pid = text(&holder.execute("SELECT pg_backend_pid()", &[], OPTS).await.unwrap().rows[0][0]);
    let waiter_pid: i64 =
        text(&waiter.execute("SELECT pg_backend_pid()", &[], OPTS).await.unwrap().rows[0][0]).parse().unwrap();
    holder.execute("BEGIN", &[], OPTS).await.unwrap();
    holder.execute("UPDATE public.it_monitor SET v = 1 WHERE id = 1", &[], OPTS).await.unwrap();
    let blocked = tokio::spawn({
        let waiter = waiter.clone();
        async move { waiter.execute("UPDATE public.it_monitor SET v = 2 WHERE id = 1", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("postgres monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        let snapshot = monitor.snapshot(30).await.unwrap();
        if snapshot.blocked.unwrap_or(0) > 0 || Instant::now() > deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let waiting = snapshot.processes.iter().find(|p| p.pid == waiter_pid).expect("waiter listed");
    assert_eq!(waiting.blocked_by, [holder_pid.parse::<i64>().unwrap()]);
    assert!(snapshot.connections.unwrap().max > 0);
    assert!(snapshot.locks.iter().any(|l| !l.granted && l.pid == waiter_pid));
    assert_eq!(snapshot.replication.as_ref().unwrap().role, "primary");
    assert!(monitor.kill(waiter_pid, false).await.unwrap());
    let err = blocked.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    holder.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert!(monitor.server_settings(Some("max_conn")).await.unwrap().iter().any(|s| s.name == "max_connections"));
    assert!(!monitor.largest_tables("dataquery_test", 5).await.unwrap().is_empty(), "no largest tables");
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in dq_core::sql::split_statements(sql, dq_core::sql::SqlFlavor::Postgres) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

#[tokio::test]
async fn schema_changes_diff_and_migrations() {
    use dq_core::diff::{Container, diff};
    use dq_core::schema::ConstraintKind;
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, IndexColumn, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    run_all(&*s, "DROP SCHEMA IF EXISTS it_diff_a CASCADE; DROP SCHEMA IF EXISTS it_diff_b CASCADE; CREATE SCHEMA it_diff_a; CREATE SCHEMA it_diff_b").await;
    let t = |schema: &str, name: &str| ObjectRef::new(NodeKind::Table, "dataquery_test", Some(schema.into()), name);
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
            columns: vec![ColumnDef { identity: true, nullable: false, ..col("id", "bigint") }, col("email", "text")],
            primary_key: vec!["id".into()],
        },
        SchemaChange::CreateTable {
            table: t("it_diff_a", "invoices"),
            columns: vec![
                ColumnDef { identity: true, nullable: false, ..col("id", "bigint") },
                col("account_id", "bigint"),
                ColumnDef { default: Some("0".into()), nullable: false, ..col("amount", "numeric(12,2)") },
            ],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddForeignKey {
            table: t("it_diff_a", "invoices"),
            name: "invoices_account_fk".into(),
            columns: vec!["account_id".into()],
            target: t("it_diff_a", "accounts"),
            target_columns: vec!["id".into()],
            on_delete: Some("cascade".into()),
            on_update: None,
        },
        SchemaChange::AddUnique {
            table: t("it_diff_a", "accounts"),
            name: "accounts_email_key".into(),
            columns: vec!["email".into()],
        },
        SchemaChange::AddCheck {
            table: t("it_diff_a", "invoices"),
            name: "amount_positive".into(),
            expression: "amount >= 0".into(),
        },
        SchemaChange::CreateIndex {
            table: t("it_diff_a", "invoices"),
            name: "invoices_amount_idx".into(),
            columns: vec![IndexColumn { name: "amount".into(), descending: true }],
            unique: false,
        },
        SchemaChange::AlterColumn {
            table: t("it_diff_a", "accounts"),
            column: "email".into(),
            data_type: Some("varchar(320)".into()),
            nullable: Some(false),
            default: Some(DefaultChange::Set { expression: "''".into() }),
        },
    ];
    let mut statements = Vec::new();
    for change in &changes {
        statements.extend(ddl::statements(s.dialect(), change, None).unwrap());
    }
    let bound = statements.iter().map(|sql| Ok(BoundStatement { sql: sql.clone(), params: vec![], expect_rows: None }));
    s.run_in_transaction("dataquery_test", &mut bound.collect::<Vec<_>>().into_iter()).await.unwrap();
    let accounts = ops::structure(&*s, &t("it_diff_a", "accounts")).await.unwrap();
    let email = accounts.column("email").unwrap();
    assert_eq!((email.data_type.as_str(), email.nullable), ("character varying(320)", false));
    assert!(accounts.constraints.iter().any(|c| c.kind == ConstraintKind::Unique));

    // Diff a → empty b, apply, then nothing is left to change.
    let snapshot = |schema: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model("dataquery_test", Some(schema)).await.unwrap();
            let mut tables = Vec::new();
            for table in model.tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap().into());
            }
            tables
        }
    };
    let a = Container { database: "dataquery_test".into(), schema: Some("it_diff_a".into()) };
    let b = Container { database: "dataquery_test".into(), schema: Some("it_diff_b".into()) };
    let plan = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(plan.sql.contains(r#"CREATE TABLE "it_diff_b"."invoices""#), "{}", plan.sql);
    assert!(plan.sql.contains(r#"REFERENCES "it_diff_b"."accounts""#), "{}", plan.sql);
    run_all(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(again.items.is_empty(), "{:#?}\n{}", again.items, again.sql);

    // A change on one side shows up as a column change.
    run_all(&*s, "ALTER TABLE it_diff_b.invoices ADD COLUMN memo text; ALTER TABLE it_diff_b.invoices ALTER COLUMN amount DROP NOT NULL").await;
    let drift = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(drift.sql.contains("DROP COLUMN \"memo\"") && drift.sql.contains("SET NOT NULL"), "{}", drift.sql);

    // Framework migrations.
    run_all(&*s, "CREATE TABLE it_diff_a.django_migrations (id serial PRIMARY KEY, app text, name text, applied timestamptz DEFAULT now()); \
                  INSERT INTO it_diff_a.django_migrations (app, name) VALUES ('auth', '0001_initial'), ('auth', '0002_more'); \
                  CREATE TABLE it_diff_a._prisma_migrations (id text PRIMARY KEY, migration_name text, started_at timestamptz DEFAULT now(), finished_at timestamptz, rolled_back_at timestamptz); \
                  INSERT INTO it_diff_a._prisma_migrations (id, migration_name, finished_at) VALUES ('a', '20260101_init', now()), ('b', '20260102_broken', NULL)").await;
    let model = s.schema_model("dataquery_test", Some("it_diff_a")).await.unwrap();
    let found = dq_core::migrations::inspect(&*s, &model).await.unwrap();
    let django = found.iter().find(|f| f.framework == "Django").unwrap();
    assert_eq!(django.entries[0].version, "0002_more");
    assert_eq!(django.entries[0].name.as_deref(), Some("auth"));
    let prisma = found.iter().find(|f| f.framework == "Prisma").unwrap();
    assert!(prisma.entries.iter().any(|e| e.status == "failed" && e.name.as_deref() == Some("20260102_broken")));
    run_all(&*s, "DROP SCHEMA it_diff_a CASCADE; DROP SCHEMA it_diff_b CASCADE").await;
}

#[tokio::test]
async fn full_cell_values_by_key() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let object = table("public", "type_samples");
    let key = [ColumnValue { column: "id".into(), value: Some("1".into()) }];
    let cell = ops::cell_value(&*s, &object, &key, "raw", dq_core::value::CellLimits::FULL).await.unwrap();
    let Cell::Binary { length, head_hex } = cell else { panic!("binary expected") };
    assert_eq!(head_hex.len() as u64, length * 2, "full value, not just the head");
    assert!(
        ops::cell_value(
            &*s,
            &object,
            &[ColumnValue { column: "id".into(), value: Some("-5".into()) }],
            "raw",
            dq_core::value::CellLimits::FULL
        )
        .await
        .is_err()
    );
}

/// Runs `sql` in one transaction on the session's write connection.
async fn transact(s: &dyn Session, sql: &[String]) -> DbResult<Vec<u64>> {
    let mut bound = sql.iter().map(|q| Ok(BoundStatement { sql: q.clone(), params: Vec::new(), expect_rows: None }));
    s.run_in_transaction("dataquery_test", &mut bound).await
}

#[tokio::test]
async fn write_connection_is_reused_until_an_error() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_writer", "(id int PRIMARY KEY, conn int, note text)").await;
    let insert = |id: u32| format!("INSERT INTO public.it_writer VALUES ({id}, pg_backend_pid())");
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
    assert!(s.run_in_transaction("dataquery_test", &mut failing).await.is_err());
    transact(&*s, &[insert(6)]).await.unwrap();
    // Two grid saves in a row.
    let cv = |c: &str, v: &str| ColumnValue { column: c.into(), value: Some(v.into()) };
    for note in ["a", "b"] {
        let change = RowChange::Update { key: vec![cv("id", "1")], values: vec![cv("note", note)] };
        ops::apply_changes(&*s, &t, &[change], false).await.unwrap();
    }
    let r = s.execute("SELECT id, conn, note FROM public.it_writer ORDER BY id", &[], OPTS).await.unwrap();
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["1", "2", "4", "6"]);
    assert_eq!(rows[0][2], "b");
    assert_eq!(rows[0][1], rows[1][1], "the second transaction reused the connection");
    assert_ne!(rows[1][1], rows[2][1], "the failed transaction's connection was dropped");
    assert_ne!(rows[2][1], rows[3][1], "the cancelled transaction's connection was dropped");
    s.execute("DROP TABLE public.it_writer", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn cached_structures_serve_pages_until_invalidated() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let t = scratch(&*s, "it_cached", "(id int PRIMARY KEY)").await;
    let cache = dq_core::schema_cache::StructureCache::default();
    let cached = ops::SessionRef { session: &*s, structures: Some(&cache) };
    let request = TablePageRequest { object: t.clone(), rows: RowFilter::default(), sort: None, offset: 0, limit: 10 };
    assert_eq!(ops::table_page(cached, &request).await.unwrap().columns.len(), 1);
    s.execute("ALTER TABLE public.it_cached ADD COLUMN note text", &[], OPTS).await.unwrap();
    assert_eq!(ops::table_page(cached, &request).await.unwrap().columns.len(), 1, "served from the cache");
    cache.invalidate(Some(&t));
    assert_eq!(ops::table_page(cached, &request).await.unwrap().columns.len(), 2);
    s.execute("DROP TABLE public.it_cached", &[], OPTS).await.unwrap();
}
