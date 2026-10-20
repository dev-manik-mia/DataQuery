//! Integration tests against `bigquery-emulator`, which speaks the `BigQuery`
//! REST API v2 (on `ZetaSQL` and SQLite) without authentication. They check the
//! job protocol, paging, parameters, value decoding, the catalog and the
//! shared table operations; a real Google Cloud project was not available.
//! The emulator reports no changed-row counts, sessions or `TABLES.ddl`, so
//! those paths fall back here.
//!
//! It also resolves unqualified names in any dataset, so the tests name theirs.
//!
//! Start it with `npm run db:up:bigquery` and run `npm run test:integration:bigquery`
//! (`DATAQUERY_TEST_BIGQUERY=http://localhost:59050`, one test at a time: the
//! emulator's memory grows with every concurrent job).

use std::sync::Arc;

use dq_bigquery::BigqueryAdapter;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::ConnConfig;
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::result::ExecOptions;
use dq_core::table::{RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{ExportFormat, ExportOptions, ExportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};

const PROJECT: &str = "dataquery-test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config(dataset: &str) -> Option<ConnConfig> {
    let endpoint = std::env::var("DATAQUERY_TEST_BIGQUERY").ok()?;
    let mut cfg = ConnConfig {
        engine: Engine::Bigquery,
        host: String::new(),
        port: 0,
        database: PROJECT.into(),
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    };
    cfg.properties.insert("endpoint".into(), endpoint);
    cfg.properties.insert("dataset".into(), dataset.into());
    Some(cfg)
}

/// Creates the dataset through the REST API (the emulator has no `CREATE SCHEMA`,
/// and replacing a dataset can corrupt its store, so every run uses new names).
async fn fresh_dataset(endpoint: &str, dataset: &str) {
    let http = reqwest::Client::new();
    let datasets = format!("{endpoint}/bigquery/v2/projects/{PROJECT}/datasets");
    let body = serde_json::json!({ "datasetReference": { "projectId": PROJECT, "datasetId": dataset } });
    let response = http.post(&datasets).json(&body).send().await.expect("emulator reachable");
    assert!(response.status().is_success(), "create dataset: {}", response.text().await.unwrap_or_default());
}

/// A session on a fresh dataset seeded for one test.
async fn seeded(name: &str) -> Option<(Arc<dyn Session>, String)> {
    let dataset = format!("dq_{name}_{}", std::process::id());
    let cfg = config(&dataset)?;
    fresh_dataset(cfg.property("endpoint")?, &dataset).await;
    let s = BigqueryAdapter.connect(&ConnectParams::new(cfg, None)).await.expect("connect");
    let d = &dataset;
    for sql in [
        format!("CREATE TABLE {d}.customers (id INT64 NOT NULL, email STRING NOT NULL)"),
        format!(
            "CREATE TABLE {d}.orders (id INT64 NOT NULL, customer_id INT64, status STRING NOT NULL, total NUMERIC NOT NULL, notes STRING)"
        ),
        format!(
            "CREATE TABLE {d}.type_samples (id INT64, big INT64, exact BIGNUMERIC, ts TIMESTAMP, day DATE, seen DATETIME, \
             label STRING, flag BOOL, ratio FLOAT64, raw BYTES, doc JSON, place GEOGRAPHY, tags ARRAY<STRING>, \
             item STRUCT<sku STRING, qty INT64>)"
        ),
        format!("CREATE VIEW {d}.paid_orders AS SELECT * FROM {d}.orders WHERE status = 'paid'"),
        format!(
            "INSERT INTO {d}.customers SELECT x, CONCAT('user', CAST(x AS STRING), '@example.com') FROM UNNEST(GENERATE_ARRAY(1, 50)) AS x"
        ),
        format!(
            "INSERT INTO {d}.orders SELECT x, MOD(x, 50) + 1, CASE MOD(x, 3) WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' ELSE 'refunded' END, \
             CAST(MOD(x * 37, 400) AS NUMERIC) + 12.25, NULL FROM UNNEST(GENERATE_ARRAY(1, 1200)) AS x"
        ),
        format!(
            "INSERT INTO {d}.type_samples VALUES (1, 9223372036854775807, BIGNUMERIC '123456789012345678901234567890.0123456789', \
             TIMESTAMP '2026-10-14 09:59:00.123456+02', DATE '2026-10-14', DATETIME '2026-10-14 09:59:00.5', '', TRUE, 1.5, \
             b'\\x00\\xff', JSON '{{\"name\": \"Zoë ✓\"}}', ST_GEOGPOINT(1, 2), ['a', 'b'], STRUCT('x-1', 3)), \
             (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, [], NULL)"
        ),
    ] {
        s.execute(&sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    Some((s, dataset))
}

macro_rules! seeded {
    ($name:expr) => {
        match seeded($name).await {
            Some(s) => s,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_BIGQUERY to run BigQuery integration tests");
                return;
            }
        }
    };
}

fn table(dataset: &str, name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, PROJECT, Some(dataset.into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn connects_and_reports_errors() {
    let (s, dataset) = seeded!("login");
    assert_eq!((s.info().engine, s.info().database.as_str()), (Engine::Bigquery, PROJECT));
    assert!(s.info().tls.is_none(), "the emulator speaks HTTP");
    let err = s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound, "{err:?}");
    assert_eq!(s.execute("SELEC 1", &[], OPTS).await.unwrap_err().kind, ErrorKind::Syntax);
    let mut cfg = config(&dataset).unwrap();
    cfg.properties.insert("endpoint".into(), "http://127.0.0.1:1".into());
    let err = BigqueryAdapter.test(&ConnectParams::new(cfg, None)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn values_decode_exactly() {
    let (s, d) = seeded!("values");
    let sql = format!(
        "SELECT big, exact, ts, day, seen, label, flag, ratio, raw, doc, place, tags, item, id FROM {d}.type_samples ORDER BY id"
    );
    let r = s.execute(&sql, &[], OPTS).await.unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("123456789012345678901234567890.0123456789"));
    assert_eq!(row[2], Cell::Temporal { value: "2026-10-14 07:59:00.123456+00:00".into(), zoned: true });
    assert_eq!(row[3], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(row[4], Cell::Temporal { value: "2026-10-14 09:59:00.5".into(), zoned: false });
    assert_eq!(row[5], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(row[6], Cell::Bool { value: true });
    assert_eq!(row[7], Cell::number("1.5"));
    assert_eq!(row[8], Cell::Binary { length: 2, head_hex: "00ff".into() });
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[9])).unwrap()["name"], "Zoë ✓");
    assert!(text(&row[10]).starts_with("POINT"), "{:?}", row[10]);
    assert_eq!(text(&row[11]), r#"["a","b"]"#);
    let item: serde_json::Value = serde_json::from_str(&text(&row[12])).unwrap();
    assert_eq!(item, serde_json::json!({ "sku": "x-1", "qty": 3 }));
    assert_eq!((&r.rows[1][0], text(&r.rows[1][11]).as_str()), (&Cell::Null, "[]"));
    let types: Vec<&str> = r.columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(&types[..3], ["INT64", "BIGNUMERIC", "TIMESTAMP"]);
    assert_eq!(types[11], "ARRAY<STRING>");
    assert!(r.columns[0].numeric && !r.columns[5].numeric);
}

#[tokio::test]
async fn results_page_and_parameters_bind() {
    let (s, d) = seeded!("pages");
    let all = format!("SELECT id FROM {d}.orders ORDER BY id");
    let r = s.execute(&all, &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!((text(&page.rows[0][0]).as_str(), page.has_more), ("501", true));
    let last = s.fetch_more(500).await.unwrap();
    assert_eq!((last.rows.len(), last.has_more), (200, false));
    // Editor values are typed by their spelling: `paid` is a STRING, `100` an INT64.
    let sql = format!("SELECT COUNT(*) FROM {d}.orders WHERE status = @status AND total > @min AND @status = @status");
    let r = s.execute(&sql, &[Some("paid".into()), Some("100".into())], OPTS).await.unwrap();
    assert!(text(&r.rows[0][0]).parse::<u32>().unwrap() > 0);
    assert!(s.execute("SELECT @a", &[], OPTS).await.is_err(), "a missing value is refused");
    let r = s.execute(&format!("UPDATE {d}.orders SET notes = 'x' WHERE id <= 3"), &[], OPTS).await.unwrap();
    assert!(r.columns.is_empty() && r.notices.iter().all(|n| n.starts_with("BigQuery: ")), "{r:?}");
    let script = format!("BEGIN TRANSACTION;\nUPDATE {d}.orders SET notes = 'y' WHERE id = 4;\nCOMMIT TRANSACTION;");
    s.execute(&script, &[], OPTS).await.unwrap();
    let r = s.execute(&format!("SELECT notes FROM {d}.orders WHERE id = 4"), &[], OPTS).await.unwrap();
    assert_eq!(text(&r.rows[0][0]), "y");
}

#[tokio::test]
async fn explorer_structure_and_model() {
    let (s, dataset) = seeded!("catalog");
    let projects = s.list_children(None).await.unwrap();
    let project = projects.iter().find(|p| p.object.name == PROJECT).expect("project listed");
    assert!(project.current);
    let datasets = s.list_children(Some(&project.object)).await.unwrap();
    let ds = datasets.iter().find(|n| n.object.name == dataset).expect("dataset listed");
    assert!(ds.current, "the default dataset is marked");
    let rels = s.list_children(Some(&ds.object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("orders"), Some(NodeKind::Table));
    assert_eq!(kind("paid_orders"), Some(NodeKind::View));
    assert!(s.search("ord", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));
    let st = s.structure(&table(&dataset, "type_samples")).await.unwrap();
    let column = |n: &str| st.column(n).unwrap();
    assert_eq!(column("tags").data_type, "ARRAY<STRING>");
    assert_eq!(column("item").data_type, "STRUCT<sku STRING, qty INT64>");
    assert!(column("exact").numeric && !column("tags").editable && column("label").editable);
    assert_eq!(s.row_estimate(&table(&dataset, "orders")).await.unwrap(), Some(1200));
    let ddl = s.ddl(&table(&dataset, "orders")).await.unwrap();
    assert!(ddl.starts_with(&format!("CREATE TABLE {dataset}.orders (")), "{ddl}");
    let view = ObjectRef { kind: NodeKind::View, ..table(&dataset, "paid_orders") };
    assert!(s.ddl(&view).await.unwrap().starts_with("CREATE VIEW"));
    let model = s.schema_model(PROJECT, Some(&dataset)).await.unwrap();
    let orders = model.tables.iter().find(|t| t.object.name == "orders").expect("orders in the model");
    assert_eq!(orders.columns.len(), 5);
    assert_eq!(s.structure(&table(&dataset, "missing")).await.unwrap_err().kind, ErrorKind::NotFound);
    let plan = s.explain(&format!("SELECT * FROM {dataset}.orders"), &[], false, 30).await.unwrap();
    assert!(plan.plan.unwrap().label.contains("dry run"));
    let snapshot = s.monitor().unwrap().snapshot(60).await.unwrap();
    assert!(snapshot.processes.iter().all(|p| p.state.as_deref() != Some("done")));
}

#[tokio::test]
async fn table_pages_edits_and_exports() -> DbResult<()> {
    let Some((s, dataset)) = seeded("edits").await else { return Ok(()) };
    let orders = table(&dataset, "orders");
    let req = TablePageRequest {
        object: orders.clone(),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await?;
    assert_eq!((page.rows.len(), page.has_more), (100, true));
    assert_eq!(ops::row_count(&*s, &orders, &req.rows).await?.count, 400);
    // No declared key: rows are matched on every comparable column (the grid asks first).
    let structure = ops::structure(&*s, &orders).await?;
    assert!(structure.row_identity.is_empty() && structure.full_row_key.len() == 5);
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let row = |id: &str, total: &str| {
        vec![
            cv("id", Some(id)),
            cv("customer_id", Some("2")),
            cv("status", Some("paid")),
            cv("total", Some(total)),
            cv("notes", None),
        ]
    };
    let changes = vec![
        RowChange::Update {
            key: row("1", "49.25"),
            values: vec![cv("total", Some("84.50")), cv("notes", Some("Zoë ✓"))],
        },
        RowChange::Insert {
            values: vec![cv("id", Some("5000")), cv("status", Some("paid")), cv("total", Some("9.99"))],
        },
    ];
    let report = ops::apply_changes(&*s, &orders, &changes, true).await?;
    assert_eq!((report.updated, report.inserted), (1, 1));
    let r = s
        .execute(&format!("SELECT total, notes FROM {dataset}.orders WHERE id IN (1, 5000) ORDER BY id"), &[], OPTS)
        .await?;
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("84.5", "Zoë ✓"));
    assert_eq!(text(&r.rows[1][0]), "9.99");
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: orders,
        rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() },
        sort: None,
    };
    let exported =
        ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &TransferControl::none()).await?;
    assert_eq!(exported.rows, 10);
    Ok(())
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let Some(mut cfg) = config("dq_ro") else { return };
    cfg.read_only = true;
    let s = BigqueryAdapter.connect(&ConnectParams::new(cfg, None)).await.unwrap();
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    assert_eq!(s.execute("CREATE SCHEMA nope", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
    let script = "DECLARE n INT64 DEFAULT 1;\nSELECT n";
    assert_eq!(s.execute(script, &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
}
