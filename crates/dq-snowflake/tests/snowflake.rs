//! Integration tests against fakesnow, a Snowflake emulator that speaks the
//! Snowflake REST protocol (on DuckDB). They check the protocol, result
//! decoding and the catalog; a real Snowflake account was not available.
//!
//! Start it with `docker compose -p dataquery up -d fakesnow` and run
//! `DATAQUERY_TEST_SNOWFLAKE=localhost:58000 cargo test -p dq-snowflake`.

use std::sync::Arc;

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::result::ExecOptions;
use dq_core::table::{RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{ExportFormat, ExportOptions, ExportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_snowflake::SnowflakeAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config(database: &str) -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_SNOWFLAKE").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "8000"));
    let mut cfg = ConnConfig {
        engine: Engine::Snowflake,
        host: host.into(),
        port: port.parse().unwrap(),
        database: database.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    };
    cfg.properties.insert("schema".into(), "PUBLIC".into());
    cfg.properties.insert("warehouse".into(), "COMPUTE_WH".into());
    Some(cfg)
}

/// A session on a fresh database seeded for one test.
async fn seeded(name: &str) -> Option<(Arc<dyn Session>, String)> {
    let database = format!("DQ_{name}");
    let cfg = config(&database)?;
    let s = SnowflakeAdapter.connect(&ConnectParams::new(cfg, Some("x".into()))).await.expect("connect");
    for sql in [
        format!("CREATE DATABASE IF NOT EXISTS {database}"),
        format!("USE DATABASE {database}"),
        "CREATE SCHEMA IF NOT EXISTS PUBLIC".into(),
        "CREATE OR REPLACE TABLE PUBLIC.CUSTOMERS (ID NUMBER(38,0) PRIMARY KEY, EMAIL VARCHAR(255) NOT NULL)".into(),
        "CREATE OR REPLACE TABLE PUBLIC.ORDERS (ID NUMBER(38,0) PRIMARY KEY, CUSTOMER_ID NUMBER(38,0), STATUS VARCHAR(20) NOT NULL, TOTAL NUMBER(12,2) NOT NULL, NOTES VARCHAR)".into(),
        "CREATE OR REPLACE TABLE PUBLIC.TYPE_SAMPLES (ID NUMBER(10,0), BIG NUMBER(38,0), EXACT NUMBER(38,10), TS TIMESTAMP_NTZ(3), DAY DATE, LABEL VARCHAR, FLAG BOOLEAN, RATIO FLOAT, DOC VARIANT)".into(),
        "CREATE OR REPLACE VIEW PUBLIC.PAID_ORDERS AS SELECT * FROM PUBLIC.ORDERS WHERE STATUS = 'paid'".into(),
        "INSERT INTO PUBLIC.CUSTOMERS SELECT SEQ4() + 1, 'user' || (SEQ4() + 1) || '@example.com' FROM TABLE(GENERATOR(ROWCOUNT => 50))".into(),
        "INSERT INTO PUBLIC.ORDERS SELECT SEQ4() + 1, MOD(SEQ4() + 1, 50) + 1, CASE MOD(SEQ4() + 1, 3) WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' ELSE 'refunded' END, MOD((SEQ4() + 1) * 37, 400) + 12.25, NULL FROM TABLE(GENERATOR(ROWCOUNT => 1200))".into(),
        "INSERT INTO PUBLIC.TYPE_SAMPLES SELECT 1, 9223372036854775807, 1234567890123456789012345678.0123456789, '2026-10-14 09:59:00.123'::TIMESTAMP_NTZ, '2026-10-14'::DATE, '', TRUE, 1.5, PARSE_JSON('{\"name\": \"Zoë ✓\"}')".into(),
        // fakesnow encodes a NULL timestamp as an invalid Arrow struct (real Snowflake masks it), so TS is set.
        "INSERT INTO PUBLIC.TYPE_SAMPLES (ID, EXACT, TS) VALUES (2, -0.0000000001, '1970-01-01 00:00:00'::TIMESTAMP_NTZ)".into(),
    ] {
        s.execute(&sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    Some((s, database))
}

macro_rules! seeded {
    ($name:expr) => {
        match seeded($name).await {
            Some(s) => s,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_SNOWFLAKE to run Snowflake integration tests");
                return;
            }
        }
    };
}

fn table(database: &str, name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, database, Some("PUBLIC".into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn logs_in_and_reports_errors() {
    let (s, database) = seeded!("LOGIN");
    assert!(s.info().version.starts_with("Snowflake "), "{}", s.info().version);
    assert_eq!(s.info().engine, Engine::Snowflake);
    assert_eq!(s.info().database, database);
    let err = s.execute("SELECT * FROM PUBLIC.MISSING_TABLE", &[], OPTS).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::NotFound | ErrorKind::Server), "{err:?}");
    let cfg = ConnConfig { port: 1, ..config("X").unwrap() };
    let err = SnowflakeAdapter.test(&ConnectParams::new(cfg, Some("x".into()))).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn values_decode_from_arrow() {
    let (s, _) = seeded!("VALUES");
    let r = s.execute("SELECT BIG, EXACT, TS, DAY, LABEL, FLAG, RATIO, DOC, ID FROM PUBLIC.TYPE_SAMPLES ORDER BY ID", &[], OPTS).await.unwrap();
    let row = &r.rows[0];
    // fakesnow stores NUMBER(38,0) as BIGINT; real Snowflake keeps 38 digits.
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert!(matches!(&row[2], Cell::Temporal { value, zoned: false } if value.starts_with("2026-10-14 09:59:00.123")), "{:?}", row[2]);
    assert_eq!(row[3], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(row[4], Cell::Text { value: String::new(), truncated: None });
    assert_eq!(row[5], Cell::Bool { value: true });
    assert_eq!(row[6], Cell::number("1.5"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[7])).unwrap()["name"], "Zoë ✓");
    assert_eq!(r.rows[1][1], Cell::number("-0.0000000001"));
    assert_eq!(r.rows[1][0], Cell::Null);
    assert!(r.columns[0].numeric && !r.columns[4].numeric);
}

#[tokio::test]
async fn results_page_and_parameters_bind() {
    let (s, _) = seeded!("PAGES");
    let r = s.execute("SELECT ID FROM PUBLIC.ORDERS ORDER BY ID", &[], ExecOptions { row_limit: 100, timeout_secs: 30 }).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (100, true));
    let page = s.fetch_more(100).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "101");
    let r = s.execute("SELECT COUNT(*) FROM PUBLIC.ORDERS WHERE STATUS = ? AND TOTAL > ?", &[Some("paid".into()), Some("100".into())], OPTS).await.unwrap();
    assert!(text(&r.rows[0][0]).parse::<u32>().unwrap() > 0);
    let r = s.execute("UPDATE PUBLIC.ORDERS SET NOTES = 'x' WHERE ID <= 3", &[], OPTS).await.unwrap();
    assert_eq!(r.affected, Some(3));
}

#[tokio::test]
async fn explorer_structure_and_model() {
    let (s, database) = seeded!("CATALOG");
    let dbs = s.list_children(None).await.unwrap();
    let db = dbs.iter().find(|d| d.object.name == database).expect("database listed");
    let schemas = s.list_children(Some(&db.object)).await.unwrap();
    let public = schemas.iter().find(|n| n.object.name == "PUBLIC").expect("PUBLIC listed");
    let rels = s.list_children(Some(&public.object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("ORDERS"), Some(NodeKind::Table));
    assert_eq!(kind("PAID_ORDERS"), Some(NodeKind::View));
    assert!(s.search("ORD", 10).await.unwrap().iter().any(|n| n.object.name == "ORDERS"));
    let st = s.structure(&table(&database, "ORDERS")).await.unwrap();
    assert_eq!(st.row_identity, ["ID"]);
    let total = st.column("TOTAL").unwrap();
    assert!(total.numeric && !total.nullable && total.data_type == "NUMBER(12,2)", "{total:?}");
    assert!(s.ddl(&table(&database, "ORDERS")).await.unwrap().contains("CREATE"));
    let model = s.schema_model(&database, Some("PUBLIC")).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "ORDERS" && t.columns.iter().any(|c| c.primary_key)));
    assert_eq!(s.structure(&table(&database, "MISSING")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn table_pages_edits_and_exports() -> DbResult<()> {
    let Some((s, database)) = seeded("EDITS").await else { return Ok(()) };
    let orders = table(&database, "ORDERS");
    let req = TablePageRequest {
        object: orders.clone(),
        rows: RowFilter { filter: Some("STATUS = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "TOTAL".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await?;
    assert_eq!((page.rows.len(), page.has_more), (100, true));
    assert_eq!(ops::row_count(&*s, &orders, &req.rows).await?.count, 400);
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("ID", Some("1"))], values: vec![cv("TOTAL", Some("84.50")), cv("NOTES", Some("Zoë ✓"))] },
        RowChange::Insert { values: vec![cv("ID", Some("5000")), cv("STATUS", Some("paid")), cv("TOTAL", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("ID", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &orders, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT TOTAL, NOTES FROM PUBLIC.ORDERS WHERE ID = 1", &[], OPTS).await?;
    assert_eq!((text(&r.rows[0][0]).as_str(), text(&r.rows[0][1]).as_str()), ("84.50", "Zoë ✓"));
    let failing = vec![RowChange::Delete { key: vec![cv("ID", Some("909090"))] }];
    assert_eq!(ops::apply_changes(&*s, &orders, &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: orders, rows: RowFilter { filter: Some("ID <= 10".into()), ..RowFilter::default() }, sort: None };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &TransferControl::none()).await?.rows, 9);
    Ok(())
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let Some(mut cfg) = config("DQ_RO") else { return };
    cfg.read_only = true;
    let s = SnowflakeAdapter.connect(&ConnectParams::new(cfg, Some("x".into()))).await.unwrap();
    assert!(s.execute("SELECT 1", &[], OPTS).await.is_ok());
    assert_eq!(s.execute("CREATE DATABASE NOPE", &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
}
