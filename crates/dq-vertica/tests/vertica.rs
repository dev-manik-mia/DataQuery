//! Integration tests against a real Vertica server (Community Edition).
//!
//! Start it with `docker compose -p dataquery --profile vertica up -d vertica`
//! (it takes about two minutes to create its database) and run
//! `DATAQUERY_TEST_VERTICA=localhost:55443 cargo test -p dq-vertica`, or
//! `npm run test:integration:vertica`. Without the variable every test is skipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::table::{ColumnSearch, RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_vertica::VerticaAdapter;

const DB: &str = "vdb";
const SCHEMA: &str = "dataquery_test";
const PASSWORD: &str = "dataquery123";
const ADMIN_PASSWORD: &str = "dataquery";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 60 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_VERTICA").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "5433"));
    Some(ConnConfig {
        engine: Engine::Vertica,
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
                eprintln!("skipped: set DATAQUERY_TEST_VERTICA to run Vertica integration tests");
                return;
            }
        }
    };
}

fn params(cfg: &ConnConfig, password: &str) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some(password.into()))
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    VerticaAdapter.connect(&params(cfg, PASSWORD)).await.expect("connect")
}

async fn admin(cfg: &ConnConfig) -> Arc<dyn Session> {
    let cfg = ConnConfig { username: "dbadmin".into(), ..cfg.clone() };
    VerticaAdapter.connect(&params(&cfg, ADMIN_PASSWORD)).await.expect("connect as dbadmin")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, Some(SCHEMA.into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

async fn scalar(s: &dyn Session, sql: &str) -> String {
    text(&s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}")).rows[0][0])
}

#[tokio::test]
async fn connects_authenticates_and_reports_errors() {
    let cfg = require_db!();
    let info = VerticaAdapter.test(&params(&cfg, PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("Vertica "), "{}", info.version);
    assert_eq!((info.database.as_str(), info.user.as_str(), info.engine), (DB, "dataquery", Engine::Vertica));
    let secret = "wrong-Secret-123";
    let err = VerticaAdapter.test(&params(&cfg, secret)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(!format!("{err:?}").contains(secret));
    let err = VerticaAdapter.test(&params(&ConnConfig { port: 1, ..cfg.clone() }, PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
    // The server has no TLS certificate: `require` refuses, `prefer` falls back to plain text.
    let err = VerticaAdapter.test(&params(&ConnConfig { ssl_mode: SslMode::Require, ..cfg.clone() }, PASSWORD)).await;
    assert_eq!(err.unwrap_err().kind, ErrorKind::Tls);
    let info = VerticaAdapter.test(&params(&ConnConfig { ssl_mode: SslMode::Prefer, ..cfg.clone() }, PASSWORD)).await;
    assert!(info.unwrap().tls.is_none());

    // SHA-512 (the default) and MD5 password hashes.
    let a = admin(&cfg).await;
    for (user, algorithm) in [("it_sha", "SHA512"), ("it_md5", "MD5")] {
        a.execute(&format!("DROP USER IF EXISTS {user}"), &[], OPTS).await.unwrap();
        a.execute(&format!("CREATE USER {user} IDENTIFIED BY 'pw-{user}-1'"), &[], OPTS).await.unwrap();
        a.execute(
            &format!("ALTER USER {user} SECURITY_ALGORITHM '{algorithm}' IDENTIFIED BY 'pw-{user}-2'"),
            &[],
            OPTS,
        )
        .await
        .unwrap();
        let user_cfg = ConnConfig { username: user.into(), ..cfg.clone() };
        let info = VerticaAdapter.test(&params(&user_cfg, &format!("pw-{user}-2"))).await;
        assert_eq!(info.unwrap_or_else(|e| panic!("{algorithm}: {e:?}")).user, user);
        a.execute(&format!("DROP USER {user}"), &[], OPTS).await.unwrap();
    }

    let s = session(&cfg).await;
    let err = s.execute("SELECT nope FROM dataquery_test.orders", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax, "{err:?}");
    assert!(err.code.is_some() && !err.message.is_empty());
    let err = s.execute("SELECT * FROM dataquery_test.missing_table", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound, "{err:?}");
    let err = s.execute("SELEC 1", &[], OPTS).await.unwrap_err();
    assert_eq!((err.kind, err.position), (ErrorKind::Syntax, Some(1)), "{err:?}");
    // The connection is usable after errors.
    assert_eq!(scalar(&*s, "SELECT 1").await, "1");
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert_eq!(dbs.len(), 1);
    assert!(dbs[0].current && dbs[0].object.name == DB);
    let schemas = s.list_children(Some(&dbs[0].object)).await.unwrap();
    assert_eq!(schemas[0].object.name, "public");
    assert!(schemas.iter().any(|n| n.object.name == SCHEMA));
    assert!(!schemas.iter().any(|n| n.object.name == "v_catalog"));
    let schema = schemas.iter().find(|n| n.object.name == SCHEMA).unwrap();
    let rels = s.list_children(Some(&schema.object)).await.unwrap();
    let node = |n: &str| rels.iter().find(|r| r.object.name == n).unwrap_or_else(|| panic!("{n} in {rels:?}"));
    assert_eq!(node("orders").object.kind, NodeKind::Table);
    assert_eq!(node("paid_orders").object.kind, NodeKind::View);
    assert_eq!(node("orders").row_estimate, Some(1200));
    assert_eq!(node("customers").comment.as_deref(), Some("People who order"));
    let sequences = node("sequences").object.clone();
    let children = s.list_children(Some(&sequences)).await.unwrap();
    assert_eq!(children.iter().map(|c| c.object.name.as_str()).collect::<Vec<_>>(), ["invoice_numbers"]);
    assert!(s.ddl(&children[0].object).await.unwrap().contains("CREATE SEQUENCE"));
    assert!(s.search("ORDER", 10).await.unwrap().iter().any(|n| n.object.name == "orders"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT big, exact, ratio, flag, day, at, at_tz, ts, ts_tz, span, label, doc, raw, uid, tags, shape, \
                    ST_AsText(shape) FROM dataquery_test.type_samples ORDER BY id",
            &[],
            OPTS,
        )
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[2], Cell::number("1.5"));
    assert_eq!(row[3], Cell::Bool { value: true });
    assert_eq!(row[4], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
    assert_eq!(row[5], Cell::Temporal { value: "09:59:00.123".into(), zoned: false });
    assert!(
        matches!(&row[6], Cell::Temporal { zoned: true, value } if value.starts_with("09:59:00") || value.starts_with("07:59:00")),
        "{:?}",
        row[6]
    );
    assert_eq!(row[7], Cell::Temporal { value: "2026-10-14 09:59:00.123456".into(), zoned: false });
    let Cell::Temporal { value, zoned: true } = &row[8] else { panic!("{:?}", row[8]) };
    assert!(chrono::DateTime::parse_from_rfc3339(value).is_ok(), "{value}");
    assert_eq!(text(&row[9]), "1 02:03:04");
    assert_eq!(text(&row[10]), "ab  ");
    assert_eq!(text(&row[11]), "Zoë ✓");
    assert_eq!(row[12], Cell::Binary { length: 4, head_hex: "89504e47".into() });
    assert_eq!(text(&row[13]), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(row[14].to_json(), serde_json::json!(["a", "b c"]));
    assert!(matches!(row[15], Cell::Binary { .. }), "{:?}", row[15]);
    assert_eq!(text(&row[16]), "POINT (1 2)");
    assert!(r.rows[1][1..].iter().all(|c| *c == Cell::Null));
    assert!(r.columns[0].numeric && r.columns[1].numeric && !r.columns[10].numeric);
    assert_eq!((r.columns[1].type_name.as_str(), r.columns[15].type_name.as_str()), ("numeric", "geometry"));
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let many = "SELECT a.ordinal_position FROM v_catalog.system_columns a CROSS JOIN v_catalog.system_columns b";
    let r = s.execute(many, &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!((page.rows.len(), page.has_more), (500, true));
    let started = Instant::now();
    assert_eq!(scalar(&*s, "SELECT 1").await, "1", "a new statement closes the open result");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(s.fetch_more(10).await.is_err());

    let slow = "SELECT COUNT(*) FROM v_catalog.system_columns a, v_catalog.system_columns b, v_catalog.system_columns c, v_catalog.system_columns d";
    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(cancelled_at.elapsed() < Duration::from_secs(10));
    let err = s.execute(slow, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert_eq!(scalar(&*s, "SELECT 2").await, "2");
    assert!(!s.cancel().await.unwrap(), "nothing runs");
}

#[tokio::test]
async fn parameters_writes_and_transactions() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT total, ?::VARCHAR IS NULL, ?::VARCHAR, ?::INT + 1 FROM dataquery_test.orders WHERE id = ?",
            &[None, Some("it's; DROP".into()), Some("41".into()), Some("7".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!((text(&r.rows[0][1]).as_str(), text(&r.rows[0][2]).as_str()), ("true", "it's; DROP"));
    assert_eq!(text(&r.rows[0][3]), "42");
    let err = s.execute("SELECT ?", &[], OPTS).await.unwrap_err();
    assert!(err.message.contains('?') || err.kind == ErrorKind::Syntax, "{err:?}");
    let err = s.execute("SELECT ?::INT", &[Some("1".into()), Some("2".into())], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput, "{err:?}");

    s.execute("DROP TABLE IF EXISTS dataquery_test.it_writes", &[], OPTS).await.unwrap();
    let r = s
        .execute("CREATE TABLE dataquery_test.it_writes (id INT, note VARCHAR(20), raw VARBINARY(8))", &[], OPTS)
        .await
        .unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("CREATE", None));
    let r = s
        .execute(
            "INSERT INTO dataquery_test.it_writes VALUES (?, ?, ?)",
            &[Some("1".into()), Some("a\\b".into()), Some("\\x5c00ff".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!((r.command.as_str(), r.affected, r.columns.len()), ("INSERT", Some(1), 0));
    assert_eq!(scalar(&*s, "SELECT TO_HEX(raw) || note FROM dataquery_test.it_writes").await, "5c00ffa\\b");
    // Autocommit is on; BEGIN opens a transaction that ROLLBACK undoes.
    s.execute("BEGIN", &[], OPTS).await.unwrap();
    s.execute("INSERT INTO dataquery_test.it_writes (id) VALUES (2)", &[], OPTS).await.unwrap();
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM dataquery_test.it_writes").await, "1");
    let r = s.execute("UPDATE dataquery_test.it_writes SET note = 'x'", &[], OPTS).await.unwrap();
    assert_eq!(r.affected, Some(1));
    s.execute("DROP TABLE dataquery_test.it_writes", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn read_only_profiles_are_enforced_by_the_server() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM dataquery_test.orders").await, "1200");
    let err = s.execute("INSERT INTO dataquery_test.no_key VALUES (9, 'ro')", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    // Catalog reads never write, whatever the profile says.
    let rw = session(&ConnConfig { read_only: false, ..cfg }).await;
    let err = rw
        .read_query(DB, "DELETE FROM dataquery_test.no_key", &[], dq_core::value::CellLimits::GRID)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
}

#[tokio::test]
async fn structure_ddl_dependencies_and_model() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert!(st.editable());
    let id = st.column("id").unwrap();
    assert!(id.identity && id.generated && id.primary_key, "{id:?}");
    let total = st.column("total").unwrap();
    assert!(total.numeric && !total.nullable && total.data_type == "numeric(12,2)", "{total:?}");
    assert_eq!(st.column("status").unwrap().default.as_deref(), Some("'pending'"));
    let kinds: Vec<_> = st.constraints.iter().map(|c| (c.kind, c.definition.as_str())).collect();
    assert!(
        kinds.iter().any(|(_, d)| *d == "FOREIGN KEY (customer_id) REFERENCES dataquery_test.customers (id)"),
        "{kinds:?}"
    );
    assert!(kinds.iter().any(|(_, d)| *d == "UNIQUE (customer_id, id)"), "{kinds:?}");
    assert!(!st.indexes.is_empty() && st.indexes[0].definition.contains("projection"), "{:?}", st.indexes);
    assert!(st.size.is_some_and(|s| s.total_bytes > 0));
    assert_eq!(st.foreign_keys[0].target.name, "customers");
    let ddl = s.ddl(&table("orders")).await.unwrap();
    assert!(ddl.contains("CREATE TABLE") && ddl.contains("PROJECTION"), "{ddl}");
    let view = ObjectRef { kind: NodeKind::View, ..table("paid_orders") };
    assert!(s.ddl(&view).await.unwrap().contains("VIEW dataquery_test.paid_orders AS"));
    let view_st = s.structure(&view).await.unwrap();
    assert!(view_st.row_identity.is_empty() && view_st.column("total").is_some());

    let deps = s.dependencies(&table("orders")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "customers"), "{deps:?}");
    assert!(deps.used_by.iter().any(|d| d.object.name == "paid_orders"), "{deps:?}");
    let deps = s.dependencies(&table("customers")).await.unwrap();
    assert!(deps.used_by.iter().any(|d| d.object.name == "orders"), "{deps:?}");
    assert!(s.dependencies(&view).await.unwrap().uses.iter().any(|d| d.object.name == "orders"));

    let model = s.schema_model(DB, Some(SCHEMA)).await.unwrap();
    let orders = model.tables.iter().find(|t| t.object.name == "orders").unwrap();
    assert!(orders.columns.iter().any(|c| c.name == "id" && c.primary_key));
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "orders" && f.target.name == "customers"));
    assert!(s.schema_model(DB, None).await.unwrap().tables.len() >= model.tables.len());
    assert_eq!(s.structure(&table("missing")).await.unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(s.row_estimate(&table("customers")).await.unwrap(), Some(50));
}

#[tokio::test]
async fn pages_edits_and_transfers() -> DbResult<()> {
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
    let literal = RowFilter {
        searches: vec![ColumnSearch { column: "status".into(), contains: "_aid".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &literal).await?.count, 0);
    let nulls =
        RowFilter { matches: vec![ColumnValue { column: "notes".into(), value: None }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &nulls).await?.count, 300);
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
    .await?;
    let shape = samples.columns.iter().position(|c| c.name == "shape").unwrap();
    assert_eq!(text(&samples.rows[0][shape]), "POINT (1 2)", "spatial columns read as WKT");

    // Grid edits: one transaction, rows matched by the primary key.
    s.execute("DROP TABLE IF EXISTS dataquery_test.it_edits", &[], OPTS).await?;
    s.execute("CREATE TABLE dataquery_test.it_edits (id INT PRIMARY KEY ENABLED, amount NUMERIC(12,2), note VARCHAR(20), flag BOOLEAN, at TIMESTAMPTZ)", &[], OPTS).await?;
    s.execute(
        "INSERT INTO dataquery_test.it_edits VALUES (9007199254740993, 1.00, 'a', true, '2026-01-01 10:00:00+00')",
        &[],
        OPTS,
    )
    .await?;
    s.execute("INSERT INTO dataquery_test.it_edits VALUES (2, 2.00, 'b', false, NULL)", &[], OPTS).await?;
    let t = table("it_edits");
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("id", Some("9007199254740993"))],
            values: vec![
                cv("amount", Some("84.50")),
                cv("note", Some("")),
                cv("flag", Some("false")),
                cv("at", Some("2026-02-03T04:05:06+00:00")),
            ],
        },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("amount", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &t, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT id, amount, note, flag FROM dataquery_test.it_edits ORDER BY id", &[], OPTS).await?;
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(rows, [["3", "NULL", "NULL", "NULL"], ["9007199254740993", "84.50", "", "false"]]);
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("note", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    assert_eq!(ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);
    assert_eq!(scalar(&*s, "SELECT note FROM dataquery_test.it_edits WHERE id = 3").await, "NULL", "rolled back");

    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table {
        object: table("orders"),
        rows: RowFilter { filter: Some("id <= 700".into()), ..RowFilter::default() },
        sort: None,
    };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 700);
    s.execute("DROP TABLE IF EXISTS dataquery_test.it_import", &[], OPTS).await?;
    s.execute("CREATE TABLE dataquery_test.it_import (id INT, status VARCHAR(20), total NUMERIC(12,2))", &[], OPTS)
        .await?;
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
        700
    );
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
    s.execute("TRUNCATE TABLE dataquery_test.it_import", &[], OPTS).await?;
    let script = std::fs::read_to_string(&sql_file).unwrap();
    for statement in dq_core::sql::split_statements(&script, s.dialect().flavor()) {
        s.execute(statement.text(&script), &[], OPTS).await?;
    }
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM dataquery_test.it_import").await, "700");
    assert_eq!(
        scalar(&*s, "SELECT (SELECT SUM(total) FROM dataquery_test.it_import) = (SELECT SUM(total) FROM dataquery_test.orders WHERE id <= 700)").await,
        "true"
    );
    s.execute("DROP TABLE dataquery_test.it_import", &[], OPTS).await?;
    s.execute("DROP TABLE dataquery_test.it_edits", &[], OPTS).await?;
    Ok(())
}

#[tokio::test]
async fn explain_monitoring_and_schema_changes() {
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    let plan = s.explain("SELECT * FROM dataquery_test.orders WHERE id = 7;", &[], false, 30).await.unwrap();
    assert!(plan.plan.is_none() && plan.raw.contains("PLAN"), "{}", plan.raw);
    assert_eq!(s.explain("SELECT 1", &[], true, 30).await.unwrap_err().kind, ErrorKind::Unsupported);
    let with_params = s.explain("SELECT ?::INT", &[Some("7".into())], false, 30).await;
    assert_eq!(with_params.unwrap_err().kind, ErrorKind::Unsupported);

    let runner = session(&cfg).await;
    let watcher = session(&cfg).await;
    let slow = "SELECT COUNT(*) FROM v_catalog.system_columns a, v_catalog.system_columns b, v_catalog.system_columns c, v_catalog.system_columns d";
    let handle = tokio::spawn({
        let runner = runner.clone();
        async move { runner.execute(slow, &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("vertica monitors");
    let deadline = Instant::now() + Duration::from_secs(15);
    let find = |marker: &'static str| async move {
        loop {
            let snapshot = monitor.snapshot(0).await.unwrap();
            assert!(snapshot.connections.is_some_and(|g| g.max > 0), "{:?}", snapshot.connections);
            if let Some(p) = snapshot.processes.iter().find(|p| p.query.as_deref().is_some_and(|q| q.contains(marker)))
            {
                assert!(p.active && p.user.as_deref() == Some("dataquery"), "{p:?}");
                return p.pid;
            }
            assert!(Instant::now() < deadline, "the slow query never showed up");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    assert!(monitor.kill(find("system_columns d").await, false).await.unwrap());
    assert_eq!(handle.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    assert_eq!(scalar(&*runner, "SELECT 1").await, "1", "interrupting keeps the session");
    let ended = "SELECT COUNT(*) FROM v_catalog.system_columns a, v_catalog.system_columns b, v_catalog.system_columns c, v_catalog.system_columns e";
    let handle = tokio::spawn({
        let runner = runner.clone();
        async move { runner.execute(ended, &[], OPTS).await }
    });
    assert!(monitor.kill(find("system_columns e").await, true).await.unwrap());
    assert!(handle.await.unwrap().is_err());
    assert!(runner.execute("SELECT 1", &[], OPTS).await.is_err(), "the session was closed");
    assert!(
        monitor.server_settings(Some("MaxClientSessions")).await.unwrap().iter().any(|s| s.name == "MaxClientSessions")
    );
    assert!(monitor.largest_tables(DB, 5).await.unwrap().iter().any(|t| t.object.name == "orders"));

    s.execute("DROP TABLE IF EXISTS dataquery_test.it_accounts CASCADE", &[], OPTS).await.unwrap();
    let col = |name: &str, ty: &str| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        default: None,
        identity: false,
    };
    let changes = vec![
        SchemaChange::CreateTable {
            table: table("it_accounts"),
            columns: vec![
                ColumnDef { nullable: false, identity: true, ..col("id", "INT") },
                col("email", "VARCHAR(80)"),
            ],
            primary_key: vec!["id".into()],
        },
        SchemaChange::AddColumn {
            table: table("it_accounts"),
            column: ColumnDef { default: Some("0".into()), nullable: false, ..col("qty", "INT") },
        },
        SchemaChange::RenameColumn { table: table("it_accounts"), column: "qty".into(), new_name: "quantity".into() },
        SchemaChange::AlterColumn {
            table: table("it_accounts"),
            column: "email".into(),
            data_type: Some("VARCHAR(200)".into()),
            nullable: Some(false),
            default: Some(DefaultChange::Set { expression: "'none'".into() }),
        },
        SchemaChange::DuplicateTable {
            table: table("it_accounts"),
            new_name: "it_accounts_copy".into(),
            with_data: true,
        },
        SchemaChange::RenameTable { table: table("it_accounts_copy"), new_name: "it_accounts_2".into() },
        SchemaChange::DropTable { table: table("it_accounts_2"), cascade: true },
        SchemaChange::TruncateTable {
            table: table("it_accounts"),
            restart_identity: false,
            cascade: false,
            disable_foreign_keys: false,
        },
    ];
    for change in &changes {
        for statement in ops::schema_statements(&*s, std::slice::from_ref(change)).await.unwrap() {
            s.execute(&statement, &[], OPTS).await.unwrap_or_else(|e| panic!("{statement}: {e:?}"));
        }
    }
    let structure = ops::structure(&*s, &table("it_accounts")).await.unwrap();
    let email = structure.column("email").unwrap();
    assert_eq!(
        (email.data_type.as_str(), email.default.as_deref(), email.nullable),
        ("varchar(200)", Some("'none'"), false)
    );
    assert!(structure.column("id").unwrap().identity);
    let index =
        SchemaChange::CreateIndex { table: table("it_accounts"), name: "i".into(), columns: Vec::new(), unique: false };
    assert_eq!(ddl::statements(s.dialect(), &index, None).unwrap_err().kind, ErrorKind::Unsupported);
    s.execute("DROP TABLE dataquery_test.it_accounts", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn schema_diff_round_trip() {
    use dq_core::diff::{Container, diff};

    let cfg = require_db!();
    let s = session(&cfg).await;
    for sql in [
        "DROP SCHEMA IF EXISTS it_diff_a CASCADE",
        "DROP SCHEMA IF EXISTS it_diff_b CASCADE",
        "CREATE SCHEMA it_diff_a",
        "CREATE SCHEMA it_diff_b",
        "CREATE TABLE it_diff_a.accounts (id INT NOT NULL PRIMARY KEY, email VARCHAR(80) NOT NULL, qty INT DEFAULT 1)",
        "CREATE TABLE it_diff_a.events (day DATE NOT NULL, kind VARCHAR(10))",
        "CREATE TABLE it_diff_b.accounts (id INT NOT NULL PRIMARY KEY, email VARCHAR(40), legacy VARCHAR(5))",
        "CREATE TABLE it_diff_b.stale (id INT)",
    ] {
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let snapshot = |schema: &'static str| {
        let s = s.clone();
        async move {
            let model = s.schema_model(DB, Some(schema)).await.unwrap();
            let mut tables = Vec::new();
            for t in model.tables.into_iter().filter(|t| t.object.kind == NodeKind::Table) {
                tables.push(ops::structure(&*s, &t.object).await.unwrap().into());
            }
            tables
        }
    };
    let a = Container { database: DB.into(), schema: Some("it_diff_a".into()) };
    let b = Container { database: DB.into(), schema: Some("it_diff_b".into()) };
    let plan = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(plan.sql.contains("CREATE TABLE") && plan.sql.contains("DROP TABLE"), "{}", plan.sql);
    for statement in dq_core::sql::split_statements(&plan.sql, s.dialect().flavor()) {
        let sql = statement.text(&plan.sql);
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let again = diff(s.dialect(), &snapshot("it_diff_a").await, &a, &snapshot("it_diff_b").await, &b).unwrap();
    assert!(again.sql.is_empty(), "{:#?}\n{}\n{:?}", again.items, again.sql, again.warnings);
    s.execute("DROP SCHEMA it_diff_a CASCADE", &[], OPTS).await.unwrap();
    s.execute("DROP SCHEMA it_diff_b CASCADE", &[], OPTS).await.unwrap();
}
