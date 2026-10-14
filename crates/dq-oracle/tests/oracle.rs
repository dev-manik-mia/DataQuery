//! Integration tests against a real Oracle database.
//!
//! Start it with `docker compose -p dataquery up -d oracle` and run
//! `DATAQUERY_TEST_ORACLE=localhost:51521 cargo test -p dq-oracle`. The
//! Oracle client library must be installed (see the README). Without the
//! variable every test is skipped.

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
use dq_core::transfer::{CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_oracle::OracleAdapter;

const PASSWORD: &str = "dataquery";
const PDB: &str = "FREEPDB1";
const SCHEMA: &str = "DATAQUERY";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const ADAPTER: OracleAdapter = OracleAdapter;

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_ORACLE").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "1521"));
    Some(ConnConfig {
        engine: Engine::Oracle,
        host: host.into(),
        port: port.parse().unwrap(),
        database: PDB.into(),
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
                eprintln!("skipped: set DATAQUERY_TEST_ORACLE to run Oracle integration tests");
                return;
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ADAPTER.connect(&params(cfg, PASSWORD)).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, PDB, Some(SCHEMA.into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Oracle) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// Drops a table if it exists, then runs `ddl`.
async fn scratch(s: &dyn Session, name: &str, ddl: &str) -> ObjectRef {
    let _ = s.execute(&format!("DROP TABLE {name} PURGE"), &[], OPTS).await;
    s.execute(&format!("CREATE TABLE {name} {ddl}"), &[], OPTS).await.unwrap();
    table(name)
}

#[tokio::test]
async fn test_reports_server_details() {
    let cfg = require_db!();
    let info = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap();
    assert!(info.version.starts_with("Oracle 23."), "{}", info.version);
    assert_eq!((info.engine, info.database.as_str(), info.user.as_str()), (Engine::Oracle, PDB, SCHEMA));
}

#[tokio::test]
async fn invalid_credentials_and_service_are_actionable() {
    let mut cfg = require_db!();
    let secret = "wrong-Secret-123";
    let err = ADAPTER.test(&params(&cfg, secret)).await.unwrap_err();
    assert_eq!((err.kind, err.code.as_deref()), (ErrorKind::Auth, Some("ORA-01017")));
    assert!(!format!("{err:?}").contains(secret));
    cfg.database = "NO_SUCH_SERVICE".into();
    let err = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound, "{err:?}");
}

#[tokio::test]
async fn network_failure_is_classified() {
    let mut cfg = require_db!();
    cfg.port = 1;
    let err = ADAPTER.test(&params(&cfg, PASSWORD)).await.unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Network | ErrorKind::Timeout), "{err:?}");
}

#[tokio::test]
async fn explorer_lists_lazily() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await.unwrap();
    assert_eq!(dbs.len(), 1);
    let schemas = s.list_children(Some(&dbs[0].object)).await.unwrap();
    let names: Vec<_> = schemas.iter().map(|n| n.object.name.as_str()).collect();
    assert_eq!(names[0], SCHEMA, "the user's own schema comes first");
    assert!(names.contains(&"BILLING") && !names.contains(&"SYS") && !names.contains(&"SYSTEM"), "{names:?}");
    let rels = s.list_children(Some(&schemas[0].object)).await.unwrap();
    let kind = |n: &str| rels.iter().find(|r| r.object.name == n).map(|r| r.object.kind);
    assert_eq!(kind("ORDERS"), Some(NodeKind::Table));
    assert_eq!(kind("PAID_ORDERS"), Some(NodeKind::View));
    for group in ["sequences", "functions", "procedures"] {
        let node = rels.iter().find(|r| r.object.kind == NodeKind::Group && r.object.name == group).unwrap_or_else(|| panic!("{group}"));
        let children = s.list_children(Some(&node.object)).await.unwrap();
        assert!(!children.is_empty(), "{group}");
        assert!(children.iter().all(|c| !c.object.name.starts_with("ISEQ$$")), "identity sequences are hidden");
        assert!(s.ddl(&children[0].object).await.unwrap().contains("CREATE"), "{group}");
    }
    let orders = rels.iter().find(|r| r.object.name == "ORDERS").unwrap();
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    let names: Vec<_> = children.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect();
    assert!(names.contains(&(NodeKind::Index, "ORDERS_CREATED_AT_IDX")) && names.contains(&(NodeKind::Trigger, "ORDERS_TOUCH")), "{names:?}");
    let found = s.search("order", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "ORDERS"));
}

#[tokio::test]
async fn value_rendering_contract() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute("SELECT big, exact, ts, day, doc, bin, label, ratio, INTERVAL '1 02:03:04' DAY TO SECOND FROM type_samples ORDER BY id", &[], OPTS)
        .await
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], Cell::number("9223372036854775807"));
    assert_eq!(row[1], Cell::number("1234567890123456789012345678.0123456789"));
    assert_eq!(row[2], Cell::Temporal { value: "2026-10-14 09:59:00.123 +02:00".into(), zoned: true });
    assert_eq!(row[3], Cell::Temporal { value: "2026-10-14 00:00:00".into(), zoned: false });
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(&row[4])).unwrap()["name"], "Zoë ✓");
    assert_eq!(row[5], Cell::Binary { length: 8, head_hex: "89504e470d0a1a0a".into() });
    assert_eq!(text(&row[6]), "Zoë");
    assert_eq!(row[7], Cell::number("1.5"));
    assert!(text(&row[8]).contains("02:03:04"), "{:?}", row[8]);
    let second = &r.rows[1];
    assert_eq!(second[1], Cell::number("-0.0000000001"));
    assert_eq!(second[0], Cell::Null);
    assert!(r.columns[0].numeric && !r.columns[6].numeric);
    assert_eq!(r.columns[1].type_name, "NUMBER(38,10)");
}

#[tokio::test]
async fn results_are_bounded_resumable_and_cancellable() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s.execute("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 1000000", &[], OPTS).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(500).await.unwrap();
    assert_eq!(text(&page.rows[0][0]), "501");
    let started = Instant::now();
    assert_eq!(text(&s.execute("SELECT 1 FROM dual", &[], OPTS).await.unwrap().rows[0][0]), "1");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(s.fetch_more(10).await.is_err());
    let r = s.execute("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 3", &[], ExecOptions { row_limit: 3, timeout_secs: 5 }).await.unwrap();
    assert_eq!((r.rows.len(), r.has_more), (3, false));

    let runner = s.clone();
    let handle = tokio::spawn(async move { runner.execute("BEGIN DBMS_SESSION.SLEEP(30); END;", &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    let cancelled_at = Instant::now();
    assert!(s.cancel().await.unwrap());
    let err = handle.await.unwrap().unwrap_err();
    assert!(cancelled_at.elapsed() < Duration::from_secs(5), "{:?}", cancelled_at.elapsed());
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    let started = Instant::now();
    let err = s.execute("BEGIN DBMS_SESSION.SLEEP(30); END;", &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    assert!(s.execute("SELECT 1 FROM dual", &[], OPTS).await.is_ok(), "the session keeps working");
}

#[tokio::test]
async fn errors_commits_and_transactions() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let err = s.execute("SELECT nope FROM dual", &[], OPTS).await.unwrap_err();
    assert_eq!((err.kind, err.code.as_deref()), (ErrorKind::Syntax, Some("ORA-00904")));
    assert!(err.position.is_some());
    let err = s.execute("SELECT * FROM missing_table", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
    assert!(s.execute("SELECT 1 FROM dual", &[], OPTS).await.is_ok(), "an error keeps the session");
    scratch(&*s, "IT_TX", "(id NUMBER PRIMARY KEY)").await;
    let r = s.execute("INSERT INTO it_tx SELECT LEVEL FROM dual CONNECT BY LEVEL <= 3", &[], OPTS).await.unwrap();
    assert_eq!((r.command.as_str(), r.affected), ("INSERT", Some(3)));
    // Outside a transaction a write is committed: another session sees it.
    let other = session(&cfg).await;
    assert_eq!(text(&other.execute("SELECT COUNT(*) FROM it_tx", &[], OPTS).await.unwrap().rows[0][0]), "3");
    // Inside one it waits for COMMIT or ROLLBACK.
    s.execute("SET TRANSACTION READ WRITE", &[], OPTS).await.unwrap();
    s.execute("DELETE FROM it_tx", &[], OPTS).await.unwrap();
    assert_eq!(text(&other.execute("SELECT COUNT(*) FROM it_tx", &[], OPTS).await.unwrap().rows[0][0]), "3");
    s.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert_eq!(text(&s.execute("SELECT COUNT(*) FROM it_tx", &[], OPTS).await.unwrap().rows[0][0]), "3");
    // PL/SQL blocks keep their own `;` and run as one statement.
    run_all(&*s, "BEGIN\n  INSERT INTO it_tx VALUES (10);\n  INSERT INTO it_tx VALUES (11);\nEND;\n/\nSELECT 1 FROM dual;").await;
    assert_eq!(text(&other.execute("SELECT COUNT(*) FROM it_tx", &[], OPTS).await.unwrap().rows[0][0]), "5");
    s.execute("DROP TABLE it_tx PURGE", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn structure_and_ddl() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let st = s.structure(&table("ORDERS")).await.unwrap();
    assert_eq!(st.row_identity, ["ID"]);
    let id = &st.columns[0];
    assert!(id.primary_key && id.identity && id.generated && !id.editable);
    let status = st.column("STATUS").unwrap();
    assert_eq!((status.data_type.as_str(), status.default.as_deref(), status.nullable), ("VARCHAR2(20)", Some("'pending'"), false));
    let total = st.column("TOTAL").unwrap();
    assert!(total.numeric && total.data_type == "NUMBER(12,2)");
    let fk = st.constraints.iter().find(|c| c.name == "ORDERS_CUSTOMER_FK").expect("fk");
    assert_eq!(fk.definition, "FOREIGN KEY (CUSTOMER_ID) REFERENCES CUSTOMERS (ID)");
    assert!(st.constraints.iter().any(|c| c.name == "ORDERS_STATUS_CHECK"));
    assert!(!st.constraints.iter().any(|c| c.definition.contains("IS NOT NULL")), "{:?}", st.constraints);
    let created = st.indexes.iter().find(|i| i.name == "ORDERS_CREATED_AT_IDX").unwrap();
    assert!(created.definition.contains("DESC") && created.columns == ["CREATED_AT"], "{created:?}");
    assert!(st.indexes.iter().any(|i| i.primary));
    assert!(st.size.is_some());
    assert_eq!(st.foreign_keys[0].target.name, "CUSTOMERS");
    assert!(s.structure(&table("NO_PK")).await.unwrap().row_identity.is_empty());
    let ddl = s.ddl(&table("ORDERS")).await.unwrap();
    assert!(ddl.contains("CREATE TABLE \"DATAQUERY\".\"ORDERS\""), "{ddl}");
    assert!(!ddl.contains("STORAGE("), "storage clauses are left out: {ddl}");
    let view = ObjectRef { kind: NodeKind::View, ..table("PAID_ORDERS") };
    assert!(s.ddl(&view).await.unwrap().contains("VIEW"));
    let deps = s.dependencies(&table("ORDERS")).await.unwrap();
    assert!(deps.uses.iter().any(|d| d.object.name == "CUSTOMERS"));
    assert!(deps.used_by.iter().any(|d| d.object.name == "PAID_ORDERS"));
    assert!(deps.used_by.iter().any(|d| d.object.kind == NodeKind::Trigger));
    let model = s.schema_model(PDB, Some(SCHEMA)).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "ORDERS" && t.columns.iter().any(|c| c.primary_key)));
    assert!(model.foreign_keys.iter().any(|f| f.source.name == "ORDERS" && f.target.name == "CUSTOMERS"));
    assert_eq!(s.structure(&table("MISSING_TABLE")).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn table_pages_filters_and_searches() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let req = TablePageRequest {
        object: table("ORDERS"),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "TOTAL".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await.unwrap();
    assert_eq!((page.rows.len(), page.has_more), (100, true));
    let next = ops::table_page(&*s, &TablePageRequest { offset: 390, ..req.clone() }).await.unwrap();
    assert_eq!((next.rows.len(), next.has_more), (10, false));
    assert_eq!(ops::row_count(&*s, &req.object, &req.rows).await.unwrap().count, 400);
    let searches = RowFilter {
        matches: vec![ColumnValue { column: "STATUS".into(), value: Some("paid".into()) }],
        searches: vec![ColumnSearch { column: "STATUS".into(), contains: "PAI".into() }],
        ..RowFilter::default()
    };
    assert_eq!(ops::row_count(&*s, &req.object, &searches).await.unwrap().count, 400);
    let literal = RowFilter { searches: vec![ColumnSearch { column: "STATUS".into(), contains: "_aid".into() }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &literal).await.unwrap().count, 0, "`_` is a literal");
    let id = RowFilter { matches: vec![ColumnValue { column: "ID".into(), value: Some("7".into()) }], ..RowFilter::default() };
    assert_eq!(ops::row_count(&*s, &req.object, &id).await.unwrap().count, 1);
    let bad = TablePageRequest { rows: RowFilter { filter: Some("1=1; DELETE FROM orders".into()), ..RowFilter::default() }, ..req.clone() };
    assert!(ops::table_page(&*s, &bad).await.is_err());
    let other = ObjectRef::new(NodeKind::Table, PDB, Some("BILLING".into()), "INVOICES");
    let page = ops::table_page(&*s, &TablePageRequest { object: other, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 }).await.unwrap();
    assert_eq!(page.columns[0].name, "ID");
}

#[tokio::test]
async fn edits_and_transfers_round_trip() -> DbResult<()> {
    let Some(cfg) = config() else { return Ok(()) };
    let s = session(&cfg).await;
    let t = scratch(&*s, "IT_EDITS", "(id NUMBER(19) PRIMARY KEY, amount NUMBER(12,2), note VARCHAR2(100), at TIMESTAMP(3), day DATE)").await;
    s.execute("INSERT INTO it_edits VALUES (9223372036854775807, 1, 'a', TIMESTAMP '2026-01-02 03:04:05.678', DATE '2026-01-02')", &[], OPTS).await?;
    s.execute("INSERT INTO it_edits VALUES (2, 2, 'b', NULL, NULL)", &[], OPTS).await?;
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update {
            key: vec![cv("ID", Some("9223372036854775807"))],
            values: vec![cv("AMOUNT", Some("84.50")), cv("NOTE", Some("Zoë ✓")), cv("AT", Some("2026-10-14 09:59:00.123")), cv("DAY", Some("2026-10-14 00:00:00"))],
        },
        RowChange::Insert { values: vec![cv("ID", Some("3")), cv("AMOUNT", None)] },
        RowChange::Delete { key: vec![cv("ID", Some("2"))] },
    ];
    let preview = ops::preview_changes(&*s, &t, &changes, false).await?;
    assert!(preview.contains("UPDATE \"DATAQUERY\".\"IT_EDITS\" SET \"AMOUNT\" = '84.50'"), "{preview}");
    let report = ops::apply_changes(&*s, &t, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let r = s.execute("SELECT id, amount, note, at, day FROM it_edits ORDER BY id", &[], OPTS).await?;
    let rows: Vec<Vec<String>> = r.rows.iter().map(|r| r.iter().map(text).collect()).collect();
    assert_eq!(rows, [
        vec!["3", "NULL", "NULL", "NULL", "NULL"],
        vec!["9223372036854775807", "84.5", "Zoë ✓", "2026-10-14 09:59:00.123", "2026-10-14 00:00:00"]
    ]);
    let failing = vec![
        RowChange::Update { key: vec![cv("ID", Some("3"))], values: vec![cv("NOTE", Some("x"))] },
        RowChange::Delete { key: vec![cv("ID", Some("404"))] },
    ];
    assert_eq!(ops::apply_changes(&*s, &t, &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);
    assert_eq!(s.execute("SELECT note FROM it_edits WHERE id = 3", &[], OPTS).await?.rows[0][0], Cell::Null);

    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: table("ORDERS"), rows: RowFilter { filter: Some("id <= 10".into()), ..RowFilter::default() }, sort: None };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 10);
    let imported = scratch(&*s, "IT_IMPORT", "(id NUMBER(19) PRIMARY KEY, status VARCHAR2(20), total NUMBER(12,2), bin RAW(8))").await;
    let csv_source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions { empty_as_null: true, ..CsvOptions::default() } };
    let map: Vec<ImportMapping> =
        [("ID", 0), ("STATUS", 2), ("TOTAL", 3)].iter().map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) }).collect();
    let report = ops::import(&*s, &imported, &csv_source, &map, &ImportOptions::default(), &none).await?;
    assert_eq!((report.rows, report.error_count), (10, 0));
    // SQL INSERT export runs back into the table it came from.
    s.execute("UPDATE it_import SET bin = HEXTORAW('0102FF') WHERE id = 1", &[], OPTS).await?;
    let sql_file = dir.path().join("rows.sql");
    ops::export(&*s, &ExportSource::Table { object: imported.clone(), rows: RowFilter::default(), sort: None }, ExportFormat::SqlInsert, &ExportOptions::default(), &sql_file, &none).await?;
    s.execute("DELETE FROM it_import", &[], OPTS).await?;
    run_all(&*s, &std::fs::read_to_string(&sql_file).unwrap()).await;
    let r = s.execute("SELECT COUNT(*), MAX(bin) FROM it_import", &[], OPTS).await?;
    assert_eq!(text(&r.rows[0][0]), "10");
    assert_eq!(r.rows[0][1], Cell::Binary { length: 3, head_hex: "0102ff".into() });
    run_all(&*s, "DROP TABLE it_edits PURGE; DROP TABLE it_import PURGE").await;
    Ok(())
}

#[tokio::test]
async fn parameters_are_bound() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let r = s
        .execute(
            "SELECT big, CASE WHEN :2 IS NULL THEN 1 ELSE 0 END, :3 FROM type_samples WHERE id = :1",
            &[Some("1".into()), None, Some("it's; DROP TABLE x".into())],
            OPTS,
        )
        .await
        .unwrap();
    assert_eq!(text(&r.rows[0][0]), "9223372036854775807");
    assert_eq!(text(&r.rows[0][1]), "1");
    assert_eq!(text(&r.rows[0][2]), "it's; DROP TABLE x");
}

#[tokio::test]
async fn read_only_profiles_cannot_write() {
    let mut cfg = require_db!();
    cfg.read_only = true;
    let s = session(&cfg).await;
    assert!(s.info().read_only);
    assert!(s.execute("SELECT 1 FROM dual", &[], OPTS).await.is_ok());
    let err = s.execute("INSERT INTO no_pk VALUES (99, 'ro')", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    let changes = vec![RowChange::Delete { key: vec![ColumnValue { column: "ID".into(), value: Some("1".into()) }] }];
    assert_eq!(ops::apply_changes(&*s, &table("ORDERS"), &changes, false).await.unwrap_err().kind, ErrorKind::Permission);
}

#[tokio::test]
async fn explain_reads_the_plan_table() {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let plan = s.explain("SELECT * FROM orders WHERE status = :1 ORDER BY total", &[Some("paid".into())], false, 30).await.unwrap();
    let root = plan.plan.expect("plan tree");
    assert_eq!(root.label, "SELECT STATEMENT");
    let mut stack = vec![&root];
    let mut found = false;
    while let Some(n) = stack.pop() {
        found |= n.detail.as_deref() == Some("DATAQUERY.ORDERS");
        stack.extend(n.children.iter());
    }
    assert!(found, "{root:#?}");
    assert!(plan.raw.contains("Plan hash value"), "{}", plan.raw);
    let analyzed = s.explain("SELECT COUNT(*) FROM orders", &[], true, 30).await.unwrap();
    assert!(analyzed.raw.contains("A-Rows"), "{}", analyzed.raw);
}

#[tokio::test]
async fn monitor_shows_blocking_and_cancels() {
    let cfg = require_db!();
    let holder = session(&cfg).await;
    let waiter = session(&cfg).await;
    let watcher = session(&cfg).await;
    scratch(&*holder, "IT_MONITOR", "(id NUMBER PRIMARY KEY, v NUMBER)").await;
    holder.execute("INSERT INTO it_monitor VALUES (1, 0)", &[], OPTS).await.unwrap();
    let sid = |s: Arc<dyn Session>| async move {
        text(&s.execute("SELECT SYS_CONTEXT('USERENV', 'SID') FROM dual", &[], OPTS).await.unwrap().rows[0][0]).parse::<i64>().unwrap()
    };
    let (holder_sid, waiter_sid) = (sid(holder.clone()).await, sid(waiter.clone()).await);
    holder.execute("SET TRANSACTION READ WRITE", &[], OPTS).await.unwrap();
    holder.execute("UPDATE it_monitor SET v = 1 WHERE id = 1", &[], OPTS).await.unwrap();
    let blocked = tokio::spawn({
        let waiter = waiter.clone();
        async move { waiter.execute("UPDATE it_monitor SET v = 2 WHERE id = 1", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("oracle monitors");
    let deadline = Instant::now() + Duration::from_secs(15);
    let snapshot = loop {
        let snapshot = monitor.snapshot(30).await.unwrap();
        if snapshot.processes.iter().any(|p| p.pid == waiter_sid && !p.blocked_by.is_empty()) || Instant::now() > deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let waiting = snapshot.processes.iter().find(|p| p.pid == waiter_sid).expect("waiter listed");
    assert_eq!(waiting.blocked_by, [holder_sid]);
    assert!(waiting.query.as_deref().unwrap_or_default().contains("UPDATE it_monitor"));
    assert!(snapshot.locks.iter().any(|l| !l.granted));
    assert!(snapshot.connections.unwrap().max > 0);
    assert!(monitor.kill(waiter_sid, false).await.unwrap());
    let err = blocked.await.unwrap().unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Cancelled | ErrorKind::Server), "{err:?}");
    holder.execute("ROLLBACK", &[], OPTS).await.unwrap();
    assert!(monitor.server_settings(Some("open_cursors")).await.unwrap().iter().any(|s| s.name == "open_cursors"));
    assert!(monitor.largest_tables(PDB, 5).await.unwrap().iter().any(|t| t.object.name == "ORDERS"));
    holder.execute("DROP TABLE it_monitor PURGE", &[], OPTS).await.unwrap();
}

#[tokio::test]
async fn schema_changes_and_diff() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, DefaultChange, SchemaChange};

    let cfg = require_db!();
    let s = session(&cfg).await;
    for t in ["IT_DIFF_INVOICES", "IT_DIFF_ACCOUNTS", "IT_DIFF_COPY"] {
        let _ = s.execute(&format!("DROP TABLE {t} CASCADE CONSTRAINTS PURGE"), &[], OPTS).await;
    }
    let col = |name: &str, ty: &str| ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, default: None, identity: false };
    let changes = vec![
        SchemaChange::CreateTable {
            table: table("IT_DIFF_ACCOUNTS"),
            columns: vec![ColumnDef { identity: true, nullable: false, ..col("ID", "NUMBER(19)") }, ColumnDef { default: Some("'x'".into()), ..col("EMAIL", "VARCHAR2(100)") }],
            primary_key: vec!["ID".into()],
        },
        SchemaChange::CreateTable {
            table: table("IT_DIFF_INVOICES"),
            columns: vec![ColumnDef { nullable: false, ..col("ID", "NUMBER(10)") }, col("ACCOUNT_ID", "NUMBER(19)"), col("AMOUNT", "NUMBER(12,2)")],
            primary_key: vec!["ID".into()],
        },
        SchemaChange::AddForeignKey {
            table: table("IT_DIFF_INVOICES"),
            name: "IT_DIFF_INVOICES_FK".into(),
            columns: vec!["ACCOUNT_ID".into()],
            target: table("IT_DIFF_ACCOUNTS"),
            target_columns: vec!["ID".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        },
        SchemaChange::AddColumn { table: table("IT_DIFF_INVOICES"), column: ColumnDef { default: Some("0".into()), nullable: false, ..col("QTY", "NUMBER(10)") } },
        SchemaChange::RenameColumn { table: table("IT_DIFF_INVOICES"), column: "AMOUNT".into(), new_name: "TOTAL".into() },
        SchemaChange::AlterColumn {
            table: table("IT_DIFF_ACCOUNTS"),
            column: "EMAIL".into(),
            data_type: Some("VARCHAR2(200)".into()),
            nullable: Some(false),
            default: Some(DefaultChange::Set { expression: "'y'".into() }),
        },
        SchemaChange::CreateIndex {
            table: table("IT_DIFF_INVOICES"),
            name: "IT_DIFF_INVOICES_QTY".into(),
            columns: vec![ddl::IndexColumn { name: "QTY".into(), descending: false }],
            unique: false,
        },
        SchemaChange::RenameTable { table: table("IT_DIFF_INVOICES"), new_name: "IT_DIFF_COPY".into() },
        SchemaChange::RenameTable { table: table("IT_DIFF_COPY"), new_name: "IT_DIFF_INVOICES".into() },
    ];
    for change in &changes {
        run_all(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    let email = ops::structure(&*s, &table("IT_DIFF_ACCOUNTS")).await.unwrap().column("EMAIL").cloned().unwrap();
    assert_eq!((email.data_type.as_str(), email.nullable, email.default.as_deref()), ("VARCHAR2(200)", false, Some("'y'")));
    let invoices = ops::structure(&*s, &table("IT_DIFF_INVOICES")).await.unwrap();
    assert!(invoices.column("TOTAL").is_some() && invoices.indexes.iter().any(|i| i.name == "IT_DIFF_INVOICES_QTY"));
    // A diff against an empty container plans every table, foreign keys last.
    let structures = vec![ops::structure(&*s, &table("IT_DIFF_ACCOUNTS")).await.unwrap(), invoices];
    let here = Container { database: PDB.into(), schema: Some(SCHEMA.into()) };
    let there = Container { database: PDB.into(), schema: Some("BILLING".into()) };
    let plan = diff(s.dialect(), &structures, &here, &[], &there).unwrap();
    assert!(plan.sql.contains("CREATE TABLE \"BILLING\".\"IT_DIFF_ACCOUNTS\""), "{}", plan.sql);
    run_all(&*s, "DROP TABLE it_diff_invoices CASCADE CONSTRAINTS PURGE; DROP TABLE it_diff_accounts CASCADE CONSTRAINTS PURGE").await;
}
