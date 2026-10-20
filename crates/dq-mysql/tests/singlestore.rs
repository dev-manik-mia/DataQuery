//! Integration tests against a real SingleStore server through the MySQL adapter.
//!
//! Start it with `npm run db:up:singlestore` (the `singlestoredb-dev` image,
//! root / `dataquery`) and run `npm run test:integration:singlestore`, i.e.
//! `DATAQUERY_TEST_SINGLESTORE=localhost:53308 cargo test -p dq-mysql --test singlestore`.
//! Without the variable every test is skipped. Each test works in its own
//! database `s2_<test>_<pid>`, dropped at the end.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::result::ExecOptions;
use dq_core::table::{RowFilter, TablePageRequest};
use dq_core::{DbResult, ErrorKind};
use dq_mysql::MysqlAdapter;

const PASSWORD: &str = "dataquery";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const ADAPTER: MysqlAdapter = MysqlAdapter::SINGLESTORE;

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_SINGLESTORE").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "3306"));
    Some(ConnConfig {
        engine: Engine::Singlestore,
        host: host.into(),
        port: port.parse().unwrap(),
        username: "root".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 10,
        ..ConnConfig::default()
    })
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_SINGLESTORE to run SingleStore integration tests");
                return;
            }
        }
    };
}

fn params(cfg: &ConnConfig) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some(PASSWORD.to_owned()))
}

/// A fresh database for one test, and a session connected to it.
async fn database(cfg: &ConnConfig, test: &str) -> (String, Arc<dyn Session>) {
    let db = format!("s2_{test}_{}", std::process::id());
    let admin = ADAPTER.connect(&params(cfg)).await.expect("connect");
    admin.execute(&format!("DROP DATABASE IF EXISTS {db}"), &[], OPTS).await.unwrap();
    admin.execute(&format!("CREATE DATABASE {db}"), &[], OPTS).await.unwrap();
    admin.close().await;
    let s = ADAPTER.connect(&params(&ConnConfig { database: db.clone(), ..cfg.clone() })).await.expect("connect");
    (db, s)
}

async fn drop_database(s: Arc<dyn Session>, db: &str) {
    s.execute(&format!("DROP DATABASE {db}"), &[], OPTS).await.unwrap();
    s.close().await;
}

async fn run_all(s: &dyn Session, statements: &[&str]) {
    for sql in statements {
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

/// Runs each statement of a generated script.
async fn run_script(s: &dyn Session, script: &str) {
    for statement in dq_core::sql::split_statements(script, dq_core::sql::SqlFlavor::Mysql) {
        let sql = statement.text(script);
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

async fn scalar(s: &dyn Session, sql: &str) -> String {
    s.execute(sql, &[], OPTS).await.unwrap().rows[0][0].to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn reports_singlestore_and_its_own_version() {
    let cfg = require_db!();
    let info = ADAPTER.test(&params(&cfg)).await.unwrap();
    assert!(info.version.starts_with("SingleStore "), "{}", info.version);
    assert!(info.version.chars().nth(12).is_some_and(|c| c.is_ascii_digit()), "{}", info.version);
    assert_eq!(info.engine, Engine::Singlestore);
    let err = ADAPTER.test(&ConnectParams::new(cfg, Some("wrong-Secret-1".into()))).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(!format!("{err:?}").contains("wrong-Secret-1"));
}

#[tokio::test]
async fn explorer_shows_storage_and_hides_system_databases() {
    let cfg = require_db!();
    let (db, s) = database(&cfg, "explorer").await;
    run_all(
        &*s,
        &[
            "CREATE ROWSTORE TABLE accounts (id BIGINT PRIMARY KEY, email VARCHAR(100) NOT NULL, UNIQUE KEY (id))",
            "CREATE TABLE events (id BIGINT NOT NULL, account_id BIGINT, at DATETIME(6), SORT KEY (at), SHARD KEY (account_id))",
            "CREATE REFERENCE TABLE countries (code CHAR(2) PRIMARY KEY, name VARCHAR(60))",
            "CREATE VIEW recent AS SELECT * FROM events WHERE at > '2020-01-01'",
        ],
    )
    .await;
    let databases = s.list_children(None).await.unwrap();
    let names: Vec<&str> = databases.iter().map(|n| n.object.name.as_str()).collect();
    assert!(names.contains(&db.as_str()), "{names:?}");
    assert!(!names.contains(&"memsql") && !names.contains(&"cluster"), "{names:?}");

    let database = ObjectRef::new(NodeKind::Database, db.clone(), None, db.clone());
    let relations = s.list_children(Some(&database)).await.unwrap();
    let comment = |name: &str| {
        let node = relations.iter().find(|n| n.object.name == name).unwrap_or_else(|| panic!("{name}"));
        node.comment.clone().unwrap_or_default()
    };
    assert!(comment("events").contains("columnstore"), "{relations:?}");
    assert!(comment("accounts").contains("rowstore"), "{relations:?}");
    let view = relations.iter().find(|n| n.object.name == "recent").unwrap();
    assert_eq!(view.object.kind, NodeKind::View);

    let events = ObjectRef::new(NodeKind::Table, db.clone(), None, "events");
    let children = s.list_children(Some(&events)).await.unwrap();
    assert!(children.iter().all(|n| n.object.kind == NodeKind::Index), "no triggers: {children:?}");
    let found = s.search("countr", 10).await.unwrap();
    assert!(found.iter().any(|n| n.object.name == "countries"), "{found:?}");
    drop_database(s, &db).await;
}

#[tokio::test]
async fn structure_ddl_and_no_foreign_keys() {
    let cfg = require_db!();
    let (db, s) = database(&cfg, "structure").await;
    run_all(
        &*s,
        &[
            "CREATE TABLE parents (id INT PRIMARY KEY, name TEXT)",
            "CREATE TABLE children (id INT, parent_id INT, note VARCHAR(20) DEFAULT 'x', SORT KEY (id), SHARD KEY (id))",
            "CREATE REFERENCE TABLE codes (code INT PRIMARY KEY)",
        ],
    )
    .await;
    let children = ObjectRef::new(NodeKind::Table, db.clone(), None, "children");
    let structure = s.structure(&children).await.unwrap();
    let names: Vec<&str> = structure.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "parent_id", "note"]);
    assert_eq!(structure.columns[2].default.as_deref(), Some("'x'"));
    assert_eq!(structure.foreign_keys, [], "SingleStore has no foreign keys");

    let ddl = s.ddl(&children).await.unwrap();
    assert!(ddl.contains("SHARD KEY") && ddl.contains("SORT KEY"), "{ddl}");
    let codes = s.ddl(&ObjectRef::new(NodeKind::Table, db.clone(), None, "codes")).await.unwrap();
    assert!(codes.contains("REFERENCE"), "{codes}");

    let deps = s.dependencies(&children).await.unwrap();
    assert!(deps.uses.is_empty() && deps.used_by.is_empty(), "{deps:?}");
    let model = s.schema_model(&db, None).await.unwrap();
    assert_eq!(model.tables.len(), 3);
    assert_eq!(model.foreign_keys, []);
    drop_database(s, &db).await;
}

#[tokio::test]
async fn queries_parameters_paging_and_edits() -> DbResult<()> {
    let Some(cfg) = config() else {
        eprintln!("skipped: set DATAQUERY_TEST_SINGLESTORE to run SingleStore integration tests");
        return Ok(());
    };
    let (db, s) = database(&cfg, "queries").await;
    run_all(
        &*s,
        &[
            "CREATE ROWSTORE TABLE items (id INT PRIMARY KEY, label VARCHAR(20), price DECIMAL(10,2))",
            "INSERT INTO items VALUES (1, 'a', 1.50), (2, 'b', NULL), (3, 'c', 3.00)",
        ],
    )
    .await;
    let r = s.execute("SELECT label FROM items WHERE id > ? ORDER BY id", &[Some("1".into())], OPTS).await?;
    assert_eq!(r.rows.len(), 2);
    assert!(s.execute("SELECT 1; SELECT 2", &[], OPTS).await.is_err(), "one statement per call");

    let paged = s.execute("SELECT id FROM items ORDER BY id", &[], ExecOptions { row_limit: 2, ..OPTS }).await?;
    assert!(paged.has_more);
    assert_eq!(s.fetch_more(10).await?.rows.len(), 1);

    let items = ObjectRef::new(NodeKind::Table, db.clone(), None, "items");
    assert_eq!(s.structure(&items).await?.row_identity, ["id"]);
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("2"))], values: vec![cv("price", Some("9.99"))] },
        RowChange::Insert { values: vec![cv("id", Some("4")), cv("label", Some("d"))] },
        RowChange::Delete { key: vec![cv("id", Some("1"))] },
    ];
    let report = ops::apply_changes(&*s, &items, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    assert_eq!(scalar(&*s, "SELECT price FROM items WHERE id = 2").await, "9.99");
    // A change that matches no row rolls the others back.
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("label", Some("x"))] },
        RowChange::Delete { key: vec![cv("id", Some("404"))] },
    ];
    assert_eq!(ops::apply_changes(&*s, &items, &failing, false).await.unwrap_err().kind, ErrorKind::Constraint);
    assert_eq!(scalar(&*s, "SELECT label FROM items WHERE id = 3").await, "c");

    let request = TablePageRequest { object: items, rows: RowFilter::default(), sort: None, offset: 0, limit: 10 };
    assert_eq!(ops::table_page(&*s, &request).await?.rows.len(), 3);
    drop_database(s, &db).await;
    Ok(())
}

#[tokio::test]
async fn explain_and_profile() {
    let cfg = require_db!();
    let (db, s) = database(&cfg, "explain").await;
    run_all(&*s, &["CREATE TABLE t (id INT, v INT, SORT KEY (id))", "INSERT INTO t VALUES (1, 1), (2, 2)"]).await;
    let plan = s.explain("SELECT v FROM t WHERE id = 1", &[], false, 30).await.unwrap();
    assert!(plan.plan.is_some() && !plan.raw.is_empty(), "{}", plan.raw);
    let profiled = s.explain("SELECT v FROM t WHERE id = 1", &[], true, 30).await.unwrap();
    assert!(profiled.analyzed && profiled.plan.is_some(), "{}", profiled.raw);
    drop_database(s, &db).await;
}

#[tokio::test]
async fn cancel_monitor_and_kill() {
    let cfg = require_db!();
    let s = ADAPTER.connect(&params(&cfg)).await.unwrap();
    let runner = s.clone();
    let started = Instant::now();
    let query = tokio::spawn(async move { runner.execute("SELECT SLEEP(30)", &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(800)).await;

    let monitor = s.monitor().expect("monitoring");
    let snapshot = monitor.snapshot(0).await.unwrap();
    assert!(snapshot.version.starts_with("SingleStore"), "{}", snapshot.version);
    let sleeping = snapshot.processes.iter().find(|p| p.query.as_deref().is_some_and(|q| q.contains("SLEEP(30)")));
    assert!(sleeping.is_some_and(|p| p.active), "{:?}", snapshot.processes);
    assert!(snapshot.unavailable.iter().any(|u| u.contains("row locks")), "{:?}", snapshot.unavailable);

    assert!(s.cancel().await.unwrap());
    let _ = query.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(20), "cancelled, not finished");
    assert!(!monitor.kill(i64::from(u32::MAX), false).await.unwrap(), "unknown ids report false");
    assert!(!monitor.server_settings(Some("max_connections")).await.unwrap().is_empty());
    assert!(s.users().is_none(), "account management is not offered");
    s.close().await;
}

#[tokio::test]
async fn statement_timeouts_are_enforced_by_the_session() {
    let cfg = require_db!();
    let s = ADAPTER.connect(&params(&cfg)).await.unwrap();
    let started = Instant::now();
    let err = s.execute("SELECT SLEEP(20)", &[], ExecOptions { timeout_secs: 1, ..OPTS }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    // The connection stays usable.
    assert_eq!(scalar(&*s, "SELECT 1").await, "1");
    s.close().await;
}

#[tokio::test]
async fn read_only_profiles_and_catalog_reads_never_write() {
    let cfg = require_db!();
    let (db, s) = database(&cfg, "readonly").await;
    run_all(&*s, &["CREATE TABLE t (id INT PRIMARY KEY)", "INSERT INTO t VALUES (1)"]).await;
    // SingleStore accepts `SET SESSION TRANSACTION READ ONLY` but still writes; the session refuses.
    let err = s.read_query(&db, "DELETE FROM t", &[], dq_core::value::CellLimits::GRID).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");

    let ro = ConnConfig { database: db.clone(), read_only: true, ..cfg.clone() };
    let reader = ADAPTER.connect(&params(&ro)).await.unwrap();
    assert!(reader.info().read_only);
    assert_eq!(scalar(&*reader, "SELECT COUNT(*) FROM t").await, "1");
    for sql in ["DELETE FROM t", "INSERT INTO t VALUES (2)", "DROP TABLE t"] {
        let err = reader.execute(sql, &[], OPTS).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission, "{sql}: {err:?}");
    }
    let t = ObjectRef::new(NodeKind::Table, db.clone(), None, "t");
    let delete = vec![RowChange::Delete { key: vec![ColumnValue { column: "id".into(), value: Some("1".into()) }] }];
    assert_eq!(ops::apply_changes(&*reader, &t, &delete, false).await.unwrap_err().kind, ErrorKind::Permission);
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM t").await, "1");
    reader.close().await;
    drop_database(s, &db).await;
}

#[tokio::test]
async fn routines_and_index_kinds() {
    let cfg = require_db!();
    let (db, s) = database(&cfg, "routines").await;
    run_all(
        &*s,
        &[
            "CREATE PROCEDURE hello() AS BEGIN ECHO SELECT 1 AS x; END",
            "CREATE FUNCTION twice(a INT) RETURNS INT AS BEGIN RETURN a * 2; END",
            "CREATE TABLE c (id INT, at DATETIME, n INT, SORT KEY (at), SHARD KEY (id), KEY (n) USING HASH)",
        ],
    )
    .await;
    let database = ObjectRef::new(NodeKind::Database, db.clone(), None, db.clone());
    let groups: Vec<_> = s
        .list_children(Some(&database))
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.object.kind == NodeKind::Group)
        .collect();
    assert_eq!(groups.len(), 2, "functions and procedures: {groups:?}");
    for group in &groups {
        let routines = s.list_children(Some(&group.object)).await.unwrap();
        assert_eq!(routines.len(), 1, "{routines:?}");
        let ddl = s.ddl(&routines[0].object).await.unwrap();
        assert!(ddl.starts_with("CREATE OR REPLACE"), "{ddl}");
    }

    let c = s.structure(&ObjectRef::new(NodeKind::Table, db.clone(), None, "c")).await.unwrap();
    let definitions: Vec<&str> = c.indexes.iter().map(|i| i.definition.as_str()).collect();
    assert_eq!(definitions.len(), 2, "the shard key is not an index: {definitions:?}");
    assert!(definitions.contains(&"SORT KEY at (at)"), "{definitions:?}");
    assert!(definitions.iter().any(|d| d.ends_with("(n) USING HASH")), "{definitions:?}");
    drop_database(s, &db).await;
}

#[tokio::test]
async fn schema_changes_and_diff_round_trip() {
    use dq_core::diff::{Container, diff};
    use dq_core::sql::ddl::{self, ColumnDef, SchemaChange};

    let cfg = require_db!();
    let (a, s) = database(&cfg, "diff_a").await;
    let (b, other) = database(&cfg, "diff_b").await;
    other.close().await;
    let col = |name: &str, ty: &str| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        default: None,
        identity: false,
    };
    let accounts = ObjectRef::new(NodeKind::Table, a.clone(), None, "accounts");
    let create = SchemaChange::CreateTable {
        table: accounts.clone(),
        columns: vec![
            ColumnDef { identity: true, nullable: false, ..col("id", "bigint") },
            ColumnDef { default: Some("'x'".into()), ..col("email", "varchar(100)") },
        ],
        primary_key: vec!["id".into()],
    };
    let add = SchemaChange::AddColumn { table: accounts.clone(), column: col("note", "text") };
    for change in [&create, &add] {
        run_script(&*s, &ddl::preview(&ddl::statements(s.dialect(), change, None).unwrap())).await;
    }
    let structure = ops::structure(&*s, &accounts).await.unwrap();
    assert!(structure.column("id").unwrap().identity);
    assert_eq!(structure.column("email").unwrap().default.as_deref(), Some("'x'"));

    let snapshot = |db: String| {
        let s = s.clone();
        async move {
            let mut tables = Vec::new();
            for table in s.schema_model(&db, None).await.unwrap().tables {
                tables.push(ops::structure(&*s, &table.object).await.unwrap().into());
            }
            tables
        }
    };
    let from = Container { database: a.clone(), schema: None };
    let to = Container { database: b.clone(), schema: None };
    let plan = diff(s.dialect(), &snapshot(a.clone()).await, &from, &snapshot(b.clone()).await, &to).unwrap();
    run_script(&*s, &plan.sql).await;
    let again = diff(s.dialect(), &snapshot(a.clone()).await, &from, &snapshot(b.clone()).await, &to).unwrap();
    assert!(again.items.is_empty(), "{:#?}\n{:?}", again.items, again.sql);
    s.execute(&format!("DROP DATABASE {b}"), &[], OPTS).await.unwrap();
    drop_database(s, &a).await;
}

#[tokio::test]
async fn backup_and_restore_with_mysqldump() {
    use dq_core::backup::{self, BackupFormat, BackupOptions, RestoreOptions, Tool, ToolOverrides};
    use dq_core::transfer::TransferControl;

    let cfg = require_db!();
    let (db, s) = database(&cfg, "backup").await;
    let p = params(&ConnConfig { database: db.clone(), ..cfg.clone() });
    let version = s.info().version.clone();
    for tool in [Tool::Mysqldump, Tool::Mysql] {
        let found =
            backup::tools::locate(tool, Engine::Singlestore, backup::ServerVersion::parse(&version), None).await;
        if found.found.is_none() {
            eprintln!("skipped: {} is not installed (brew install mysql-client@8.4)", tool.id());
            return drop_database(s, &db).await;
        }
    }
    run_all(
        &*s,
        &[
            "CREATE ROWSTORE TABLE accounts (id INT PRIMARY KEY, email VARCHAR(100) NOT NULL)",
            "CREATE TABLE events (id INT, account_id INT, note TEXT, SORT KEY (id), SHARD KEY (account_id))",
            "INSERT INTO accounts VALUES (1, 'a@x'), (2, 'b@x')",
            "INSERT INTO events VALUES (1, 1, 'Zoë ✓'), (2, 2, NULL), (3, 2, 'it''s')",
            "CREATE PROCEDURE hello() AS BEGIN ECHO SELECT 1 AS x; END",
        ],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s2.sql");
    let none = ToolOverrides::new();
    let options =
        BackupOptions { database: db.clone(), format: BackupFormat::Plain, clean: true, ..BackupOptions::default() };
    let report = backup::backup(&p, &version, None, &options, &path, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(report.objects, 2);
    let dump = std::fs::read_to_string(&path).unwrap();
    assert!(dump.contains("SHARD KEY") && dump.contains("SORT KEY"), "{dump}");

    run_all(&*s, &["DROP TABLE events", "DROP TABLE accounts", "DROP PROCEDURE hello"]).await;
    let restore = RestoreOptions { database: db.clone(), ..RestoreOptions::default() };
    backup::restore(&p, &version, None, &restore, &path, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM events").await, "3");
    assert_eq!(scalar(&*s, "SELECT note FROM events WHERE id = 1").await, "Zoë ✓");
    assert_eq!(scalar(&*s, "SELECT email FROM accounts WHERE id = 2").await, "b@x");
    let routines = format!("SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{db}'");
    assert_eq!(scalar(&*s, &routines).await, "1", "procedures come back too");
    drop_database(s, &db).await;
}
